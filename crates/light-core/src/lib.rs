//! Neptune light wallet core for the ElastOS capsule spike.
//!
//! Finds a seed's coins in blocks fetched from a neptune-core node and works out
//! the balance. It does no I/O: the caller fetches with [`rpc`] and feeds the
//! blocks to [`LightWallet::apply_blocks`]. The same code runs natively
//! (light-cli) and in the browser (light-wasm).
//!
//! What it checks: blocks connect by height and `prev_block_digest`, and every
//! coin it adopts is an addition record of that block. What it does not check:
//! block proofs, proof of work, or which chain is canonical. It trusts the node
//! for those, like neptune-wallet-app does.

pub mod rpc;
pub mod send;
pub mod swap;
pub mod tip;

use std::collections::HashMap;

use anyhow::bail;
use anyhow::ensure;
use anyhow::Result;
use neptune_consensus::block::block_kernel::BlockKernel;
use neptune_consensus::transaction::utxo::Utxo;
use neptune_consensus::type_scripts::native_currency_amount::NativeCurrencyAmount;
use neptune_mutator_set::removal_record::absolute_index_set::AbsoluteIndexSet;
use neptune_primitives::announcement_flag::AnnouncementFlag;
use neptune_primitives::network::Network;
use neptune_primitives::timestamp::Timestamp;
use neptune_rpc_api::model::wallet::block::RpcWalletBlock;
use neptune_wallet::address::KeyType;
use neptune_wallet::address::ReceivingAddress;
use neptune_wallet::address::SpendingKey;
use neptune_wallet::secret_key_material::SecretKeyMaterial;
use neptune_wallet::wallet_entropy::WalletEntropy;
use serde::Deserialize;
use serde::Serialize;
use tasm_lib::prelude::Digest;
use tasm_lib::prelude::Tip5;

pub use neptune_primitives;
pub use neptune_rpc_api;
pub use neptune_wallet;

/// Keys derived past the highest one that has received a coin, per key type.
/// neptune-wallet-app uses the same look-ahead.
pub const KEY_GAP: u64 = 25;

/// The key types a seed receives on: Generation (the default address),
/// Symmetric (change) and EC Hybrid (short addresses).
pub(crate) const SCANNED_KEY_TYPES: [KeyType; 3] = [KeyType::Generation, KeyType::Symmetric, KeyType::EcHybrid];

/// A coin the seed owns, with what it takes to recognise its spend.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Coin {
    pub utxo: Utxo,
    pub sender_randomness: Digest,
    pub receiver_preimage: Digest,
    pub aocl_index: u64,
    /// Tip5 hash of the coin's absolute index set, which its spend publishes.
    pub index_set_hash: Digest,
    pub key_type: u8,
    pub key_index: u64,
    pub received_height: u64,
    pub received_block: Digest,
    pub spent_height: Option<u64>,
}

/// The last block applied.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SyncPoint {
    pub height: u64,
    pub hash: Digest,
}

/// Everything about the wallet except the seed. Contains which coins are the
/// seed's, so it is private, but it cannot spend anything.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WalletState {
    pub network: Network,
    pub sync: Option<SyncPoint>,
    pub coins: Vec<Coin>,
    /// Number of keys derived per key type (indexed like `SCANNED_KEY_TYPES`).
    pub derived: [u64; 3],
    /// The next Symmetric key index to send change to.
    #[serde(default)]
    pub next_change: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Balance {
    /// Unspent coins, in NPT (lossless decimal string).
    pub total: String,
    /// The part of `total` whose time-lock has passed at the given time.
    pub spendable: String,
    pub total_nau: String,
    pub unspent_coins: usize,
    pub spent_coins: usize,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ApplyReport {
    pub blocks: u64,
    pub announcements: u64,
    pub inputs: u64,
    pub received: u64,
    pub spent: u64,
    pub height: Option<u64>,
}

struct DerivedKey {
    key_type_slot: usize,
    index: u64,
    key: SpendingKey,
}

pub struct LightWallet {
    entropy: WalletEntropy,
    state: WalletState,
    keys: Vec<DerivedKey>,
    /// `index_set_hash` -> position in `state.coins`.
    by_index_set: HashMap<Digest, usize>,
}

/// A new 18-word seed phrase from the platform's random source.
pub fn new_phrase() -> Vec<String> {
    let secret: [u8; 24] = rand::random();
    // Same layout as SecretKeyMaterial::from_phrase: three little-endian u64s.
    let words = secret
        .chunks(8)
        .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
        .collect::<Vec<_>>();
    let xfe = tasm_lib::triton_vm::prelude::XFieldElement::new([
        words[0].into(),
        words[1].into(),
        words[2].into(),
    ]);
    SecretKeyMaterial(xfe).to_phrase()
}

pub fn parse_network(name: &str) -> Result<Network> {
    Ok(match name {
        "main" | "mainnet" => Network::Main,
        "testnet" => Network::Testnet(0),
        // Mock proofs and proof of work: local development only.
        "regtest" => Network::RegTest,
        other => bail!("unknown network {other}"),
    })
}

impl LightWallet {
    pub fn from_phrase(phrase: &str, network: Network) -> Result<Self> {
        let words: Vec<&str> = phrase.split_whitespace().collect();
        ensure!(words.len() == 18, "a Neptune seed phrase has 18 words, got {}", words.len());
        let entropy = WalletEntropy::from_phrase(words)?;
        let state = WalletState { network, sync: None, coins: vec![], derived: [0; 3], next_change: 0 };
        Self::with_state(entropy, state)
    }

    /// Resume from a saved state; the phrase must be the one it was made with.
    pub fn resume(phrase: &str, state: WalletState) -> Result<Self> {
        let mut w = Self::from_phrase(phrase, state.network)?;
        let derived = state.derived;
        w.state = state;
        w.state.derived = [0; 3];
        w.keys.clear();
        w.derive_up_to(derived);
        w.reindex();
        Ok(w)
    }

    fn with_state(entropy: WalletEntropy, state: WalletState) -> Result<Self> {
        let mut w = Self { entropy, state, keys: vec![], by_index_set: HashMap::new() };
        w.derive_up_to([KEY_GAP; 3]);
        Ok(w)
    }

    pub(crate) fn derive_up_to(&mut self, wanted: [u64; 3]) {
        for (slot, key_type) in SCANNED_KEY_TYPES.into_iter().enumerate() {
            while self.state.derived[slot] < wanted[slot] {
                let index = self.state.derived[slot];
                let key = self.entropy.nth_spending_key(key_type, index);
                self.keys.push(DerivedKey { key_type_slot: slot, index, key });
                self.state.derived[slot] += 1;
            }
        }
    }

    fn reindex(&mut self) {
        self.by_index_set =
            self.state.coins.iter().enumerate().map(|(i, c)| (c.index_set_hash, i)).collect();
    }

    /// The sync point that `block` is: start syncing after it.
    pub fn sync_point_of(block: &RpcWalletBlock) -> SyncPoint {
        let kernel: BlockKernel = block.kernel.clone().into();
        SyncPoint { height: kernel.header.height.into(), hash: block.hash() }
    }

    pub fn state(&self) -> &WalletState {
        &self.state
    }

    pub fn network(&self) -> Network {
        self.state.network
    }

    /// The default receiving address (Generation key 0), bech32m.
    pub fn receiving_address(&self) -> Result<String> {
        let address: ReceivingAddress = self.entropy.nth_generation_spending_key(0).to_address().into();
        address.to_bech32m(self.state.network)
    }

    /// Start syncing after `point` instead of from genesis (a new wallet
    /// starts at the tip; a restore at a block before its first payment).
    pub fn set_sync_point(&mut self, point: SyncPoint) {
        self.state.sync = Some(point);
    }

    /// The flags fast restore sends to the node's UTXO index. Revealing them
    /// tells the node which announcements are the seed's.
    pub fn announcement_flags(&self) -> Vec<AnnouncementFlag> {
        self.keys
            .iter()
            .map(|k| AnnouncementFlag {
                flag: u64::from(SCANNED_KEY_TYPES[k.key_type_slot] as u8).into(),
                receiver_id: k.key.receiver_identifier(),
            })
            .collect()
    }

    /// Index sets of unspent coins, for asking the UTXO index where they were
    /// spent (fast restore only; reveals the coins).
    pub fn unspent_index_sets(&self) -> Vec<AbsoluteIndexSet> {
        self.state.coins.iter().filter(|c| c.spent_height.is_none()).map(index_set_of).collect()
    }

    /// Apply consecutive blocks after the sync point.
    pub fn apply_blocks(&mut self, blocks: Vec<RpcWalletBlock>) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for block in blocks {
            self.apply_block(block, true, &mut report)?;
        }
        report.height = self.state.sync.map(|s| s.height);
        Ok(report)
    }

    /// Apply blocks out of order, as fast restore fetches them. Does not move
    /// the sync point.
    pub fn apply_unordered_blocks(&mut self, blocks: Vec<RpcWalletBlock>) -> Result<ApplyReport> {
        let mut report = ApplyReport::default();
        for block in blocks {
            self.apply_block(block, false, &mut report)?;
        }
        report.height = self.state.sync.map(|s| s.height);
        Ok(report)
    }

    fn apply_block(&mut self, block: RpcWalletBlock, in_order: bool, report: &mut ApplyReport) -> Result<()> {
        let hash = block.hash();
        let kernel: BlockKernel = block.kernel.into();
        let height: u64 = kernel.header.height.into();

        if in_order {
            if let Some(sync) = self.state.sync {
                ensure!(
                    height == sync.height + 1,
                    "expected block {} after {}, got {height}",
                    sync.height + 1,
                    sync.height
                );
                ensure!(
                    kernel.header.prev_block_digest == sync.hash,
                    "block {height} does not follow block {} (reorg or wrong node)",
                    sync.height
                );
            } else {
                ensure!(height == 0, "a wallet without a sync point starts at genesis, got {height}");
            }
        }

        let tx = &kernel.body.transaction_kernel;
        report.blocks += 1;
        report.announcements += tx.announcements.len() as u64;
        report.inputs += tx.inputs.len() as u64;

        // Spends first: a coin received and spent in one block is still found
        // below, then marked spent by the second pass.
        report.spent += self.mark_spends(tx.inputs.iter().map(|rr| &rr.absolute_indices), height);

        let aocl_prior = kernel.body.num_aocl_leafs_prior();
        let mut extended = true;
        while extended {
            extended = false;
            let mut grow = self.state.derived;
            for k in &self.keys {
                for incoming in k.key.scan_for_announced_utxos(tx) {
                    // Adopt only coins that really are outputs of this block.
                    let record = incoming.addition_record();
                    let Some(position) = tx.outputs.iter().position(|o| *o == record) else {
                        continue;
                    };
                    let aocl_index = aocl_prior + position as u64;
                    let item = Tip5::hash(&incoming.utxo);
                    let index_set = AbsoluteIndexSet::compute(
                        item,
                        incoming.sender_randomness,
                        incoming.receiver_preimage,
                        aocl_index,
                    );
                    let index_set_hash = Tip5::hash(&index_set);
                    if self.by_index_set.contains_key(&index_set_hash) {
                        continue;
                    }
                    self.state.coins.push(Coin {
                        utxo: incoming.utxo,
                        sender_randomness: incoming.sender_randomness,
                        receiver_preimage: incoming.receiver_preimage,
                        aocl_index,
                        index_set_hash,
                        key_type: SCANNED_KEY_TYPES[k.key_type_slot] as u8,
                        key_index: k.index,
                        received_height: height,
                        received_block: hash,
                        spent_height: None,
                    });
                    self.by_index_set.insert(index_set_hash, self.state.coins.len() - 1);
                    report.received += 1;
                    let slot = k.key_type_slot;
                    grow[slot] = grow[slot].max(k.index + 1 + KEY_GAP);
                }
            }
            if grow != self.state.derived {
                // A key near the end of the window received: derive more and
                // scan this block again with them.
                self.derive_up_to(grow);
                extended = true;
            }
        }
        report.spent += self.mark_spends(tx.inputs.iter().map(|rr| &rr.absolute_indices), height);

        if in_order {
            self.state.sync = Some(SyncPoint { height, hash });
        }
        Ok(())
    }

    fn mark_spends<'a>(&mut self, index_sets: impl Iterator<Item = &'a AbsoluteIndexSet>, height: u64) -> u64 {
        let mut spent = 0;
        for set in index_sets {
            if let Some(&i) = self.by_index_set.get(&Tip5::hash(set)) {
                let coin = &mut self.state.coins[i];
                if coin.spent_height.is_none() {
                    coin.spent_height = Some(height);
                    spent += 1;
                }
            }
        }
        spent
    }

    pub fn balance(&self, now_ms: u64) -> Balance {
        let now = Timestamp::millis(now_ms);
        let unspent: Vec<&Coin> = self.state.coins.iter().filter(|c| c.spent_height.is_none()).collect();
        let total: NativeCurrencyAmount = unspent.iter().map(|c| c.utxo.get_native_currency_amount()).sum();
        let spendable: NativeCurrencyAmount = unspent
            .iter()
            .filter(|c| c.utxo.can_spend_at(now))
            .map(|c| c.utxo.get_native_currency_amount())
            .sum();
        Balance {
            total: npt(total),
            spendable: npt(spendable),
            total_nau: total.to_nau().to_string(),
            unspent_coins: unspent.len(),
            spent_coins: self.state.coins.len() - unspent.len(),
        }
    }
}

/// An amount in NPT without trailing zeros: "14.95", "25", "0".
pub fn npt(amount: NativeCurrencyAmount) -> String {
    let text = amount.display_lossless();
    if !text.contains('.') {
        return text;
    }
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

pub(crate) fn index_set_of(coin: &Coin) -> AbsoluteIndexSet {
    AbsoluteIndexSet::compute(
        Tip5::hash(&coin.utxo),
        coin.sender_randomness,
        coin.receiver_preimage,
        coin.aocl_index,
    )
}

#[cfg(test)]
mod tests;
#[cfg(test)]
mod tip_tests;
#[cfg(test)]
mod swap_tests;
