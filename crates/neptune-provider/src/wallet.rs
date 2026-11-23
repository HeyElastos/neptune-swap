//! The Neptune wallet the provider holds for one network: seed, scanned coins,
//! balance. The capsule never sees key material except the phrase when it
//! creates or imports a wallet (the user writes it down or types it in).
//!
//! Storage, per network under `NEPTUNE_PROVIDER_DATA/<network id>/`:
//! - `phrase`: the 18 seed words, owner-only (0600). **Plain text: development
//!   only.** Main net needs it sealed (elastOS key provider or a passphrase).
//! - `state.json`: light-core's `WalletState` (coins, sync point). Private, but
//!   it cannot spend.

mod journal;
mod swaps;

pub use swaps::Proving;

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use light_core::history::AddressKind;
use light_core::{rpc, LightWallet, WalletState};
use light_core::neptune_primitives::network::Network;
use serde_json::{json, Value};

/// Blocks fetched per `wallet_getBlocks` call: the node's own cap.
const BATCH: u64 = rpc::MAX_BLOCKS_PER_CALL;
/// Blocks one `wallet_sync` call applies at most, so a call stays short.
pub const SYNC_STEP: u64 = 500;

/// Sends one JSON-RPC body to the node and returns its reply.
pub trait Node {
    fn call(&self, body: String) -> Result<String>;
    /// An owned handle to the same node, for a background job.
    fn handle(&self) -> Box<dyn Node + Send>;
}

/// A payment being proven in the background. Real proving takes about half a
/// minute per input, longer than the Runtime waits for one provider reply.
enum JobState {
    Proving,
    Sent { recorded: bool },
    Failed(String),
}

struct Job {
    network_id: String,
    state: std::sync::Arc<std::sync::Mutex<JobState>>,
    /// A payment's pending entry for the history; swap locks, claims and refunds have none.
    pending: Option<light_core::history::PendingSend>,
    reply: Value,
}

pub struct Wallets {
    root: Option<PathBuf>,
    jobs: std::sync::Mutex<std::collections::HashMap<String, Job>>,
    /// When offers were signed (Unix seconds): a page cannot spend the
    /// solver's name on a flood of offers.
    offers_signed: std::sync::Mutex<Vec<u64>>,
}

/// Offer signatures per hour: five offers republished every ten minutes
/// need 30; withdrawals and new terms fit in the rest.
const OFFER_SIGNS_PER_HOUR: usize = 60;

impl Wallets {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self { root, jobs: Default::default(), offers_signed: Default::default() }
    }

    fn dir(&self, network_id: &str) -> Result<PathBuf> {
        let root = self.root.as_ref().context("wallets are not configured (NEPTUNE_PROVIDER_DATA)")?;
        Ok(root.join(network_id))
    }

    fn load(&self, network_id: &str, network: Network) -> Result<Option<LightWallet>> {
        let dir = self.dir(network_id)?;
        let Ok(phrase) = fs::read_to_string(dir.join("phrase")) else {
            return Ok(None);
        };
        let state: WalletState =
            serde_json::from_slice(&fs::read(dir.join("state.json")).context("wallet state is missing")?)?;
        if state.network != network {
            bail!("the stored wallet is for another network");
        }
        Ok(Some(LightWallet::resume(phrase.trim(), state)?))
    }

    fn save_state(&self, network_id: &str, wallet: &LightWallet) -> Result<()> {
        let dir = self.dir(network_id)?;
        let tmp = dir.join("state.json.tmp");
        fs::write(&tmp, serde_json::to_vec(wallet.state())?)?;
        fs::rename(tmp, dir.join("state.json"))?;
        Ok(())
    }

    fn create_files(&self, network_id: &str, phrase: &str, wallet: &LightWallet) -> Result<()> {
        let dir = self.dir(network_id)?;
        if dir.join("phrase").exists() {
            bail!("a wallet already exists for {network_id}");
        }
        fs::create_dir_all(&dir)?;
        // The wallet's folder and the data root above it: owner only.
        use std::os::unix::fs::PermissionsExt;
        for folder in [dir.as_path(), dir.parent().unwrap_or(&dir)] {
            fs::set_permissions(folder, fs::Permissions::from_mode(0o700))?;
        }
        write_private(&dir.join("phrase"), phrase.as_bytes())?;
        self.save_state(network_id, wallet)
    }

    pub fn status(&self, network_id: &str, network: Network) -> Result<Value> {
        Ok(match self.load(network_id, network)? {
            None => json!({ "wallet": false }),
            Some(w) => json!({
                "wallet": true,
                "address": w.receiving_address()?,
                "synced_height": w.state().sync.map(|s| s.height),
            }),
        })
    }

    /// A new seed, synced from the node's current tip: it has no history.
    pub fn create(&self, network_id: &str, network: Network, node: &dyn Node) -> Result<Value> {
        let phrase = light_core::new_phrase().join(" ");
        let mut wallet = LightWallet::from_phrase(&phrase, network)?;
        let tip = tip_block(node)?;
        wallet.start_at(&tip)?;
        self.create_files(network_id, &phrase, &wallet)?;
        Ok(json!({ "phrase": phrase, "address": wallet.receiving_address()? }))
    }

    /// An existing seed. It scans from `from_height` (the block before its
    /// first payment, or 0 to scan from genesis).
    pub fn import(&self, network_id: &str, network: Network, phrase: &str, from_height: u64, node: &dyn Node) -> Result<Value> {
        let phrase = phrase.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut wallet = LightWallet::from_phrase(&phrase, network)?;
        if from_height > 0 {
            let before = blocks(node, from_height - 1, from_height - 1)?;
            let block = before.first().context("the node has no block before that height")?;
            wallet.start_at(block)?;
        }
        self.create_files(network_id, &phrase, &wallet)?;
        Ok(json!({ "address": wallet.receiving_address()? }))
    }

    pub fn receive(&self, network_id: &str, network: Network) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        Ok(json!({ "address": wallet.receiving_address()? }))
    }

    /// Applies up to `SYNC_STEP` blocks after the sync point.
    pub fn sync(&self, network_id: &str, network: Network, node: &dyn Node) -> Result<Value> {
        let mut wallet = self.load(network_id, network)?.context("no wallet")?;
        // A wallet saved before it kept its own membership proofs: take the
        // mutator set at its sync point, so proofs are kept from here on.
        if let (None, Some(point)) = (wallet.state().mutator_set.as_ref(), wallet.state().sync) {
            if let Some(block) = blocks(node, point.height, point.height)?.into_iter().next() {
                wallet.adopt_mutator_set(&block)?;
            }
        }
        let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
        let start = wallet.state().sync.map_or(0, |s| s.height + 1);
        let end = tip.min(start.saturating_add(SYNC_STEP - 1));
        let (mut received, mut spent) = (0, 0);
        let mut proofs_dropped = None;
        let mut from = start;
        while from <= end {
            let to = end.min(from + BATCH - 1);
            let report = wallet.apply_blocks(blocks(node, from, to)?)?;
            received += report.received;
            spent += report.spent;
            proofs_dropped = report.proofs_dropped.or(proofs_dropped);
            from = to + 1;
        }
        self.save_state(network_id, &wallet)?;
        let height = wallet.state().sync.map(|s| s.height);
        let done = height.is_some_and(|h| h >= tip);
        // At the tip, check its proof ourselves (main net: the only network
        // with an activation checkpoint compiled in), unless already done.
        let verifiable = network == Network::Main;
        let already = wallet.state().verified.as_ref().map(|v| v.height) == height;
        if done && verifiable && !already {
            let synced = height.unwrap_or_default();
            let tip_block = rpc::parse_block(&node.call(rpc::get_block_request(synced))?)?;
            let parent = blocks(node, synced - 1, synced - 1)?.into_iter().next().context("the node lacks the parent block")?;
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(wallet.verify_synced_tip(tip_block, parent, now_ms()))
                .context("the node's chain did not verify; nothing from it is trusted")?;
            self.save_state(network_id, &wallet)?;
        }
        let verified_height = wallet.state().verified.as_ref().map(|v| v.height);
        Ok(json!({
            "synced_height": height, "tip": tip, "received": received, "spent": spent,
            "done": done, "verified_height": verified_height, "verifiable": verifiable,
            "own_proofs": wallet.state().mutator_set.is_some(), "proofs_dropped": proofs_dropped,
        }))
    }

    /// Pays `amount` NPT (plus `fee`) to `to`: the wallet's own membership
    /// proofs (the node's only for coins without one), the transaction built
    /// and proven here, then submitted. The wallet is synced to the tip first.
    pub fn send(&self, network_id: &str, network: Network, to: &str, amount: &str, fee: &str, now_ms: u64, node: &dyn Node) -> Result<Value> {
        let mut wallet = self.load(network_id, network)?.context("no wallet")?;
        catch_up(&mut wallet, node)?;
        let plan = wallet.plan_spend(amount, fee, now_ms)?;
        let basis = match wallet.own_basis(&plan) {
            Some(own) => own,
            None => wallet.node_basis(
                &plan,
                rpc::parse_membership_snapshot(&node.call(rpc::restore_membership_proof_request(plan.index_sets.clone()))?)?,
            )?,
        };
        let proofs = if basis.own { "own" } else { "node" };
        let mut jobs = self.jobs.lock().map_err(|_| anyhow::anyhow!("jobs lock"))?;
        if Self::proving(&jobs, network_id) {
            bail!("a payment is still being proven; wait for it first");
        }
        let (details, built) = wallet.build_payment(to, &plan, basis, now_ms)?;
        // The change key index moved on: keep it, so no later payment reuses it.
        self.save_state(network_id, &wallet)?;

        let reply = json!({ "sent": amount.trim(), "fee": fee.trim(), "inputs": built.inputs, "change": built.change, "proofs": proofs });
        let id = Self::spawn_job(&mut jobs, network_id, details.primitive_witness(), network, node, Some(wallet.pending_for(&plan, now_ms)), reply, now_ms);
        Ok(json!({ "job": id, "state": "proving", "proofs": proofs }))
    }

    /// Proves and submits `witness` on a background thread; `job_status` follows it.
    #[allow(clippy::too_many_arguments)]
    fn spawn_job(
        jobs: &mut std::collections::HashMap<String, Job>,
        network_id: &str,
        witness: light_core::neptune_consensus::transaction::primitive_witness::PrimitiveWitness,
        network: Network,
        node: &dyn Node,
        pending: Option<light_core::history::PendingSend>,
        reply: Value,
        now_ms: u64,
    ) -> String {
        let id = format!("{now_ms:x}-{}", jobs.len());
        let state = std::sync::Arc::new(std::sync::Mutex::new(JobState::Proving));
        let (job_state, job_node) = (state.clone(), node.handle());
        std::thread::spawn(move || {
            let outcome = submit_witness(witness, network, job_node.as_ref());
            *job_state.lock().unwrap() = match outcome {
                Ok(()) => JobState::Sent { recorded: false },
                Err(e) => JobState::Failed(format!("{e:#}")),
            };
        });
        jobs.insert(id.clone(), Job { network_id: network_id.to_string(), state, pending, reply });
        id
    }

    /// Whether a transaction of this network is still being proven: one at a time, so two
    /// never spend the same coins.
    fn proving(jobs: &std::collections::HashMap<String, Job>, network_id: &str) -> bool {
        jobs.values().any(|j| j.network_id == network_id && matches!(*j.state.lock().unwrap(), JobState::Proving))
    }

    /// Where a payment job stands: "proving", "sent" (with what was sent) or
    /// "failed" (with why). A sent payment is recorded as pending the first
    /// time it is seen here.
    pub fn send_status(&self, network_id: &str, network: Network, job: &str) -> Result<Value> {
        let jobs = self.jobs.lock().map_err(|_| anyhow::anyhow!("jobs lock"))?;
        let found = jobs.get(job).filter(|j| j.network_id == network_id).context("no such payment")?;
        let mut state = found.state.lock().unwrap();
        Ok(match &mut *state {
            JobState::Proving => json!({ "state": "proving" }),
            JobState::Failed(message) => json!({ "state": "failed", "message": message }),
            JobState::Sent { recorded } => {
                if !*recorded {
                    if let Some(pending) = &found.pending {
                        let mut wallet = self.load(network_id, network)?.context("no wallet")?;
                        wallet.add_pending(pending.clone());
                        self.save_state(network_id, &wallet)?;
                    }
                    *recorded = true;
                }
                let mut reply = found.reply.clone();
                reply["state"] = json!("sent");
                reply
            }
        })
    }

    /// The address handed out for each kind.
    pub fn addresses(&self, network_id: &str, network: Network) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        Ok(json!({
            "generation": wallet.current_address(AddressKind::Generation)?,
            "ec_hybrid": wallet.current_address(AddressKind::EcHybrid)?,
            // Every address handed out, newest first, with what each received.
            "issued": {
                "generation": wallet.issued_addresses(AddressKind::Generation)?,
                "ec_hybrid": wallet.issued_addresses(AddressKind::EcHybrid)?,
            },
        }))
    }

    /// A fresh address of `kind` ("generation" or "ec_hybrid"); earlier ones keep receiving.
    pub fn new_address(&self, network_id: &str, network: Network, kind: &str) -> Result<Value> {
        let kind = AddressKind::parse(kind)?;
        let mut wallet = self.load(network_id, network)?.context("no wallet")?;
        let issued = wallet.new_address(kind)?;
        self.save_state(network_id, &wallet)?;
        Ok(json!({ "kind": kind, "index": issued.index, "address": issued.address }))
    }

    /// This wallet's solver identity: a `did:key` from its seed, the same on
    /// every restore. The key stays here; only signatures leave.
    pub fn solver_identity(&self, network_id: &str, network: Network) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        let key = ed25519_dalek::SigningKey::from_bytes(&wallet.solver_key_seed());
        Ok(json!({ "did": intents_protocol::DidKey::new(key.verifying_key()).to_string(), "topic": offers_topic(network) }))
    }

    /// Checks `offer` and signs it with the solver key, for publishing on the
    /// public offers topic: the envelope as base64url, as the carrier carries it.
    pub fn offer_sign(&self, network_id: &str, network: Network, offer: &intents_protocol::Offer, now_secs: u64) -> Result<Value> {
        use base64::Engine as _;
        use intents_protocol::Message as _;
        offer.validate(now_secs).map_err(|e| anyhow::anyhow!("not a valid offer: {e}"))?;
        {
            let mut signed = self.offers_signed.lock().map_err(|_| anyhow::anyhow!("offers lock"))?;
            signed.retain(|at| at + 3600 > now_secs);
            if signed.len() >= OFFER_SIGNS_PER_HOUR {
                bail!("too many offers signed in the last hour; wait a little");
            }
            signed.push(now_secs);
        }
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        let key = ed25519_dalek::SigningKey::from_bytes(&wallet.solver_key_seed());
        let net = if network == Network::Main { intents_protocol::Net::Main } else { intents_protocol::Net::Test };
        let bytes = intents_protocol::Signed::sign(offer, &key, net)
            .and_then(|s| s.encode())
            .map_err(|e| anyhow::anyhow!("signing failed: {e}"))?;
        Ok(json!({
            "did": intents_protocol::DidKey::new(key.verifying_key()).to_string(),
            "topic": offers_topic(network),
            "message": base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes),
        }))
    }

    /// Every payment, newest first, pending ones included.
    pub fn history(&self, network_id: &str, network: Network) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        Ok(json!({ "items": wallet.history() }))
    }

    pub fn balance(&self, network_id: &str, network: Network, now_ms: u64) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        Ok(json!({
            "balance": wallet.balance(now_ms),
            "synced_height": wallet.state().sync.map(|s| s.height),
        }))
    }
}

/// Applies the blocks the wallet has not seen, up to the node's tip, and
/// returns the tip. A coin spent in an unseen block would make the node refuse
/// a transaction that spends it again.
fn catch_up(wallet: &mut LightWallet, node: &dyn Node) -> Result<u64> {
    let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
    if tip.saturating_sub(wallet.state().sync.map_or(0, |s| s.height)) > SYNC_STEP {
        bail!("the wallet is far behind the node; sync it first");
    }
    let mut from = wallet.state().sync.map_or(0, |s| s.height + 1);
    while from <= tip {
        let to = tip.min(from + BATCH - 1);
        wallet.apply_blocks(blocks(node, from, to)?)?;
        from = to + 1;
    }
    Ok(tip)
}

/// Proves the transaction (checking every lock and type script first) and
/// hands it to the node.
fn prove_and_submit(details: &light_core::neptune_wallet::transaction_details::TransactionDetails, node: &dyn Node) -> Result<()> {
    submit_witness(details.primitive_witness(), details.network, node)
}

/// Proves an assembled primitive witness and hands the transaction to the node.
fn submit_witness(
    witness: light_core::neptune_consensus::transaction::primitive_witness::PrimitiveWitness,
    network: Network,
    node: &dyn Node,
) -> Result<()> {
    // The tip picks the consensus rule set the proofs are made for.
    let height = rpc::parse_height(&node.call(rpc::height_request())?)?;
    let transaction = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(light_core::send::prove_witness(witness, network, height))?;
    let rpc_transaction = light_core::neptune_rpc_api::model::wallet::transaction::RpcTransaction::try_from(transaction)
        .map_err(|e| anyhow::anyhow!(e))?;
    if !rpc::parse_submitted(&node.call(rpc::submit_transaction_request(rpc_transaction))?)? {
        bail!("the node did not accept the transaction");
    }
    Ok(())
}

fn tip_block(node: &dyn Node) -> Result<light_core::neptune_rpc_api::model::wallet::block::RpcWalletBlock> {
    let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
    blocks(node, tip, tip)?.into_iter().next().context("the node did not return its tip")
}

fn blocks(node: &dyn Node, from: u64, to: u64) -> Result<Vec<light_core::neptune_rpc_api::model::wallet::block::RpcWalletBlock>> {
    rpc::parse_blocks(&node.call(rpc::get_blocks_request(from, to))?)
}

/// Main net offers on their own topic; every other network shares the test one.
fn offers_topic(network: Network) -> &'static str {
    if network == Network::Main { intents_protocol::OFFERS_TOPIC_MAIN } else { intents_protocol::OFFERS_TOPIC_TEST }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as u64)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Read;
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A node whose tip is main-net block 62,980 (light-core's fixture).
    /// A main-net node whose tip is block 62,981 (light-core's fixtures: the
    /// tip with its proof, and its parent 62,980).
    #[derive(Clone)]
    struct FixtureNode {
        parent: String,
        tip_full: String,
        tip_wallet: String,
    }

    fn gunzip(gz: &[u8]) -> String {
        let mut text = String::new();
        flate2::read::GzDecoder::new(gz).read_to_string(&mut text).unwrap();
        text
    }

    impl FixtureNode {
        fn new() -> Self {
            let parent = gunzip(include_bytes!("../../light-core/fixtures/main-parent.json.gz"));
            // chain_tip's reply has archival_getBlock's shape: {"block": ...}.
            let tip_full = gunzip(include_bytes!("../../light-core/fixtures/main-tip.json.gz"));
            let block: light_core::neptune_consensus::block::Block = rpc::parse_tip(&tip_full).unwrap().into();
            let wallet_block = light_core::neptune_rpc_api::model::wallet::block::RpcWalletBlock::from(&block);
            let tip_wallet = json!({ "jsonrpc": "2.0", "id": 1, "result": { "blocks": [wallet_block] } }).to_string();
            Self { parent, tip_full, tip_wallet }
        }
    }

    impl Node for FixtureNode {
        fn handle(&self) -> Box<dyn Node + Send> {
            Box::new(self.clone())
        }

        fn call(&self, body: String) -> Result<String> {
            let v: Value = serde_json::from_str(&body)?;
            match v["method"].as_str() {
                Some("chain_height") => Ok(r#"{"jsonrpc":"2.0","id":1,"result":{"height":62981}}"#.into()),
                Some("wallet_getBlocks") if v["params"] == json!([62980, 62980]) => Ok(self.parent.clone()),
                Some("wallet_getBlocks") if v["params"] == json!([62981, 62981]) => Ok(self.tip_wallet.clone()),
                Some("wallet_getBlocks") => Ok(r#"{"jsonrpc":"2.0","id":1,"result":{"blocks":[]}}"#.into()),
                Some("archival_getBlock") if v["params"] == json!([62981]) => Ok(self.tip_full.clone()),
                other => bail!("unexpected call {other:?}"),
            }
        }
    }

    fn temp_root() -> PathBuf {
        // Tests run in parallel: a counter keeps two from sharing a folder.
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("neptune-provider-test-{}-{n}-{}", std::process::id(), now_nanos()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn now_nanos() -> u128 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    }

    #[test]
    fn create_starts_at_the_tip_and_keeps_the_phrase_private() {
        let root = temp_root();
        let wallets = Wallets::new(Some(root.clone()));
        let node = FixtureNode::new();
        assert_eq!(wallets.status("neptune-main", Network::Main).unwrap()["wallet"], false);

        let created = wallets.create("neptune-main", Network::Main, &node).unwrap();
        let phrase = created["phrase"].as_str().unwrap();
        assert_eq!(phrase.split(' ').count(), 18);
        let mode = fs::metadata(root.join("neptune-main/phrase")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        for folder in [root.clone(), root.join("neptune-main")] {
            assert_eq!(fs::metadata(folder).unwrap().permissions().mode() & 0o777, 0o700);
        }

        let status = wallets.status("neptune-main", Network::Main).unwrap();
        assert_eq!(status["synced_height"], 62981);
        assert_eq!(status["address"], created["address"]);
        // Synced to the tip already: nothing to apply, balance zero.
        let synced = wallets.sync("neptune-main", Network::Main, &node).unwrap();
        assert_eq!(synced["done"], true);
        // At the tip, the wallet checked block 62,981's proof itself.
        assert_eq!(synced["verified_height"], 62981);
        assert_eq!(wallets.balance("neptune-main", Network::Main, 0).unwrap()["balance"]["unspent_coins"], 0);

        assert!(wallets.create("neptune-main", Network::Main, &node).is_err(), "never overwrite a seed");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn import_restores_the_same_address_and_refuses_a_bad_phrase() {
        let root = temp_root();
        let wallets = Wallets::new(Some(root.clone()));
        let node = FixtureNode::new();
        let created = wallets.create("neptune-main", Network::Main, &node).unwrap();
        let other = Wallets::new(Some(root.join("other")));
        let imported = other
            .import("neptune-main", Network::Main, created["phrase"].as_str().unwrap(), 0, &node)
            .unwrap();
        assert_eq!(imported["address"], created["address"]);
        assert!(other.import("neptune-test", Network::Main, "not a phrase", 0, &node).is_err());
        assert!(Wallets::new(None).status("neptune-main", Network::Main).is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn swap_keys_are_handed_out_in_turn_and_kept_private() {
        let root = temp_root();
        let wallets = Wallets::new(Some(root.clone()));
        let node = FixtureNode::new();
        wallets.create("neptune-main", Network::Main, &node).unwrap();
        let first = wallets.htlc_claim_lock("neptune-main", Network::Main).unwrap();
        let second = wallets.htlc_claim_lock("neptune-main", Network::Main).unwrap();
        assert_eq!((first["index"].as_u64(), second["index"].as_u64()), (Some(0), Some(1)));
        assert_ne!(first["claim_lock"], second["claim_lock"]);
        let mode = fs::metadata(root.join("neptune-main/swaps.json")).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn swap_requests_are_checked_before_any_spend() {
        let root = temp_root();
        let wallets = Wallets::new(Some(root.clone()));
        let node = FixtureNode::new();
        wallets.create("neptune-main", Network::Main, &node).unwrap();
        let lock = wallets.htlc_claim_lock("neptune-main", Network::Main).unwrap();
        let claim_lock = lock["claim_lock"].as_str().unwrap();
        let past = wallets.htlc_lock("neptune-main", Network::Main, "1", "0.05", claim_lock, 1_000, None, 2_000, &node, Proving::Wait);
        assert!(past.unwrap_err().to_string().contains("future"));
        let bad = wallets.htlc_lock("neptune-main", Network::Main, "1", "0.05", "not a digest", 3_000, None, 2_000, &node, Proving::Wait);
        assert!(bad.unwrap_err().to_string().contains("claim lock"));
        assert!(wallets.htlc_secret("neptune-main", "nope").is_err());
        let prepared = wallets.htlc_prepare("neptune-main", Network::Main, 2_000).unwrap();
        assert_eq!(prepared["hash_lock"].as_str().unwrap().len(), 64);
        assert!(prepared["hash_lock"].as_str().unwrap().starts_with(prepared["swap_id"].as_str().unwrap()));
        // A prepared secret with no lock is dropped a day later, when the next one is made.
        let later = wallets.htlc_prepare("neptune-main", Network::Main, 2_000 + 24 * 3600 * 1000).unwrap();
        let book: serde_json::Value = serde_json::from_slice(&fs::read(root.join("neptune-main/swaps.json")).unwrap()).unwrap();
        let kept: Vec<&serde_json::Value> = book["prepared"].as_array().unwrap().iter().map(|p| &p["id"]).collect();
        assert_eq!(kept, [&later["swap_id"]]);
        let unknown = wallets.htlc_lock("neptune-main", Network::Main, "1", "0.05", claim_lock, 3_000, Some("nope"), 2_000, &node, Proving::Background);
        assert!(unknown.unwrap_err().to_string().contains("no such prepared swap"));
        assert!(wallets.htlc_status("neptune-main", None, None, 0, &node).is_err());
        let status = wallets.htlc_status("neptune-main", later["swap_id"].as_str(), None, 0, &node).unwrap();
        assert_eq!(status["locked"], false, "prepared, never locked");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn swap_records_are_kept_bounded_and_private() {
        let root = temp_root();
        let wallets = Wallets::new(Some(root.clone()));
        let id = "00112233445566778899aabbccddeeff";
        wallets.swap_save("neptune-main", id, serde_json::json!({"step": "quoted"})).unwrap();
        wallets.swap_save("neptune-main", id, serde_json::json!({"step": "locked"})).unwrap();
        let list = wallets.swap_list("neptune-main").unwrap();
        assert_eq!(list["swaps"][id]["step"], "locked");
        assert_eq!(list["swaps"].as_object().unwrap().len(), 1);
        let mode = fs::metadata(root.join("neptune-main/journal.json")).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        assert!(wallets.swap_save("neptune-main", "../../etc", serde_json::json!({})).is_err(), "ids are hex");
        assert!(wallets.swap_save("neptune-main", "ABCDEF0123456789", serde_json::json!({})).is_err(), "lowercase only");
        assert!(wallets.swap_save("neptune-main", id, serde_json::json!("text")).is_err(), "an object");
        let big = serde_json::json!({ "x": "a".repeat(40 * 1024) });
        assert!(wallets.swap_save("neptune-main", id, big).is_err(), "bounded");
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn the_solver_signs_offers_with_one_stable_did() {
        use base64::Engine as _;
        let root = temp_root();
        let wallets = Wallets::new(Some(root.clone()));
        wallets.create("neptune-main", Network::Main, &FixtureNode::new()).unwrap();
        let id = wallets.solver_identity("neptune-main", Network::Main).unwrap();
        assert!(id["did"].as_str().unwrap().starts_with("did:key:z6Mk"));
        assert_eq!(id["topic"], "neptune-swap/offers/main");
        let now = 1_800_000_000;
        let offer = intents_protocol::Offer {
            v: intents_protocol::PROTOCOL_VERSION,
            id: "0123456789abcdef0123456789abcdef".into(),
            give: intents_protocol::Asset::npt(),
            want: intents_protocol::Asset { chain: intents_protocol::Chain::Ethereum, symbol: "USDC".into() },
            rate_give: intents_protocol::Amount::new(4 * 10u128.pow(30)),
            rate_want: intents_protocol::Amount::new(150_000),
            min_give: intents_protocol::Amount::new(4 * 10u128.pow(31)),
            max_give: intents_protocol::Amount::new(4 * 10u128.pow(34)),
            expires_at: now + 1800,
            withdrawn: false,
        };
        let signed = wallets.offer_sign("neptune-main", Network::Main, &offer, now).unwrap();
        assert_eq!(signed["did"], id["did"]);
        let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(signed["message"].as_str().unwrap()).unwrap();
        let decoded = intents_protocol::decode::<intents_protocol::Offer>(&bytes, now, intents_protocol::Net::Main).unwrap();
        assert_eq!(decoded.signer.to_string(), id["did"].as_str().unwrap());
        assert_eq!(decoded.body, offer);
        // An invalid or expired offer is not signed.
        assert!(wallets.offer_sign("neptune-main", Network::Main, &offer, now + 3600).is_err());
        // Nor more than the hourly budget.
        for _ in 1..OFFER_SIGNS_PER_HOUR {
            wallets.offer_sign("neptune-main", Network::Main, &offer, now).unwrap();
        }
        assert!(wallets.offer_sign("neptune-main", Network::Main, &offer, now).is_err());
        assert!(wallets.offer_sign("neptune-main", Network::Main, &offer, now + 3601).is_err(), "expired offer");
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn addresses_rotate_and_history_starts_empty() {
        let root = temp_root();
        let wallets = Wallets::new(Some(root.clone()));
        let node = FixtureNode::new();
        let created = wallets.create("neptune-main", Network::Main, &node).unwrap();

        let addresses = wallets.addresses("neptune-main", Network::Main).unwrap();
        assert_eq!(addresses["generation"]["address"], created["address"]);
        assert_eq!(addresses["generation"]["index"], 0);
        let short = addresses["ec_hybrid"]["address"].as_str().unwrap().to_string();
        assert!(short.len() < created["address"].as_str().unwrap().len());

        let fresh = wallets.new_address("neptune-main", Network::Main, "generation").unwrap();
        assert_eq!(fresh["index"], 1);
        assert_ne!(fresh["address"], created["address"]);
        // Kept across loads, and the status shows the new one.
        let addresses = wallets.addresses("neptune-main", Network::Main).unwrap();
        assert_eq!(addresses["generation"]["index"], 1);
        // Both stay listed, newest first; the short kind still has only its first.
        let issued = addresses["issued"]["generation"].as_array().unwrap();
        assert_eq!(issued.len(), 2);
        assert_eq!((issued[0]["index"].clone(), issued[0]["current"].clone()), (json!(1), json!(true)));
        assert_eq!((issued[1]["address"].clone(), issued[1]["payments"].clone()), (created["address"].clone(), json!(0)));
        assert_eq!(addresses["issued"]["ec_hybrid"].as_array().unwrap().len(), 1);
        assert_eq!(wallets.status("neptune-main", Network::Main).unwrap()["address"], fresh["address"]);
        assert!(wallets.new_address("neptune-main", Network::Main, "taproot").is_err());

        assert_eq!(wallets.history("neptune-main", Network::Main).unwrap()["items"], json!([]));
        fs::remove_dir_all(root).unwrap();
    }
}
