//! A solver that answers Neptune Swap apps over the carrier, for test networks: it publishes
//! its standing offers on the offers topic, opens the sealed messages addressed to it on the
//! deals topic, quotes the intents that take one of its offers, and runs each deal
//! (`solver::deal`) to the end. Its Neptune wallet is its own provider (stdio); its EVM key is
//! a local test key on a local chain.
//!
//!   solver-daemon <config.json>
//!
//! The config (all fields required):
//! ```json
//! {
//!   "coords": "<an elastOS node's home-runtime-coords.json>",
//!   "solver_key": "<32-byte hex seed of the solver's did:key>",
//!   "provider": { "binary": "target/release/neptune-provider", "data": "/tmp/npt-solver",
//!                 "network": "neptune-regtest",
//!                 "config": "{\"networks\":[{\"id\":\"neptune-regtest\",\"rpc\":\"http://127.0.0.1:9797\"}]}" },
//!   "evm": { "rpc": "http://127.0.0.1:8545", "key": "<anvil test key hex>" },
//!   "offers": [ { "give": {"chain":"neptune","symbol":"NPT"}, "want": {"chain":"base","symbol":"USDC"},
//!                 "rate_give": "4000000000000000000000000000000", "rate_want": "150000",
//!                 "min_give": "4000000000000000000000000000000", "max_give": "4000000000000000000000000000000000" } ]
//! }
//! ```

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{ensure, Context, Result};
use ed25519_dalek::SigningKey;
use evm_signer::{Node, Wallet};
use intents_protocol::{decode, open, seal, Accept, Amount, Asset, DidKey, EvmLocked, Intent, Net, NptLocked, Offer, Signed, PROTOCOL_VERSION};
use serde::Deserialize;
use solver::carrier::Carrier;
use solver::deal::{matches, Chains, Deal, Stage};
use solver::neptune::Provider;

#[derive(Deserialize)]
struct Config {
    coords: PathBuf,
    solver_key: String,
    provider: ProviderConfig,
    evm: EvmConfig,
    offers: Vec<OfferTerms>,
    /// Carrier node ids to join the topics with (optional): e.g. the node of the Home whose
    /// apps should see this solver, when the two nodes do not discover each other.
    #[serde(default)]
    peers: Vec<String>,
}

#[derive(Deserialize)]
struct ProviderConfig {
    binary: PathBuf,
    data: PathBuf,
    network: String,
    config: String,
}

#[derive(Deserialize)]
struct EvmConfig {
    rpc: String,
    key: String,
}

#[derive(Deserialize, Clone)]
struct OfferTerms {
    give: Asset,
    want: Asset,
    rate_give: Amount,
    rate_want: Amount,
    min_give: Amount,
    max_give: Amount,
}

/// Offers last this long and are republished at half of it.
const OFFER_SECS: u64 = 20 * 60;
/// Limits against spam: quotes a minute, deals open at once, and NPT tied up at once in
/// locks this solver made for takers who have not finished (a taker who accepts and walks
/// away ties up the solver's NPT until the timeout; this caps what that can cost).
const QUOTES_PER_MINUTE: usize = 20;
const OPEN_DEALS_MAX: usize = 20;
const NPT_EXPOSURE_MAX: u128 = 50 * 4 * 10u128.pow(30);

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn main() -> Result<()> {
    let path = std::env::args().nth(1).context("usage: solver-daemon <config.json>")?;
    let config: Config = serde_json::from_slice(&std::fs::read(&path)?)?;
    let seed: [u8; 32] = evm_htlc::parse_word(&config.solver_key).context("solver_key: 32 bytes hex")?;
    let key = SigningKey::from_bytes(&seed);
    let me = DidKey::new(key.verifying_key()).to_string();
    ensure!(config.evm.rpc.starts_with("http://127.0.0.1:") || config.evm.rpc.starts_with("http://localhost:"), "test networks only: a local EVM chain");
    let net = if config.provider.network == "neptune-main" { Net::Main } else { Net::Test };
    ensure!(net == Net::Test, "test networks only");
    let (offers_topic, deals_topic) = (intents_protocol::OFFERS_TOPIC_TEST, intents_protocol::DEALS_TOPIC_TEST);

    let neptune = Provider {
        binary: config.provider.binary.clone(),
        data: config.provider.data.clone(),
        network: config.provider.network.clone(),
        config: config.provider.config.clone(),
    };
    let evm = Node::local(&config.evm.rpc)?;
    let wallet = Wallet::from_hex(&config.evm.key)?;
    // A loopback RPC is not proof of a test chain (a local full node, a fork of Base): the
    // chain must also be young and carry a test chain's id.
    let height = evm.call("eth_blockNumber", serde_json::json!([]))?;
    let height = u64::from_str_radix(height.as_str().unwrap_or("0x0").trim_start_matches("0x"), 16)?;
    let chain_id = evm.chain_id()?;
    ensure!(height < 1_000_000 && matches!(chain_id, 8453 | 31337), "test networks only: chain {chain_id} at height {height} looks public");
    let chains = Chains { neptune: &neptune, evm: &evm, wallet: &wallet };
    println!("solver {me}\n  NPT {} · EVM {}", neptune.balance()?, evm_htlc::hex(&wallet.address));

    let carrier = Carrier::connect(&config.coords, "neptune-swap", &format!("solver-{}", &me[me.len() - 8..]), "Neptune Swap solver (test networks)", Duration::from_secs(280))?;
    carrier.join_with(offers_topic, &config.peers)?;
    carrier.join_with(deals_topic, &config.peers)?;

    let ids: Vec<String> = config.offers.iter().map(|_| hex16()).collect();
    let mut published = Instant::now() - Duration::from_secs(OFFER_SECS);
    let mut deals: HashMap<String, Deal> = HashMap::new();
    loop {
        if published.elapsed() >= Duration::from_secs(OFFER_SECS / 2) {
            for (terms, id) in config.offers.iter().zip(&ids) {
                let offer = offer_of(terms, id);
                match Signed::sign(&offer, &key, net).and_then(|s| s.encode()) {
                    Ok(bytes) => {
                        // The carrier sends the bytes as base64url, as the app's offer board reads them.
                        if let Err(e) = carrier.send(offers_topic, &bytes) {
                            eprintln!("publishing an offer: {e:#}");
                        }
                    }
                    Err(e) => eprintln!("signing an offer: {e}"),
                }
            }
            published = Instant::now();
            println!("offers published ({})", config.offers.len());
            // A peer that joined a topic after us never heard our first join: ask again.
            if !config.peers.is_empty() {
                for topic in [offers_topic, deals_topic] {
                    if let Err(e) = carrier.join_with(topic, &config.peers) {
                        eprintln!("joining the peers on {topic}: {e:#}");
                    }
                }
            }
        }

        for sealed in carrier.recv(deals_topic).unwrap_or_default() {
            let Ok(plain) = open(&key, &sealed) else { continue };
            if let Err(e) = handle(&plain, &key, net, &chains, &config.offers, &ids, &mut deals, &carrier, deals_topic) {
                eprintln!("a message: {e:#}");
            }
        }
        for deal in deals.values_mut() {
            match deal.poll(&key, net, &chains) {
                Ok(Some(bytes)) => send_to(&carrier, deals_topic, &deal.user, &bytes),
                Ok(None) => {}
                Err(e) => deal.line = format!("{e:#}"),
            }
        }
        deals.retain(|digest, deal| {
            let done = matches!(deal.stage, Stage::Done | Stage::Failed);
            if done {
                println!("deal {}…: {}", &digest[..12], deal.line);
            }
            !done
        });
        std::thread::sleep(Duration::from_secs(3));
    }
}

thread_local! {
    /// When the last quotes went out (Unix seconds).
    static QUOTED: std::cell::RefCell<Vec<u64>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn hex16() -> String {
    rand::random::<[u8; 16]>().iter().map(|b| format!("{b:02x}")).collect()
}

fn offer_of(terms: &OfferTerms, id: &str) -> Offer {
    Offer {
        v: PROTOCOL_VERSION,
        id: id.to_string(),
        give: terms.give.clone(),
        want: terms.want.clone(),
        rate_give: terms.rate_give,
        rate_want: terms.rate_want,
        min_give: terms.min_give,
        max_give: terms.max_give,
        expires_at: now_secs() + OFFER_SECS,
        withdrawn: false,
    }
}

fn send_to(carrier: &Carrier, topic: &str, to: &str, bytes: &[u8]) {
    let sealed = DidKey::parse(to).map_err(anyhow::Error::from).and_then(|did| Ok(seal(&did, bytes)?));
    match sealed.and_then(|s| carrier.send(topic, &s)) {
        Ok(()) => {}
        Err(e) => eprintln!("sending to {to}: {e:#}"),
    }
}

/// One opened message: a new intent, or the next message of a deal.
#[allow(clippy::too_many_arguments)]
fn handle(
    plain: &[u8],
    key: &SigningKey,
    net: Net,
    chains: &Chains,
    offers: &[OfferTerms],
    ids: &[String],
    deals: &mut HashMap<String, Deal>,
    carrier: &Carrier,
    topic: &str,
) -> anyhow::Result<()> {
    let now = now_secs();
    if let Ok(intent) = decode::<Intent>(plain, now, net) {
        // An intent seen before (resent, or replayed) never replaces its deal: that would drop
        // the deal's locks and keys mid-swap.
        if deals.contains_key(&intent.digest) || deals.values().any(|d| d.user == intent.signer.to_string()) {
            return Ok(());
        }
        anyhow::ensure!(deals.len() < OPEN_DEALS_MAX, "too many open deals; not quoting");
        QUOTED.with(|q| {
            let mut q = q.borrow_mut();
            q.retain(|at| now.saturating_sub(*at) < 60);
            anyhow::ensure!(q.len() < QUOTES_PER_MINUTE, "too many quotes this minute");
            q.push(now);
            Ok(())
        })?;
        let deliver = offers.iter().zip(ids).find_map(|(t, id)| matches(&offer_of(t, id), &intent.body)).context("the intent takes none of our offers")?;
        // Checked before quoting: a quote already takes a swap key and a secret.
        let adds = if intent.body.want.is_npt() { deliver.units() } else { 0 };
        let exposed: u128 = deals.values().map(Deal::npt_exposure).sum::<u128>() + adds;
        anyhow::ensure!(exposed <= NPT_EXPOSURE_MAX, "NPT tied up in open deals is at its cap; not quoting");
        let (deal, quote) = Deal::quote(intent, deliver, key, net, chains)?;
        send_to(carrier, topic, &deal.user, &quote);
        println!("quoted {}…", &deal.intent_digest()[..12]);
        deals.insert(deal.intent_digest().to_string(), deal);
        return Ok(());
    }
    if let Ok(accept) = decode::<Accept>(plain, now, net) {
        let deal = deals.values_mut().find(|d| d.user == accept.signer.to_string()).context("an accept for no deal")?;
        if let Some(bytes) = deal.on_accept(accept, key, net, chains)? {
            send_to(carrier, topic, &deal.user.clone(), &bytes);
        }
        return Ok(());
    }
    if let Ok(locked) = decode::<NptLocked>(plain, now, net) {
        let deal = deals.values_mut().find(|d| d.user == locked.signer.to_string()).context("an NPT lock for no deal")?;
        if let Some(bytes) = deal.on_npt_locked(locked, key, net, chains)? {
            send_to(carrier, topic, &deal.user.clone(), &bytes);
        }
        return Ok(());
    }
    if let Ok(locked) = decode::<EvmLocked>(plain, now, net) {
        let deal = deals.values_mut().find(|d| d.user == locked.signer.to_string()).context("an EVM lock for no deal")?;
        deal.on_evm_locked(locked)?;
        return Ok(());
    }
    anyhow::bail!("not a message this solver takes")
}
