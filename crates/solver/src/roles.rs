//! The two sides of an NPT → ETH swap (ETH on Base; a local chain in tests). The user gives NPT, so the user holds
//! the secret (the protocol's rule: the NPT giver holds it): it locks NPT
//! first, with the longer timeout, then claims the solver's ETH (which makes
//! the secret public on the EVM chain), and the solver claims the NPT last.
//!
//! Every step checks what the protocol requires before anything moves:
//! signatures and terms (`intents-protocol`), the NPT lock against the agreed
//! terms and on the solver's own node, the ETH lock on chain, and the timeouts
//! (`other_refund_at`, `may_reveal`).

use anyhow::{ensure, Context, Result};
use ed25519_dalek::SigningKey;
use evm_htlc::{decode_lock, hex, lock_id, parse_address, parse_hex, parse_word, Address, Word};
use evm_signer::{Node, Wallet};
use intents_protocol::{decode, may_reveal, other_refund_at, Accept, Amount, Asset, Chain, Decoded, EvmLocked, Intent, Net, NptLocked, Quote, Signed};
use light_core::swap::HtlcNote;
use serde_json::json;

use crate::neptune::{npt_text, Provider};

pub const NET: Net = Net::Test;
const PROTOCOL_VERSION: u16 = intents_protocol::PROTOCOL_VERSION;
/// NPT fee per Neptune transaction in these swaps.
pub const NPT_FEE: &str = "0.05";
/// Smallest NPT unit per coin.
const NAU_PER_NPT: u128 = 4 * 10u128.pow(30);

pub struct EvmChain {
    pub node: Node,
    pub contract: Address,
}

impl EvmChain {
    fn lock_view(&self, id: &Word) -> Result<evm_htlc::LockView> {
        let ret = self.node.call("eth_call", json!([{ "to": hex(&self.contract), "data": hex(&evm_htlc::locks(id)) }, "latest"]))?;
        decode_lock(&parse_hex(ret.as_str().context("eth_call")?).context("hex")?).context("not a lock")
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn eth_key_hex(a: &Address) -> String {
    hex(a)[2..].to_string()
}

// ---------------------------------------------------------------------------

pub struct User {
    pub did: SigningKey,
    pub neptune: Provider,
    pub eth: Wallet,
    intent: Option<Decoded<Intent>>,
    quote: Option<Decoded<Quote>>,
    accept: Option<Decoded<Accept>>,
    swap_id: Option<String>,
    hash_lock: Option<[u8; 32]>,
    neptune_refund_ms: u64,
}

impl User {
    /// A fresh one-time DID per intent, so solvers cannot link a user's trades.
    pub fn new(neptune: Provider, eth: Wallet) -> Self {
        Self { did: SigningKey::from_bytes(&rand::random()), neptune, eth, intent: None, quote: None, accept: None, swap_id: None, hash_lock: None, neptune_refund_ms: 0 }
    }

    /// Give `npt_units` NPT for at least `min_wei` ETH.
    pub fn intent(&mut self, npt_units: u128, min_wei: u128) -> Result<Vec<u8>> {
        let id: [u8; 16] = rand::random();
        let intent = Intent {
            v: PROTOCOL_VERSION,
            id: hex(&id)[2..].to_string(),
            give: Asset::npt(),
            give_amount: Amount::new(npt_units),
            want: Asset { chain: Chain::Base, symbol: "ETH".into() },
            min_want_amount: Amount::new(min_wei),
            expires_at: now_secs() + 600,
        };
        let bytes = Signed::sign(&intent, &self.did, NET)?.encode()?;
        self.intent = Some(decode(&bytes, now_secs(), NET)?);
        Ok(bytes)
    }

    /// Checks the quote and answers with an Accept carrying the hash lock.
    pub fn accept(&mut self, quote_bytes: &[u8]) -> Result<Vec<u8>> {
        let intent = self.intent.as_ref().context("no intent")?;
        let quote: Decoded<Quote> = decode(quote_bytes, now_secs(), NET)?;
        quote.body.answers(intent)?;
        let prepared = self.neptune.call("htlc_prepare", json!({}))?;
        let hash_lock: [u8; 32] = parse_word(prepared["hash_lock"].as_str().context("hash lock")?).context("hash lock")?;
        let refund = self.neptune.call("htlc_claim_lock", json!({}))?;
        let accept = Accept {
            v: PROTOCOL_VERSION,
            quote: quote.digest.clone(),
            hash_lock: Some(hex(&hash_lock)[2..].to_string()),
            claim_key: eth_key_hex(&self.eth.address),
            refund_key: refund["claim_lock"].as_str().context("refund key")?.to_string(),
        };
        let bytes = Signed::sign(&accept, &self.did, NET)?.encode()?;
        let decoded: Decoded<Accept> = decode(&bytes, now_secs(), NET)?;
        decoded.body.answers(&decoded.signer, intent, &quote, now_secs())?;
        self.accept = Some(decoded);
        self.swap_id = Some(prepared["swap_id"].as_str().context("swap id")?.to_string());
        self.hash_lock = Some(hash_lock);
        self.quote = Some(quote);
        Ok(bytes)
    }

    /// Locks the NPT for the solver, refundable after the quote's Neptune lock time, and
    /// returns the signed `npt_locked` message carrying the note.
    pub fn lock_npt(&mut self) -> Result<Vec<u8>> {
        let (intent, quote) = (self.intent.as_ref().context("no intent")?, self.quote.as_ref().context("no quote")?);
        let from_height = self.neptune.tip()?;
        self.neptune_refund_ms = (now_secs() + quote.body.neptune_lock_secs) * 1000;
        let locked = self.neptune.call(
            "htlc_lock",
            json!({
                "amount": npt_text(intent.body.give_amount.units()),
                "fee": NPT_FEE,
                "claim_lock": quote.body.claim_key,
                "refund_after_ms": self.neptune_refund_ms,
                "swap_id": self.swap_id,
            }),
        )?;
        let message = NptLocked {
            v: PROTOCOL_VERSION,
            deal: self.accept.as_ref().context("no accept")?.digest.clone(),
            note: locked["note"].clone(),
            from_height,
        };
        Ok(Signed::sign(&message, &self.did, NET)?.encode()?)
    }

    /// Checks the solver's ETH lock on chain, then claims it, which publishes the secret.
    pub fn claim_eth(&self, evm: &EvmChain, evm_locked: &[u8]) -> Result<String> {
        let (quote, accept) = (self.quote.as_ref().context("no quote")?, self.accept.as_ref().context("no accept")?);
        let locked: Decoded<EvmLocked> = decode(evm_locked, now_secs(), NET)?;
        locked.body.answers(&locked.signer, accept, quote)?;
        let contract = parse_address(&locked.body.contract).context("contract")?;
        ensure!(contract == evm.contract, "the ETH lock is in another contract");
        let code = evm.node.call("eth_getCode", json!([hex(&contract), "latest"]))?;
        ensure!(evm_htlc::is_htlc_code(&parse_hex(code.as_str().context("eth_getCode")?).context("hex")?), "not the NeptuneHtlc contract");
        let id = parse_word(&locked.body.id).context("id")?;
        let lock = evm.lock_view(&id)?;
        let solver = parse_address(&quote.body.refund_key).context("solver key")?;
        ensure!(lock.sender == solver, "not locked by the solver that quoted");
        ensure!(lock.receiver == self.eth.address, "not locked for this user");
        ensure!(lock.token == [0; 20], "not ETH");
        ensure!(lock.amount == quote.body.want_amount.units(), "not the quoted amount");
        ensure!(Some(lock.hash_lock) == self.hash_lock, "not under this swap's hash lock");
        ensure!(!lock.spent, "already spent");
        // The ETH refund must come well before the NPT refund, and there must be
        // time to claim before it.
        let neptune_refund = self.neptune_refund_ms / 1000;
        ensure!(lock.refund_after.saturating_add(Chain::Base.gap_secs()) <= neptune_refund, "the ETH lock refunds too late");
        let evm_now = evm.node.now()?;
        ensure!(may_reveal(Chain::Base, lock.refund_after, evm_now), "too late to reveal the secret safely");
        let secret = self.neptune.call("htlc_secret", json!({ "swap_id": self.swap_id }))?;
        let secret = parse_word(secret["secret"].as_str().context("secret")?).context("secret")?;
        let receipt = evm.node.send(&self.eth, &evm.contract, 0, evm_htlc::claim(&id, &secret))?;
        Ok(receipt.hash)
    }
}

// ---------------------------------------------------------------------------

/// A solver for NPT → ETH: fixed price, its own DID, Neptune wallet and EVM key.
pub struct Solver {
    pub did: SigningKey,
    pub neptune: Provider,
    pub eth: Wallet,
    /// Wei per whole NPT.
    pub wei_per_npt: u128,
    intent: Option<Decoded<Intent>>,
    quote: Option<Decoded<Quote>>,
    accept: Option<Decoded<Accept>>,
    claim_index: u64,
    /// The checked NPT lock: its note and where to search from.
    npt: Option<(HtlcNote, u64)>,
    eth_lock: Option<Word>,
}

impl Solver {
    pub fn new(did: SigningKey, neptune: Provider, eth: Wallet, wei_per_npt: u128) -> Self {
        Self { did, neptune, eth, wei_per_npt, intent: None, quote: None, accept: None, claim_index: 0, npt: None, eth_lock: None }
    }

    pub fn quote(&mut self, intent_bytes: &[u8]) -> Result<Vec<u8>> {
        let intent: Decoded<Intent> = decode(intent_bytes, now_secs(), NET)?;
        let i = &intent.body;
        ensure!(i.give.is_npt() && i.want == Asset { chain: Chain::Base, symbol: "ETH".into() }, "this solver buys NPT for ETH only");
        let want = mul_div(i.give_amount.units(), self.wei_per_npt, NAU_PER_NPT).context("the price overflows")?;
        ensure!(want >= i.min_want_amount.units(), "the price is below the intent's minimum");
        let claim = self.neptune.call("htlc_claim_lock", json!({}))?;
        self.claim_index = claim["index"].as_u64().context("claim index")?;
        let other = Chain::Base.min_lock_secs();
        let quote = Quote {
            v: PROTOCOL_VERSION,
            intent: intent.digest.clone(),
            want_amount: Amount::new(want),
            other_lock_secs: other,
            neptune_lock_secs: other + Chain::Base.gap_secs() + intents_protocol::NEPTUNE_SETTLE_SECS,
            hash_lock: None,
            claim_key: claim["claim_lock"].as_str().context("claim lock")?.to_string(),
            refund_key: eth_key_hex(&self.eth.address),
            expires_at: now_secs() + 300,
        };
        let bytes = Signed::sign(&quote, &self.did, NET)?.encode()?;
        let decoded: Decoded<Quote> = decode(&bytes, now_secs(), NET)?;
        decoded.body.answers(&intent)?;
        self.intent = Some(intent);
        self.quote = Some(decoded);
        Ok(bytes)
    }

    /// The DID the current intent came from (one-time): replies are sealed to it.
    pub fn user_did(&self) -> Option<intents_protocol::DidKey> {
        self.intent.as_ref().map(|i| i.signer)
    }

    pub fn take_accept(&mut self, accept_bytes: &[u8]) -> Result<()> {
        let accept: Decoded<Accept> = decode(accept_bytes, now_secs(), NET)?;
        let (intent, quote) = (self.intent.as_ref().context("no intent")?, self.quote.as_ref().context("no quote")?);
        accept.body.answers(&accept.signer, intent, quote, now_secs())?;
        self.accept = Some(accept);
        Ok(())
    }

    /// Checks the user's signed `npt_locked`: this deal, from the NPT giver, the note against
    /// the agreed terms, and the lock on the solver's own node at depth.
    pub fn check_npt_lock(&mut self, npt_locked: &[u8], min_depth: u64) -> Result<u64> {
        let (intent, quote, accept) = (
            self.intent.as_ref().context("no intent")?,
            self.quote.as_ref().context("no quote")?,
            self.accept.as_ref().context("no accept")?,
        );
        let locked: Decoded<NptLocked> = decode(npt_locked, now_secs(), NET)?;
        locked.body.answers(&locked.signer, accept, &accept.signer)?;
        let note: HtlcNote = serde_json::from_value(locked.body.note.clone()).context("not an HTLC note")?;
        let note = &note;
        note.check()?;
        let htlc = note.htlc()?;
        ensure!(Some(hex(&htlc.hash_lock)[2..].to_string()) == accept.body.hash_lock, "not the accepted hash lock");
        ensure!(htlc.claim_lock.to_hex() == quote.body.claim_key, "not claimable by this solver");
        ensure!(
            note.utxo.get_native_currency_amount().to_nau() == intent.body.give_amount.units() as i128,
            "not the intent's amount"
        );
        let least_refund_ms = (quote.body.expires_at.saturating_sub(300) + quote.body.neptune_lock_secs) * 1000;
        ensure!(htlc.refund_after >= least_refund_ms - 600_000, "the NPT lock refunds too early");
        let status = self.neptune.call("htlc_status", json!({ "note": note, "from_height": locked.body.from_height }))?;
        ensure!(status["found"] == true, "the NPT lock is not on chain (yet)");
        let depth = status["depth"].as_u64().unwrap_or(0);
        ensure!(depth >= min_depth, "the NPT lock is {depth} blocks deep, {min_depth} needed");
        self.npt = Some((note.clone(), locked.body.from_height));
        Ok(depth)
    }

    /// Locks the quoted ETH for the user, refunding well before the NPT lock does, and
    /// returns the signed `evm_locked` message saying where.
    pub fn lock_eth(&mut self, evm: &EvmChain) -> Result<Vec<u8>> {
        let (quote, accept, npt) = (
            self.quote.as_ref().context("no quote")?,
            self.accept.as_ref().context("no accept")?,
            self.npt.as_ref().context("no checked NPT lock")?,
        );
        let neptune_refund = npt.0.refund_after_ms / 1000;
        let evm_now = evm.node.now()?;
        let refund_after = other_refund_at(Chain::Base, neptune_refund, quote.body.other_lock_secs, evm_now)?;
        let receiver = parse_address(&accept.body.claim_key).context("user key")?;
        let hash_lock = parse_word(accept.body.hash_lock.as_deref().context("hash lock")?).context("hash lock")?;
        let amount = quote.body.want_amount.units();
        evm.node.send(&self.eth, &evm.contract, amount, evm_htlc::lock(&receiver, &hash_lock, refund_after))?;
        let id = lock_id(&self.eth.address, &receiver, &[0; 20], amount, &hash_lock, refund_after);
        ensure!(evm.lock_view(&id)?.sender == self.eth.address, "the ETH lock is not on chain");
        self.eth_lock = Some(id);
        let message = EvmLocked {
            v: PROTOCOL_VERSION,
            deal: accept.digest.clone(),
            contract: eth_key_hex(&evm.contract),
            id: hex(&id)[2..].to_string(),
        };
        Ok(Signed::sign(&message, &self.did, NET)?.encode()?)
    }

    /// Reads the secret from the user's ETH claim and claims the NPT with it.
    pub fn claim_npt(&self, evm: &EvmChain) -> Result<()> {
        let (accept, npt, id) = (
            self.accept.as_ref().context("no accept")?,
            self.npt.as_ref().context("no NPT lock")?,
            self.eth_lock.context("no ETH lock")?,
        );
        let hash_lock = parse_word(accept.body.hash_lock.as_deref().context("hash lock")?).context("hash lock")?;
        let logs = evm.node.logs(&evm.contract, &evm_htlc::keccak(b"Claimed(bytes32,bytes32)"), &id, 0)?;
        let secret = logs
            .iter()
            .find_map(|l| evm_htlc::claimed_secret(&l.as_log(), &evm.contract, &id, &hash_lock))
            .context("the user has not claimed the ETH yet")?;
        self.neptune.call(
            "htlc_claim",
            json!({
                "note": npt.0,
                "preimage": hex(&secret)[2..],
                "claim_index": self.claim_index,
                "fee": NPT_FEE,
                "from_height": npt.1,
            }),
        )?;
        Ok(())
    }
}

/// `a * b / c`, rounded down, reducing the fraction first so typical prices
/// (wei per NPT against 4·10^30 nau per NPT) stay within u128.
pub fn mul_div(a: u128, b: u128, c: u128) -> Option<u128> {
    fn gcd(mut x: u128, mut y: u128) -> u128 {
        while y != 0 {
            (x, y) = (y, x % y);
        }
        x
    }
    let g = gcd(b, c);
    let (b, c) = (b / g, c / g);
    let h = gcd(a, c);
    let (a, c) = (a / h, c / h);
    a.checked_mul(b).map(|n| n / c)
}

#[cfg(test)]
mod tests {
    use super::mul_div;

    #[test]
    fn prices_do_not_overflow() {
        let nau = 4 * 10u128.pow(30);
        assert_eq!(mul_div(2 * nau, 5 * 10u128.pow(17), nau), Some(10u128.pow(18)));
        assert_eq!(mul_div(nau / 2, 3 * 10u128.pow(15), nau), Some(15 * 10u128.pow(14)));
        assert_eq!(mul_div(7, 3, 2), Some(10));
        assert_eq!(mul_div(u128::MAX, u128::MAX, 1), None);
    }
}
