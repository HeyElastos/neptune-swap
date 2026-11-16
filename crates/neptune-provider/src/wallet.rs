//! The Neptune wallet the provider holds for one network: seed, scanned coins,
//! balance. The capsule never sees key material except the phrase when it
//! creates or imports a wallet (the user writes it down or types it in).
//!
//! Storage, per network under `NEPTUNE_PROVIDER_DATA/<network id>/`:
//! - `phrase`: the 18 seed words, owner-only (0600). **Plain text: development
//!   only.** Main net needs it sealed (elastOS key provider or a passphrase).
//! - `state.json`: light-core's `WalletState` (coins, sync point). Private, but
//!   it cannot spend.

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
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
}

pub struct Wallets {
    root: Option<PathBuf>,
}

impl Wallets {
    pub fn new(root: Option<PathBuf>) -> Self {
        Self { root }
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
        wallet.set_sync_point(LightWallet::sync_point_of(&tip));
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
            wallet.set_sync_point(LightWallet::sync_point_of(block));
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
        let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
        let start = wallet.state().sync.map_or(0, |s| s.height + 1);
        let end = tip.min(start.saturating_add(SYNC_STEP - 1));
        let (mut received, mut spent) = (0, 0);
        let mut from = start;
        while from <= end {
            let to = end.min(from + BATCH - 1);
            let report = wallet.apply_blocks(blocks(node, from, to)?)?;
            received += report.received;
            spent += report.spent;
            from = to + 1;
        }
        self.save_state(network_id, &wallet)?;
        let height = wallet.state().sync.map(|s| s.height);
        Ok(json!({
            "synced_height": height, "tip": tip, "received": received, "spent": spent,
            "done": height.is_some_and(|h| h >= tip),
        }))
    }

    /// Pays `amount` NPT (plus `fee`) to `to`: membership proofs from the
    /// node, the transaction built and proven here, then submitted. The wallet
    /// must be synced to the node's tip first.
    pub fn send(&self, network_id: &str, network: Network, to: &str, amount: &str, fee: &str, now_ms: u64, node: &dyn Node) -> Result<Value> {
        // Catch up first: a coin spent in a block the wallet has not seen
        // would make the node refuse the transaction.
        let mut wallet = self.load(network_id, network)?.context("no wallet")?;
        let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
        if tip.saturating_sub(wallet.state().sync.map_or(0, |s| s.height)) > SYNC_STEP {
            bail!("the wallet is far behind the node; sync it first");
        }
        let start = wallet.state().sync.map_or(0, |s| s.height + 1);
        let mut from = start;
        while from <= tip {
            let to = tip.min(from + BATCH - 1);
            wallet.apply_blocks(blocks(node, from, to)?)?;
            from = to + 1;
        }
        let plan = wallet.plan_spend(amount, fee, now_ms)?;
        let snapshot = rpc::parse_membership_snapshot(
            &node.call(rpc::restore_membership_proof_request(plan.index_sets.clone()))?,
        )?;
        let (details, built) = wallet.build_payment(to, &plan, snapshot, now_ms)?;
        let transaction = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
            .block_on(light_core::send::prove(&details))?;
        let rpc_transaction = light_core::neptune_rpc_api::model::wallet::transaction::RpcTransaction::try_from(transaction)
            .map_err(|e| anyhow::anyhow!(e))?;
        if !rpc::parse_submitted(&node.call(rpc::submit_transaction_request(rpc_transaction))?)? {
            bail!("the node did not accept the transaction");
        }
        // The change key index moved on; the spent coins show as spent once
        // a block includes the transaction.
        self.save_state(network_id, &wallet)?;
        Ok(json!({ "sent": amount.trim(), "fee": fee.trim(), "inputs": built.inputs, "change": built.change }))
    }

    pub fn balance(&self, network_id: &str, network: Network, now_ms: u64) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        Ok(json!({
            "balance": wallet.balance(now_ms),
            "synced_height": wallet.state().sync.map(|s| s.height),
        }))
    }
}

fn tip_block(node: &dyn Node) -> Result<light_core::neptune_rpc_api::model::wallet::block::RpcWalletBlock> {
    let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
    blocks(node, tip, tip)?.into_iter().next().context("the node did not return its tip")
}

fn blocks(node: &dyn Node, from: u64, to: u64) -> Result<Vec<light_core::neptune_rpc_api::model::wallet::block::RpcWalletBlock>> {
    rpc::parse_blocks(&node.call(rpc::get_blocks_request(from, to))?)
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
    struct FixtureNode(String);

    impl FixtureNode {
        fn new() -> Self {
            let gz: &[u8] = include_bytes!("../../light-core/fixtures/main-parent.json.gz");
            let mut reply = String::new();
            flate2::read::GzDecoder::new(gz).read_to_string(&mut reply).unwrap();
            Self(reply)
        }
    }

    impl Node for FixtureNode {
        fn call(&self, body: String) -> Result<String> {
            let v: Value = serde_json::from_str(&body)?;
            match v["method"].as_str() {
                Some("chain_height") => Ok(r#"{"jsonrpc":"2.0","id":1,"result":{"height":62980}}"#.into()),
                Some("wallet_getBlocks") if v["params"] == json!([62980, 62980]) => Ok(self.0.clone()),
                Some("wallet_getBlocks") => Ok(r#"{"jsonrpc":"2.0","id":1,"result":{"blocks":[]}}"#.into()),
                other => bail!("unexpected call {other:?}"),
            }
        }
    }

    fn temp_root() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("neptune-provider-test-{}-{}", std::process::id(), now_nanos()));
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

        let status = wallets.status("neptune-main", Network::Main).unwrap();
        assert_eq!(status["synced_height"], 62980);
        assert_eq!(status["address"], created["address"]);
        // Synced to the tip already: nothing to apply, balance zero.
        let synced = wallets.sync("neptune-main", Network::Main, &node).unwrap();
        assert_eq!(synced["done"], true);
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
}
