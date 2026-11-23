//! Whole swaps on local chains: the app's own swap engine (`intents_ui::swap`) as the user,
//! against the solver (`solver::deal`), each with its own `neptune-provider` wallet on the
//! regtest node, and the swap contract at its fixed address on anvil. A local test key
//! stands in for elastOS Wallet on the user's side; messages are sealed and opened exactly
//! as on the carrier, and handed over directly. Both directions, with ETH.
//!
//! Needs `scripts/regtest-node.sh` and `anvil --chain-id 8453 --port 8545 --host 127.0.0.1`
//! running, `scripts/deploy-htlc-local.sh`, and two funded regtest wallets:
//!   NEPTUNE_SWAP_E2E=<user provider data>,<solver provider data> \
//!     cargo test --release -p intents-ui --test swap_e2e -- --ignored --nocapture

use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use ed25519_dalek::SigningKey;
use evm_htlc::{hex, Address, HTLC_ADDRESS};
use evm_signer::{Node, Wallet};
use intents_protocol::{decode, open, seal, Accept, Amount, Asset, Chain, DidKey, EvmLocked, Intent, Net, NptLocked, Offer, PROTOCOL_VERSION};
use intents_ui::swap::{Step, Swap};
use serde_json::json;
use solver::deal::{matches, Chains, Deal, Stage};
use solver::neptune::Provider;

// anvil's published test accounts: 1 for the solver, 2 for the user (local chain only).
const SOLVER_EVM_KEY: &str = "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const USER_EVM_KEY: &str = "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const NAU: u128 = 4 * 10u128.pow(30);

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_secs()
}

fn provider(data: &str) -> Provider {
    Provider {
        binary: "../../target/release/neptune-provider".into(),
        data: data.into(),
        network: "neptune-regtest".into(),
        config: r#"{"networks":[{"id":"neptune-regtest","rpc":"http://127.0.0.1:9797"}]}"#.into(),
    }
}

/// Retries `f` until it gives Some, for up to `secs`.
fn wait<T>(what: &str, secs: u64, mut f: impl FnMut() -> Result<Option<T>>) -> Result<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = f()? {
            return Ok(v);
        }
        if start.elapsed() > Duration::from_secs(secs) {
            bail!("timed out waiting for {what}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

struct World {
    user_npt: Provider,
    solver_npt: Provider,
    evm: Node,
    user_wallet: Wallet,
    solver_wallet: Wallet,
    solver_key: SigningKey,
}

impl World {
    fn new() -> Result<Self> {
        let dirs = std::env::var("NEPTUNE_SWAP_E2E").context("NEPTUNE_SWAP_E2E=<user data>,<solver data>")?;
        let (user, solver) = dirs.split_once(',').context("two folders")?;
        let evm = Node::local("http://127.0.0.1:8545")?;
        let code = evm.call("eth_getCode", json!([hex(&HTLC_ADDRESS), "latest"]))?;
        anyhow::ensure!(evm_htlc::is_htlc_code(&evm_htlc::parse_hex(code.as_str().unwrap_or("")).unwrap_or_default()), "run scripts/deploy-htlc-local.sh first");
        Ok(Self {
            user_npt: provider(user),
            solver_npt: provider(solver),
            evm,
            user_wallet: Wallet::from_hex(USER_EVM_KEY)?,
            solver_wallet: Wallet::from_hex(SOLVER_EVM_KEY)?,
            solver_key: SigningKey::from_bytes(&[0x5a; 32]),
        })
    }

    fn chains(&self) -> Chains<'_> {
        Chains { neptune: &self.solver_npt, evm: &self.evm, wallet: &self.solver_wallet }
    }

    fn solver_did(&self) -> String {
        DidKey::new(self.solver_key.verifying_key()).to_string()
    }

    /// The solver opens a message the user sealed to it.
    fn to_solver(&self, sealed: &[u8]) -> Result<Vec<u8>> {
        Ok(open(&self.solver_key, sealed)?)
    }

    /// The solver's signed reply, sealed to the user's one-time DID and opened by the swap.
    fn to_user(&self, swap: &Swap, signed: &[u8]) -> Result<Vec<u8>> {
        let sealed = seal(&DidKey::parse(&swap.me())?, signed)?;
        swap.open(&sealed).context("not sealed to the swap")
    }

    fn mine_evm(&self, blocks: u64) -> Result<()> {
        self.evm.call("anvil_mine", json!([format!("0x{blocks:x}")]))?;
        Ok(())
    }

    fn sync(&self) -> Result<()> {
        self.user_npt.sync()?;
        self.solver_npt.sync()
    }
}

fn offer(give: Asset, want: Asset, rate_give: u128, rate_want: u128, min_give: u128, max_give: u128) -> Offer {
    Offer {
        v: PROTOCOL_VERSION,
        id: "ab".repeat(16),
        give,
        want,
        rate_give: Amount::new(rate_give),
        rate_want: Amount::new(rate_want),
        min_give: Amount::new(min_give),
        max_give: Amount::new(max_give),
        expires_at: now() + 1200,
        withdrawn: false,
    }
}

/// Quote and accept, the same in both directions.
fn quote_and_accept(w: &World, intent: Intent, offer: &Offer, user_account: &Address) -> Result<(Swap, Deal)> {
    let (mut swap, sealed) = Swap::start(intent, &w.solver_did(), false, rand_seed(), now()).map_err(anyhow::Error::msg)?;
    let got: intents_protocol::Decoded<Intent> = decode(&w.to_solver(&sealed)?, now(), Net::Test)?;
    let deliver = matches(offer, &got.body).context("the intent takes the offer")?;
    let (mut deal, quote) = Deal::quote(got, deliver, &w.solver_key, Net::Test, &w.chains())?;
    swap.on_quote(&w.to_user(&swap, &quote)?, now()).map_err(anyhow::Error::msg)?;
    let key = w.user_npt.call("htlc_claim_lock", json!({}))?;
    let prepared = if swap.gives_npt { Some(w.user_npt.call("htlc_prepare", json!({}))?) } else { None };
    let prepared_ref = prepared.as_ref().map(|p| (p["hash_lock"].as_str().unwrap(), p["swap_id"].as_str().unwrap()));
    let sealed = swap
        .accept(key["claim_lock"].as_str().unwrap(), key["index"].as_u64().unwrap(), prepared_ref, user_account, now())
        .map_err(anyhow::Error::msg)?;
    let accept: intents_protocol::Decoded<Accept> = decode(&w.to_solver(&sealed)?, now(), Net::Test)?;
    let notice = deal.on_accept(accept, &w.solver_key, Net::Test, &w.chains())?;
    if let Some(notice) = notice {
        // The solver gives NPT: its lock notice goes to the user.
        let request = swap.on_npt_locked(&w.to_user(&swap, &notice)?, now()).map_err(anyhow::Error::msg)?;
        let _ = request;
    }
    Ok((swap, deal))
}

fn rand_seed() -> [u8; 32] {
    let mut seed = [0u8; 32];
    getrandom::getrandom(&mut seed).unwrap();
    seed
}

#[test]
#[ignore = "needs the local chains and funded regtest wallets"]
fn the_user_gives_npt_for_eth() -> Result<()> {
    let w = World::new()?;
    w.sync()?;
    let eth = Asset { chain: Chain::Base, symbol: "ETH".into() };
    // The solver gives 0.5 ETH per NPT, 0.1 to 5 ETH a swap.
    let o = offer(eth.clone(), Asset::npt(), 5 * 10u128.pow(17), NAU, 10u128.pow(17), 5 * 10u128.pow(18));
    let intent = o.intent_for("12".repeat(16), Amount::new(2 * NAU), now() + 600).context("intent")?;
    let user_eth_before = w.evm.balance(&w.user_wallet.address)?;
    let (mut swap, mut deal) = quote_and_accept(&w, intent, &o, &w.user_wallet.address)?;
    assert_eq!(swap.step, Step::Accepted);

    // The user locks its NPT (here proven while it waits).
    let npt = intents_ui::intent::ASSETS.iter().find(|a| a.symbol == "NPT").unwrap();
    let mut request = swap.npt_lock_request(&intents_ui::offers_ui::to_text(swap.intent().give_amount.units(), npt), now()).map_err(anyhow::Error::msg)?;
    request["background"] = json!(false);
    let reply = w.user_npt.call("htlc_lock", request)?;
    swap.npt_lock_started(&reply).map_err(anyhow::Error::msg)?;
    let notice = swap.npt_lock_sent(now()).map_err(anyhow::Error::msg)?;
    let locked: intents_protocol::Decoded<NptLocked> = decode(&w.to_solver(&notice)?, now(), Net::Test)?;
    let mut evm_notice = deal.on_npt_locked(locked, &w.solver_key, Net::Test, &w.chains())?;
    // The solver locks on Base once the NPT lock is deep enough on its node.
    let evm_notice = wait("the solver's Base lock", 300, || {
        if evm_notice.is_none() {
            evm_notice = deal.poll(&w.solver_key, Net::Test, &w.chains())?;
        }
        Ok(evm_notice.take())
    })?;
    println!("solver: {}", deal.line);
    swap.on_evm_locked(&w.to_user(&swap, &evm_notice)?, now()).map_err(anyhow::Error::msg)?;

    // The user reads the lock from the chain, waits for confirmations, then claims.
    let id = swap.evm_id().unwrap();
    let view = solver::deal::lock_view(&w.evm, &id)?;
    assert_eq!(swap.check_evm_lock(&view, 0, now()), Ok(false));
    w.mine_evm(intents_protocol::EVM_CONFIRMATIONS)?;
    assert_eq!(swap.check_evm_lock(&view, intents_protocol::EVM_CONFIRMATIONS, now()), Ok(true));
    let secret = w.user_npt.call("htlc_secret", json!({ "swap_id": swap.swap_id() }))?;
    let tx = swap.claim_tx(&evm_htlc::parse_word(secret["secret"].as_str().unwrap()).unwrap()).map_err(anyhow::Error::msg)?;
    w.evm.send(&w.user_wallet, &tx.to, tx.value, tx.data)?;
    swap.finish("claimed");
    let gained = w.evm.balance(&w.user_wallet.address)? as i128 - user_eth_before as i128;
    println!("user: +{gained} wei (1 ETH less gas)");
    assert!(gained > 99 * 10i128.pow(16), "the user got about 1 ETH");

    // The solver sees the secret on Base and claims the NPT.
    wait("the solver's NPT claim", 120, || {
        deal.poll(&w.solver_key, Net::Test, &w.chains())?;
        Ok((deal.stage == Stage::Done).then_some(()))
    })?;
    println!("solver: {}", deal.line);
    Ok(())
}

#[test]
#[ignore = "needs the local chains and funded regtest wallets"]
fn the_user_gives_eth_for_npt() -> Result<()> {
    let w = World::new()?;
    w.sync()?;
    let eth = Asset { chain: Chain::Base, symbol: "ETH".into() };
    // The solver gives 2 NPT per 1 ETH, 1 to 10 NPT a swap.
    let o = offer(Asset::npt(), eth.clone(), 2 * NAU, 10u128.pow(18), NAU, 10 * NAU);
    let intent = o.intent_for("34".repeat(16), Amount::new(10u128.pow(18)), now() + 600).context("intent")?;
    let (mut swap, mut deal) = quote_and_accept(&w, intent, &o, &w.user_wallet.address)?;
    assert_eq!(swap.step, Step::CheckNptLock, "the solver locked its NPT at once");

    // The user checks the solver's NPT lock on its own node until it is deep enough.
    wait("the solver's NPT lock deep enough", 300, || {
        let reply = w.user_npt.call("htlc_check", swap.npt_check_request().map_err(anyhow::Error::msg)?)?;
        Ok(swap.npt_checked(&reply, now()).map_err(anyhow::Error::msg)?.then_some(()))
    })?;
    assert_eq!(swap.step, Step::LockingEvm);

    // The user locks its ETH on Base and tells the solver.
    let mut block = 0;
    for tx in swap.evm_lock_txs().map_err(anyhow::Error::msg)? {
        block = w.evm.send(&w.user_wallet, &tx.to, tx.value, tx.data)?.block;
    }
    let notice = swap.evm_lock_sent(block, now()).map_err(anyhow::Error::msg)?;
    let locked: intents_protocol::Decoded<EvmLocked> = decode(&w.to_solver(&notice)?, now(), Net::Test)?;
    deal.on_evm_locked(locked)?;

    // The solver checks it, waits for confirmations and claims: the secret goes public.
    wait("the solver's claim on Base", 60, || {
        w.mine_evm(1)?;
        deal.poll(&w.solver_key, Net::Test, &w.chains())?;
        Ok((deal.stage == Stage::Done).then_some(()))
    })?;
    println!("solver: {}", deal.line);

    // The user reads the secret from the Claimed log and claims its NPT.
    let id = swap.evm_id().unwrap();
    let logs = w.evm.logs(&HTLC_ADDRESS, &evm_htlc::keccak(b"Claimed(bytes32,bytes32)"), &id, block)?;
    let hash_lock = swap.hash_lock().unwrap();
    let secret = logs.iter().find_map(|l| evm_htlc::claimed_secret(&l.as_log(), &HTLC_ADDRESS, &id, &hash_lock)).context("no secret")?;
    let mut request = swap.npt_claim_request(&secret).map_err(anyhow::Error::msg)?;
    request["background"] = json!(false);
    let before = w.user_npt.balance()?;
    w.user_npt.call("htlc_claim", request)?;
    let after = wait("the user's NPT", 120, || {
        let b = w.user_npt.balance()?;
        Ok((b != before).then_some(b))
    })?;
    println!("user NPT: {before} → {after}");
    swap.finish("claimed");
    Ok(())
}
