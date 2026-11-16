//! Addresses and activity.
//!
//! Addresses: the wallet hands out one current address per kind and a fresh
//! one on request; every earlier one keeps receiving, because the scan window
//! stays `KEY_GAP` keys ahead of the newest issued.
//!
//! Activity comes from the coins themselves: a coin on a Generation or EC
//! Hybrid key is a receipt; coins spent in one block, less the change that came
//! back in that block, are a payment (fee included). Payments submitted but not
//! yet in a block are kept as pending until their coins show up spent.

use anyhow::Result;
use neptune_consensus::type_scripts::native_currency_amount::NativeCurrencyAmount;
use neptune_wallet::address::{KeyType, ReceivingAddress};
use serde::{Deserialize, Serialize};
use tasm_lib::prelude::Digest;

use crate::send::SpendPlan;
use crate::{npt, LightWallet, KEY_GAP};

/// The kinds of receiving address the wallet hands out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AddressKind {
    /// Lattice-based, the most private; thousands of characters.
    Generation,
    /// Short (a QR code fits); whoever knows it sees what it receives.
    EcHybrid,
}

impl AddressKind {
    fn key_type(self) -> KeyType {
        match self {
            AddressKind::Generation => KeyType::Generation,
            AddressKind::EcHybrid => KeyType::EcHybrid,
        }
    }

    /// Position in `WalletState::issued`.
    fn issued_slot(self) -> usize {
        match self {
            AddressKind::Generation => 0,
            AddressKind::EcHybrid => 1,
        }
    }

    /// Position in `WalletState::derived` (scan order Generation, Symmetric, EcHybrid).
    fn derived_slot(self) -> usize {
        match self {
            AddressKind::Generation => 0,
            AddressKind::EcHybrid => 2,
        }
    }

    pub fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "generation" => AddressKind::Generation,
            "ec_hybrid" => AddressKind::EcHybrid,
            other => anyhow::bail!("unknown address kind {other}"),
        })
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct IssuedAddress {
    pub index: u64,
    pub address: String,
}

/// A payment submitted to the node and not yet seen in a block.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PendingSend {
    /// What leaves the wallet: amount plus fee, NPT.
    pub amount: String,
    /// The coins it spends (their index-set hashes).
    pub coins: Vec<Digest>,
    pub time_ms: u64,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct HistoryItem {
    /// "in" or "out".
    pub direction: &'static str,
    pub amount: String,
    pub height: Option<u64>,
    pub time_ms: Option<u64>,
    pub pending: bool,
    /// For receipts: "generation" or "ec_hybrid".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<&'static str>,
}

impl LightWallet {
    /// The address of `kind` at key `index`, bech32m.
    pub fn address(&self, kind: AddressKind, index: u64) -> Result<String> {
        let key = self.entropy.nth_spending_key(kind.key_type(), index);
        let address: ReceivingAddress = key.to_address();
        address.to_bech32m(self.state.network)
    }

    /// The address of `kind` currently handed out.
    pub fn current_address(&self, kind: AddressKind) -> Result<IssuedAddress> {
        let index = self.state.issued[kind.issued_slot()];
        Ok(IssuedAddress { index, address: self.address(kind, index)? })
    }

    /// Hands out the next unused address of `kind`; the scan window moves with it.
    pub fn new_address(&mut self, kind: AddressKind) -> Result<IssuedAddress> {
        let used_up_to = self
            .state
            .coins
            .iter()
            .filter(|c| c.key_type == kind.key_type() as u8)
            .map(|c| c.key_index + 1)
            .max()
            .unwrap_or(0);
        let slot = kind.issued_slot();
        self.state.issued[slot] = (self.state.issued[slot] + 1).max(used_up_to);
        let mut wanted = self.state.derived;
        let derived = kind.derived_slot();
        wanted[derived] = wanted[derived].max(self.state.issued[slot] + 1 + KEY_GAP);
        self.derive_up_to(wanted);
        self.current_address(kind)
    }

    /// Records a submitted payment as pending.
    pub fn record_pending(&mut self, plan: &SpendPlan, now_ms: u64) {
        let pending = self.pending_for(plan, now_ms);
        self.add_pending(pending);
    }

    /// The pending record a payment of `plan` will get once it is submitted
    /// (kept aside while it is proven).
    pub fn pending_for(&self, plan: &SpendPlan, now_ms: u64) -> PendingSend {
        let coins = plan.coins.iter().map(|&i| self.state.coins[i].index_set_hash).collect();
        PendingSend { amount: npt(plan.amount + plan.fee), coins, time_ms: now_ms }
    }

    pub fn add_pending(&mut self, pending: PendingSend) {
        if !self.state.pending.contains(&pending) {
            self.state.pending.push(pending);
        }
    }

    /// Drops pending payments whose coins a block has spent: they show as sent.
    pub(crate) fn prune_pending(&mut self) {
        let by_index_set = &self.by_index_set;
        let coins = &self.state.coins;
        self.state.pending.retain(|p| {
            !p.coins.iter().any(|h| by_index_set.get(h).is_some_and(|&i| coins[i].spent_height.is_some()))
        });
    }

    /// Every payment, newest first: pending ones, then by block.
    pub fn history(&self) -> Vec<HistoryItem> {
        let symmetric = KeyType::Symmetric as u8;
        let coins = &self.state.coins;
        let spent_at = |h: u64| coins.iter().any(|c| c.spent_height == Some(h));
        let mut items: Vec<HistoryItem> = coins
            .iter()
            // Change comes back on a Symmetric key in the block that spends.
            .filter(|c| !(c.key_type == symmetric && spent_at(c.received_height)))
            .map(|c| HistoryItem {
                direction: "in",
                amount: npt(c.utxo.get_native_currency_amount()),
                height: Some(c.received_height),
                time_ms: (c.received_ms > 0).then_some(c.received_ms),
                pending: false,
                kind: Some(if c.key_type == KeyType::EcHybrid as u8 { "ec_hybrid" } else { "generation" }),
            })
            .collect();

        let mut heights: Vec<u64> = coins.iter().filter_map(|c| c.spent_height).collect();
        heights.sort_unstable();
        heights.dedup();
        for h in heights {
            let spent: NativeCurrencyAmount =
                coins.iter().filter(|c| c.spent_height == Some(h)).map(|c| c.utxo.get_native_currency_amount()).sum();
            let change: NativeCurrencyAmount = coins
                .iter()
                .filter(|c| c.key_type == symmetric && c.received_height == h)
                .map(|c| c.utxo.get_native_currency_amount())
                .sum();
            let time_ms = coins.iter().filter(|c| c.spent_height == Some(h)).find_map(|c| c.spent_ms);
            items.push(HistoryItem {
                direction: "out",
                amount: npt(spent + -change),
                height: Some(h),
                time_ms,
                pending: false,
                kind: None,
            });
        }
        items.sort_by(|a, b| b.height.cmp(&a.height).then(a.direction.cmp(b.direction)));

        let mut pending: Vec<HistoryItem> = self
            .state
            .pending
            .iter()
            .rev()
            .map(|p| HistoryItem {
                direction: "out",
                amount: p.amount.clone(),
                height: None,
                time_ms: Some(p.time_ms),
                pending: true,
                kind: None,
            })
            .collect();
        pending.extend(items);
        pending
    }
}
