//! The Neptune leg of swaps, per network: HTLC locks this wallet gave (with
//! their secrets) and the swap keys it handed out.
//!
//! Stored in `<network>/swaps.json`, owner-only (0600) like the phrase: it
//! holds the secrets of open locks. **Plain text: development only.**

use std::fs;
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use anyhow::{bail, ensure, Context, Result};
use light_core::neptune_primitives::network::Network;
use light_core::rpc;
use light_core::swap::{hex, hex_bytes, locate, new_secret, HtlcNote, HtlcSpend, Located};
use light_core::LightWallet;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use super::{blocks, catch_up, prove_and_submit, Node, Wallets, BATCH};

/// A lock this wallet gave.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Lock {
    id: String,
    /// The 32-byte secret, hex. The NPT giver holds it and reveals it by
    /// claiming the other chain.
    secret: String,
    note: HtlcNote,
    refund_index: u64,
    /// The node's tip when the lock was sent: the lock lands after it.
    sent_at_height: u64,
    located: Option<Located>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Book {
    /// The next swap key index (claim and refund keys share the sequence).
    next_index: u64,
    locks: Vec<Lock>,
}

impl Wallets {
    fn book(&self, network_id: &str) -> Result<Book> {
        match fs::read(self.dir(network_id)?.join("swaps.json")) {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes)?),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Book::default()),
            Err(e) => Err(e.into()),
        }
    }

    fn save_book(&self, network_id: &str, book: &Book) -> Result<()> {
        let dir = self.dir(network_id)?;
        let tmp = dir.join("swaps.json.tmp");
        let mut file = fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&tmp)?;
        fs::set_permissions(&tmp, fs::Permissions::from_mode(0o600))?;
        file.write_all(&serde_json::to_vec(book)?)?;
        file.sync_all()?;
        fs::rename(tmp, dir.join("swaps.json"))?;
        Ok(())
    }

    fn take_index(&self, network_id: &str, book: &mut Book) -> Result<u64> {
        let index = book.next_index;
        book.next_index += 1;
        self.save_book(network_id, book)?;
        Ok(index)
    }

    /// A fresh swap key: its lock goes to the counterparty as this wallet's
    /// claim key.
    pub fn htlc_claim_lock(&self, network_id: &str, network: Network) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        let mut book = self.book(network_id)?;
        let index = self.take_index(network_id, &mut book)?;
        Ok(json!({ "index": index, "claim_lock": wallet.swap_lock(index).to_hex() }))
    }

    /// Locks `amount` NPT for the holder of `claim_lock`, refundable to this
    /// wallet from `refund_after_ms`. The secret is made here: the NPT giver
    /// holds it.
    pub fn htlc_lock(
        &self,
        network_id: &str,
        network: Network,
        amount: &str,
        fee: &str,
        claim_lock: &str,
        refund_after_ms: u64,
        now_ms: u64,
        node: &dyn Node,
    ) -> Result<Value> {
        ensure!(refund_after_ms > now_ms, "the refund time must be in the future");
        let claim_lock = light_core::neptune_wallet::twenty_first::prelude::Digest::try_from_hex(claim_lock.trim())
            .context("the claim lock is not a digest")?;
        let mut wallet = self.load(network_id, network)?.context("no wallet")?;
        let tip = catch_up(&mut wallet, node)?;
        let plan = wallet.plan_spend(amount, fee, now_ms)?;
        let snapshot = rpc::parse_membership_snapshot(
            &node.call(rpc::restore_membership_proof_request(plan.index_sets.clone()))?,
        )?;
        let mut book = self.book(network_id)?;
        let refund_index = self.take_index(network_id, &mut book)?;
        let (secret, hash_lock) = new_secret();
        let (details, note) =
            wallet.build_htlc_lock(&plan, hash_lock, refund_after_ms, claim_lock, refund_index, snapshot, now_ms)?;
        // Keep the secret before anything is sent: a lock whose secret was
        // lost could only be refunded.
        let id = hex(&hash_lock)[..16].to_string();
        book.locks.push(Lock { id: id.clone(), secret: hex(&secret), note: note.clone(), refund_index, sent_at_height: tip, located: None });
        self.save_book(network_id, &book)?;
        prove_and_submit(&details, node)?;
        self.save_state(network_id, &wallet)?;
        Ok(json!({ "swap_id": id, "hash_lock": hex(&hash_lock), "note": note }))
    }

    /// The secret of a lock this wallet gave.
    pub fn htlc_secret(&self, network_id: &str, swap_id: &str) -> Result<Value> {
        let book = self.book(network_id)?;
        let lock = book.locks.iter().find(|l| l.id == swap_id).context("no such swap")?;
        Ok(json!({ "secret": lock.secret }))
    }

    /// Where a lock is on chain: this wallet's own (by id) or a note from a
    /// counterparty, searched from `from_height`.
    pub fn htlc_status(&self, network_id: &str, swap_id: Option<&str>, note: Option<HtlcNote>, from_height: u64, node: &dyn Node) -> Result<Value> {
        let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
        let located = match (swap_id, note) {
            (Some(id), _) => self.locate_own(network_id, id, node)?,
            (None, Some(note)) => {
                note.check()?;
                find(&note, from_height, tip, node)?
            }
            (None, None) => bail!("name a swap_id or give a note"),
        };
        Ok(match located {
            Some(at) => json!({ "found": true, "height": at.height, "aocl_index": at.aocl_index, "depth": tip.saturating_sub(at.height) + 1, "tip": tip }),
            None => json!({ "found": false, "tip": tip }),
        })
    }

    fn locate_own(&self, network_id: &str, swap_id: &str, node: &dyn Node) -> Result<Option<Located>> {
        let mut book = self.book(network_id)?;
        let lock = book.locks.iter_mut().find(|l| l.id == swap_id).context("no such swap")?;
        if lock.located.is_none() {
            let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
            lock.located = find(&lock.note, lock.sent_at_height, tip, node)?;
            if lock.located.is_some() {
                self.save_book(network_id, &book)?;
            }
        }
        Ok(book.locks.iter().find(|l| l.id == swap_id).and_then(|l| l.located))
    }

    /// Claims a counterparty's lock with the revealed secret and this
    /// wallet's swap key `claim_index`, to this wallet's address.
    #[allow(clippy::too_many_arguments)]
    pub fn htlc_claim(
        &self,
        network_id: &str,
        network: Network,
        note: HtlcNote,
        preimage: &str,
        claim_index: u64,
        fee: &str,
        from_height: u64,
        now_ms: u64,
        node: &dyn Node,
    ) -> Result<Value> {
        let preimage: [u8; 32] = hex_bytes(preimage)?.try_into().map_err(|_| anyhow::anyhow!("the secret is 32 bytes"))?;
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        note.check()?;
        let tip = rpc::parse_height(&node.call(rpc::height_request())?)?;
        let at = find(&note, from_height, tip, node)?.context("the lock is not on chain (yet)")?;
        self.spend(&wallet, &note, at, HtlcSpend::Claim { preimage, claim_index }, fee, now_ms, node)?;
        Ok(json!({ "claimed": note.amount, "fee": fee.trim(), "lock_height": at.height }))
    }

    /// Takes a lock this wallet gave back, from its refund time on.
    pub fn htlc_refund(&self, network_id: &str, network: Network, swap_id: &str, fee: &str, now_ms: u64, node: &dyn Node) -> Result<Value> {
        let wallet = self.load(network_id, network)?.context("no wallet")?;
        let at = self.locate_own(network_id, swap_id, node)?.context("the lock is not on chain (yet)")?;
        let book = self.book(network_id)?;
        let lock = book.locks.iter().find(|l| l.id == swap_id).context("no such swap")?;
        self.spend(&wallet, &lock.note, at, HtlcSpend::Refund { refund_index: lock.refund_index }, fee, now_ms, node)?;
        Ok(json!({ "refunded": lock.note.amount, "fee": fee.trim(), "lock_height": at.height }))
    }

    #[allow(clippy::too_many_arguments)]
    fn spend(&self, wallet: &LightWallet, note: &HtlcNote, at: Located, spend: HtlcSpend, fee: &str, now_ms: u64, node: &dyn Node) -> Result<()> {
        let snapshot = rpc::parse_membership_snapshot(
            &node.call(rpc::restore_membership_proof_request(vec![LightWallet::htlc_index_set(note, at)]))?,
        )?;
        let details = wallet.build_htlc_spend(note, at, spend, fee, snapshot, now_ms)?;
        prove_and_submit(&details, node)
    }
}

/// Searches blocks `from..=tip` for the note's coin.
fn find(note: &HtlcNote, from: u64, tip: u64, node: &dyn Node) -> Result<Option<Located>> {
    let mut start = from;
    while start <= tip {
        let end = tip.min(start + BATCH - 1);
        if let Some(at) = locate(note, &blocks(node, start, end)?) {
            return Ok(Some(at));
        }
        start = end + 1;
    }
    Ok(None)
}
