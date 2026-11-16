//! Paying from the wallet: choose coins, unlock them, build the transaction and
//! its proof. The caller fetches the coins' membership proofs from its node
//! (`wallet_restoreMembershipProof`) and submits the result.
//!
//! Asking a node for membership proofs tells it which coins are being spent,
//! so this is meant for the user's own node (the provider's loopback node).

use anyhow::{bail, ensure, Context, Result};
use neptune_consensus::consensus_rule_set::ConsensusRuleSet;
use neptune_consensus::transaction::validity::proof_collection::ProofCollection;
use neptune_consensus::transaction::{Transaction, TransactionProof};
use neptune_consensus::type_scripts::native_currency_amount::NativeCurrencyAmount;
use neptune_mutator_set::mutator_set_accumulator::MutatorSetAccumulator;
use neptune_mutator_set::removal_record::absolute_index_set::AbsoluteIndexSet;
use neptune_primitives::block_height::BlockHeight;
use neptune_primitives::timestamp::Timestamp;
use neptune_rpc_api::model::wallet::mutator_set::RpcMsMembershipSnapshot;
use neptune_wallet::address::{ReceivingAddress, SpendingKey};
use neptune_wallet::transaction_details::TransactionDetails;
use neptune_wallet::transaction_output::TxOutput;
use neptune_wallet::unlocked_utxo::UnlockedUtxo;
use serde::Serialize;

use crate::{index_set_of, Coin, LightWallet, KEY_GAP, SCANNED_KEY_TYPES};

/// The coins a payment spends, and what the node must prove membership for.
pub struct SpendPlan {
    /// Positions in the wallet's coin list.
    pub coins: Vec<usize>,
    pub index_sets: Vec<AbsoluteIndexSet>,
    pub amount: NativeCurrencyAmount,
    pub fee: NativeCurrencyAmount,
}

#[derive(Clone, Debug, Serialize)]
pub struct Built {
    pub inputs: usize,
    pub change: String,
}

pub fn parse_amount(text: &str) -> Result<NativeCurrencyAmount> {
    let amount = NativeCurrencyAmount::coins_from_str(text.trim()).context("not an NPT amount")?;
    ensure!(!amount.is_negative(), "amounts cannot be negative");
    Ok(amount)
}

impl LightWallet {
    /// Picks unspent coins, largest first, that are spendable at `now_ms`
    /// and together cover `amount + fee`.
    pub fn plan_spend(&self, amount: &str, fee: &str, now_ms: u64) -> Result<SpendPlan> {
        let (amount, fee) = (parse_amount(amount)?, parse_amount(fee)?);
        ensure!(amount.is_positive(), "send a positive amount");
        let needed = amount + fee;
        let now = Timestamp::millis(now_ms);
        let mut candidates: Vec<(usize, &Coin)> = self
            .state
            .coins
            .iter()
            .enumerate()
            .filter(|(_, c)| c.spent_height.is_none() && c.utxo.can_spend_at(now))
            .collect();
        candidates.sort_by_key(|(_, c)| std::cmp::Reverse(c.utxo.get_native_currency_amount()));
        let mut chosen = vec![];
        let mut total = NativeCurrencyAmount::coins(0);
        for (i, coin) in candidates {
            if total >= needed {
                break;
            }
            total = total + coin.utxo.get_native_currency_amount();
            chosen.push(i);
        }
        if total < needed {
            bail!("not enough spendable NPT: {} available, {} needed", crate::npt(total), crate::npt(needed));
        }
        let index_sets = chosen.iter().map(|&i| index_set_of(&self.state.coins[i])).collect();
        Ok(SpendPlan { coins: chosen, index_sets, amount, fee })
    }

    fn spending_key(&self, coin: &Coin) -> Result<SpendingKey> {
        let key_type = SCANNED_KEY_TYPES
            .into_iter()
            .find(|t| *t as u8 == coin.key_type)
            .context("coin of an unknown key type")?;
        Ok(self.entropy.nth_spending_key(key_type, coin.key_index))
    }

    /// The transaction for `plan`, paying `to` (bech32m) and returning change
    /// to a fresh change key, against the node's `snapshot` (membership
    /// proofs in the plan's order, and the mutator set they are valid for).
    pub fn build_payment(&mut self, to: &str, plan: &SpendPlan, snapshot: RpcMsMembershipSnapshot, now_ms: u64) -> Result<(TransactionDetails, Built)> {
        let recipient = ReceivingAddress::from_bech32m(to.trim(), self.state.network).context("not an address for this network")?;
        let height = BlockHeight::from(snapshot.synced_height);
        let output = TxOutput::onchain_native_currency(
            plan.amount,
            self.entropy.generate_sender_randomness(height, recipient.privacy_digest()),
            recipient,
            false,
        );
        self.build_spend(plan, snapshot, output, now_ms)
    }

    /// Spends the plan's coins into `output` (worth `plan.amount`), with the
    /// rest, less the fee, as change to the next Symmetric key.
    pub(crate) fn build_spend(&mut self, plan: &SpendPlan, snapshot: RpcMsMembershipSnapshot, output: TxOutput, now_ms: u64) -> Result<(TransactionDetails, Built)> {
        let network = self.state.network;
        ensure!(snapshot.membership_proofs.len() == plan.coins.len(), "the node returned the wrong number of membership proofs");
        ensure!(output.native_currency_amount() == plan.amount, "the output does not carry the planned amount");
        let mutator_set: MutatorSetAccumulator = snapshot.synced_mutator_set.into();
        let height = BlockHeight::from(snapshot.synced_height);

        let mut inputs = vec![];
        let mut total = NativeCurrencyAmount::coins(0);
        for (&i, proof) in plan.coins.iter().zip(snapshot.membership_proofs) {
            let coin = self.state.coins[i].clone();
            let key = self.spending_key(&coin)?;
            let membership = proof
                .extract_ms_membership_proof(coin.aocl_index, coin.sender_randomness, coin.receiver_preimage)
                .context("the node's membership proof does not cover the coin")?;
            ensure!(
                mutator_set.verify(neptune_consensus::tasm_lib::prelude::Tip5::hash(&coin.utxo), &membership),
                "a membership proof from the node does not verify"
            );
            total = total + coin.utxo.get_native_currency_amount();
            inputs.push(UnlockedUtxo::unlock(coin.utxo, key.lock_script_and_witness(), membership));
        }

        let mut outputs = vec![output];
        let change = total + -(plan.amount + plan.fee);
        let mut change_text = "0".to_string();
        if change.is_positive() {
            let index = self.state.next_change;
            let change_key: SpendingKey = self.entropy.nth_symmetric_key(index).into();
            let change_address = change_key.to_address();
            outputs.push(TxOutput::onchain_native_currency_as_change(
                change,
                self.entropy.generate_sender_randomness(height, change_address.privacy_digest()),
                change_address,
            ));
            self.state.next_change += 1;
            // Keep the scanned window ahead of the change keys in use.
            let mut wanted = self.state.derived;
            wanted[1] = wanted[1].max(self.state.next_change + KEY_GAP);
            self.derive_up_to(wanted);
            change_text = crate::npt(change);
        }

        let details = TransactionDetails::new(
            inputs,
            outputs,
            plan.fee,
            None,
            Timestamp::millis(now_ms),
            mutator_set,
            network,
        );
        Ok((details, Built { inputs: plan.coins.len(), change: change_text }))
    }
}

/// The shareable transaction: its kernel and a proof that reveals no secret.
/// Networks with mock proofs (regtest) get a mock proof collection.
pub async fn prove(details: &TransactionDetails) -> Result<Transaction> {
    details.validate().await.map_err(|e| anyhow::anyhow!("the transaction is not valid: {e:?}"))?;
    let witness = details.primitive_witness();
    let network = details.network;
    if !network.use_mock_proof() {
        bail!("proving on {network} is not built yet (ProofCollection with the Triton VM prover)");
    }
    let _ = ConsensusRuleSet::infer_from(network, BlockHeight::from(0u64));
    let proof = ProofCollection::produce_mock(&witness, true);
    Ok(Transaction { kernel: witness.kernel, proof: TransactionProof::ProofCollection(proof) })
}
