//! The swap's leg on an EVM chain (Base), through elastOS Wallet's app bridge: the capsule
//! builds the calldata and checks what it reads from the chain; Wallet holds the key, shows
//! each transaction and asks the user to approve it (passkey, MetaMask, as set up there).
//!
//! Nothing here trusts a message: a counterparty's lock is read from the chain itself, in a
//! contract recognized by its code hash, and checked against the agreed terms and the
//! Neptune lock's refund time before anything is locked or revealed.

// The swap flow that drives these steps comes next (PLAN.md, "EVM leg"); until then only
// the Wallet connection uses this module.
#![allow(dead_code)]

use evm_htlc::{decode_lock, hex, parse_address, parse_hex, parse_word, Address, LockView, Word};
use intents_protocol::{asset_row, may_reveal, Chain};
use serde_json::{json, Value};

use crate::runtime;

/// The Wallet bridge's name for Base, the EVM chain elastOS Wallet pays on.
pub const BASE: &str = "eip155:8453";

/// The bridge's chain namespace for `chain`, if Wallet can pay there.
pub fn namespace(chain: Chain) -> Option<&'static str> {
    match chain {
        Chain::Base => Some(BASE),
        _ => None,
    }
}

/// The account elastOS Wallet disclosed for this page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Account {
    pub id: String,
    pub address: Address,
}

/// One transaction for Wallet to show and the user to approve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tx {
    pub to: Address,
    /// Native coin sent with the call, in wei.
    pub value: u128,
    pub data: Vec<u8>,
    /// What the user is approving, in words.
    pub what: String,
}

/// The token contract of `symbol` on `chain`: None for the chain's native coin.
pub fn token_of(chain: Chain, symbol: &str) -> Result<Option<Address>, String> {
    let (_, token) = asset_row(chain, symbol).ok_or(format!("{symbol} is not listed on this chain"))?;
    token.map(|t| parse_address(t).ok_or("bad token address".to_string())).transpose()
}

/// The transactions that lock `amount` for `receiver`: one call with the coin as value, or
/// an exact approval of the token followed by the lock.
pub fn lock_txs(contract: &Address, token: Option<Address>, amount: u128, receiver: &Address, hash_lock: &Word, refund_after: u64, label: &str) -> Vec<Tx> {
    match token {
        None => vec![Tx { to: *contract, value: amount, data: evm_htlc::lock(receiver, hash_lock, refund_after), what: format!("Lock {label} for the swap") }],
        Some(token) => vec![
            Tx { to: token, value: 0, data: evm_htlc::approve(contract, amount), what: format!("Allow the swap contract to take exactly {label}") },
            Tx { to: *contract, value: 0, data: evm_htlc::lock_token(&token, amount, receiver, hash_lock, refund_after), what: format!("Lock {label} for the swap") },
        ],
    }
}

pub fn claim_tx(contract: &Address, id: &Word, secret: &Word, label: &str) -> Tx {
    Tx { to: *contract, value: 0, data: evm_htlc::claim(id, secret), what: format!("Claim {label}") }
}

pub fn refund_tx(contract: &Address, id: &Word, label: &str) -> Tx {
    Tx { to: *contract, value: 0, data: evm_htlc::refund(id), what: format!("Take back {label}") }
}

/// What a counterparty's lock must be, from the agreed terms.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Expected {
    pub sender: Address,
    pub receiver: Address,
    /// Zero for the native coin.
    pub token: Address,
    pub amount: u128,
    pub hash_lock: Word,
}

/// Checks a lock read from the chain: the terms, unspent, a refund that comes the chain's gap
/// before the Neptune refund (`neptune_refund_at`, read from the Neptune lock script), and
/// still time to claim it safely. Errors are sentences for the person.
pub fn check_lock(view: &LockView, want: &Expected, chain: Chain, neptune_refund_at: u64, now: u64) -> Result<(), String> {
    let checks = [
        (view.sender == want.sender, "The lock is not from the solver that quoted."),
        (view.receiver == want.receiver, "The lock is not for your Wallet account."),
        (view.token == want.token, "The lock holds another token."),
        (view.amount == want.amount, "The lock holds another amount than agreed."),
        (view.hash_lock == want.hash_lock, "The lock is under another secret."),
        (!view.spent, "The lock is already spent."),
        (view.refund_after.saturating_add(chain.gap_secs()) <= neptune_refund_at, "The lock refunds too late for the Neptune side."),
        (may_reveal(chain, view.refund_after, now), "Too little time is left to claim safely."),
    ];
    match checks.iter().find(|(ok, _)| !ok) {
        Some((_, why)) => Err(why.to_string()),
        None => Ok(()),
    }
}

/// A receipt's outcome: None while it is not mined, then whether it succeeded.
pub fn receipt_ok(receipt: &Value) -> Option<bool> {
    match receipt["status"].as_str()? {
        "0x1" => Some(true),
        _ => Some(false),
    }
}

/// The secret a `Claimed` log in `logs` (as `eth_getLogs` returns them) published for `id`,
/// only if it opens `hash_lock`.
pub fn secret_from_logs(logs: &Value, contract: &Address, id: &Word, hash_lock: &Word) -> Option<Word> {
    logs.as_array()?.iter().find_map(|log| {
        let address = parse_address(log["address"].as_str()?)?;
        let topics: Vec<Word> = log["topics"].as_array()?.iter().map(|t| parse_word(t.as_str()?)).collect::<Option<_>>()?;
        let data = parse_hex(log["data"].as_str()?)?;
        evm_htlc::claimed_secret(&evm_htlc::Log { address: &address, topics: &topics, data: &data }, contract, id, hash_lock)
    })
}

// --- through the Wallet bridge ------------------------------------------------

const BRIDGE: &str = "/api/apps/browser/wallet";

/// Percent-encodes a query value.
fn escape(text: &str) -> String {
    text.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Waits for the user's answer to a Wallet approval (up to ten minutes) and returns the
/// completed status.
pub async fn wait_approval(request_id: &str, token: &str) -> Result<Value, String> {
    let (page_url, origin) = runtime::page_and_origin();
    let path = format!("{BRIDGE}/approvals/{request_id}?page_url={}&origin={}", escape(&page_url), escape(&origin));
    for _ in 0..300 {
        runtime::sleep_ms(2_000).await;
        match runtime::gateway("GET", &path, None, token).await {
            Ok(status) if status["status"] == "completed" => return Ok(status),
            Ok(status) if matches!(status["status"].as_str(), Some("pending" | "approved")) => continue,
            Ok(status) => return Err(format!("elastOS Wallet: the request was {}.", status["status"].as_str().unwrap_or("not approved"))),
            Err(e) => return Err(format!("elastOS Wallet: {}", e.message)),
        }
    }
    Err("elastOS Wallet: no answer within ten minutes.".into())
}

/// A typed chain read through the bridge (the Runtime's chain provider answers it).
pub async fn read(namespace: &str, method: &str, params: Value, token: &str) -> Result<Value, String> {
    let (page_url, origin) = runtime::page_and_origin();
    let body = json!({ "method": method, "params": params, "chain_namespace": namespace, "page_url": page_url, "origin": origin });
    let reply = runtime::gateway("POST", &format!("{BRIDGE}/read"), Some(&body), token).await.map_err(|e| format!("Chain read: {}", e.message))?;
    Ok(reply["result"].clone())
}

/// Checks that `contract` holds the swap contract's code, whoever deployed it.
pub async fn check_contract(namespace: &str, contract: &Address, token: &str) -> Result<(), String> {
    let code = read(namespace, "eth_getCode", json!([hex(contract), "latest"]), token).await?;
    let code = code.as_str().and_then(parse_hex).ok_or("Chain read: no code")?;
    if evm_htlc::is_htlc_code(&code) {
        Ok(())
    } else {
        Err("That address is not the swap contract.".into())
    }
}

/// A lock as the contract holds it.
pub async fn lock_view(namespace: &str, contract: &Address, id: &Word, token: &str) -> Result<LockView, String> {
    let ret = read(namespace, "eth_call", json!([{ "to": hex(contract), "data": hex(&evm_htlc::locks(id)) }, "latest"]), token).await?;
    ret.as_str().and_then(parse_hex).as_deref().and_then(decode_lock).ok_or("The contract gave no lock.".into())
}

/// The secret a claim of `id` published, if it has been claimed.
pub async fn claimed_secret(namespace: &str, contract: &Address, id: &Word, hash_lock: &Word, token: &str) -> Result<Option<Word>, String> {
    let topic = hex(&evm_htlc::keccak(b"Claimed(bytes32,bytes32)"));
    let filter = json!({ "address": hex(contract), "topics": [topic, hex(id)], "fromBlock": "earliest", "toBlock": "latest" });
    let logs = read(namespace, "eth_getLogs", json!([filter]), token).await?;
    Ok(secret_from_logs(&logs, contract, id, hash_lock))
}

/// Sends `tx` from `account`: Wallet shows it and the user approves, then it is broadcast
/// and its receipt awaited. Returns the transaction hash. `waiting` gets a line for the
/// screen at each stage.
pub async fn send(namespace: &str, account: &Account, tx: &Tx, token: &str, waiting: &dyn Fn(String)) -> Result<String, String> {
    let (page_url, origin) = runtime::page_and_origin();
    let from = hex(&account.address);
    let body = json!({
        "method": "eth_sendTransaction",
        "params": [{ "from": from, "to": hex(&tx.to), "value": format!("0x{:x}", tx.value), "data": hex(&tx.data) }],
        "account_id": account.id,
        "chain_namespace": namespace,
        "address": from,
        "page_url": page_url,
        "origin": origin,
    });
    waiting(format!("{}: approve it in the Wallet app…", tx.what));
    let asked = runtime::gateway("POST", &format!("{BRIDGE}/request-transaction"), Some(&body), token)
        .await
        .map_err(|e| format!("elastOS Wallet: {}", e.message))?;
    let request_id = asked["approval_request"]["request_id"].as_str().unwrap_or_default().to_string();
    if request_id.is_empty() {
        return Err("elastOS Wallet did not open an approval.".into());
    }
    wait_approval(&request_id, token).await?;
    waiting(format!("{}: sending…", tx.what));
    let sent = runtime::gateway("POST", &format!("{BRIDGE}/broadcast-transaction"), Some(&json!({ "request_id": request_id })), token)
        .await
        .map_err(|e| format!("elastOS Wallet: {}", e.message))?;
    let hash = sent["transaction_hash"].as_str().filter(|h| parse_word(h).is_some()).ok_or("elastOS Wallet sent no transaction hash.")?.to_string();
    waiting(format!("{}: waiting for the chain…", tx.what));
    for _ in 0..180 {
        let receipt = read(namespace, "eth_getTransactionReceipt", json!([hash]), token).await.unwrap_or(Value::Null);
        match receipt_ok(&receipt) {
            Some(true) => return Ok(hash),
            Some(false) => return Err(format!("{}: the chain refused it (reverted).", tx.what)),
            None => runtime::sleep_ms(3_000).await,
        }
    }
    Err(format!("{}: not mined within nine minutes; it may still land.", tx.what))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONTRACT: Address = [0xcc; 20];
    const SOLVER: Address = [0x50; 20];
    const USER: Address = [0x05; 20];

    fn view() -> LockView {
        LockView { sender: SOLVER, receiver: USER, token: [0; 20], amount: 10u128.pow(18), hash_lock: [7; 32], refund_after: 100_000, spent: false }
    }

    fn expected() -> Expected {
        Expected { sender: SOLVER, receiver: USER, token: [0; 20], amount: 10u128.pow(18), hash_lock: [7; 32] }
    }

    #[test]
    fn a_good_lock_passes_and_every_wrong_term_is_named() {
        let neptune_refund = 100_000 + Chain::Base.gap_secs();
        let now = 100_000 - Chain::Base.claim_margin_secs();
        assert_eq!(check_lock(&view(), &expected(), Chain::Base, neptune_refund, now), Ok(()));
        let wrong = [
            (LockView { sender: USER, ..view() }, "not from the solver"),
            (LockView { receiver: SOLVER, ..view() }, "not for your Wallet"),
            (LockView { token: [1; 20], ..view() }, "another token"),
            (LockView { amount: 1, ..view() }, "another amount"),
            (LockView { hash_lock: [8; 32], ..view() }, "another secret"),
            (LockView { spent: true, ..view() }, "already spent"),
            (LockView { refund_after: 100_001, ..view() }, "refunds too late"),
        ];
        for (v, why) in wrong {
            let err = check_lock(&v, &expected(), Chain::Base, neptune_refund, now).unwrap_err();
            assert!(err.contains(why), "{err}");
        }
        assert!(check_lock(&view(), &expected(), Chain::Base, neptune_refund, now + 1).unwrap_err().contains("Too little time"));
    }

    #[test]
    fn eth_locks_in_one_call_and_usdc_approves_exactly_first() {
        assert_eq!(token_of(Chain::Base, "ETH"), Ok(None));
        let usdc = token_of(Chain::Base, "USDC").unwrap().unwrap();
        assert_eq!(hex(&usdc), "0x833589fcd6edb6e08f4c7c32d4f71b54bda02913");
        assert!(token_of(Chain::Base, "ELA").is_err());
        let eth = lock_txs(&CONTRACT, None, 5, &SOLVER, &[7; 32], 9, "0.5 ETH");
        assert_eq!(eth.len(), 1);
        assert_eq!((eth[0].to, eth[0].value), (CONTRACT, 5));
        let usdc_txs = lock_txs(&CONTRACT, Some(usdc), 250_000_000, &SOLVER, &[7; 32], 9, "250 USDC");
        assert_eq!(usdc_txs.len(), 2);
        assert_eq!((usdc_txs[0].to, usdc_txs[0].value), (usdc, 0));
        assert_eq!(usdc_txs[0].data, evm_htlc::approve(&CONTRACT, 250_000_000));
        assert_eq!(usdc_txs[1].to, CONTRACT);
        assert!(usdc_txs[1].what.contains("250 USDC"));
    }

    #[test]
    fn the_secret_comes_only_from_the_right_claimed_log() {
        let secret = [3u8; 32];
        let lock = evm_htlc::hash_lock(&secret);
        let id = [9u8; 32];
        let log = |address: &Address, id: &Word, data: &Word| {
            json!({ "address": hex(address), "topics": [hex(&evm_htlc::keccak(b"Claimed(bytes32,bytes32)")), hex(id)], "data": hex(data) })
        };
        let logs = json!([log(&[0xdd; 20], &id, &secret), log(&CONTRACT, &[8; 32], &secret), log(&CONTRACT, &id, &secret)]);
        assert_eq!(secret_from_logs(&logs, &CONTRACT, &id, &lock), Some(secret));
        assert_eq!(secret_from_logs(&json!([log(&CONTRACT, &id, &[4; 32])]), &CONTRACT, &id, &lock), None, "does not open the lock");
        assert_eq!(secret_from_logs(&json!({}), &CONTRACT, &id, &lock), None);
    }

    #[test]
    fn receipts_read_as_pending_success_or_revert() {
        assert_eq!(receipt_ok(&Value::Null), None);
        assert_eq!(receipt_ok(&json!({"status": "0x1"})), Some(true));
        assert_eq!(receipt_ok(&json!({"status": "0x0"})), Some(false));
    }
}
