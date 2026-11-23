//! The Neptune leg of an atomic swap: lock NPT in the HTLC lock script
//! (`crates/htlc`), claim it with the preimage, or refund it after the timeout.
//!
//! The lock output carries no on-chain notification. Its opening data travels
//! to the counterparty off-chain as an [`HtlcNote`]; nothing on chain ties the
//! output to either party.
//!
//! Claim and refund secrets come from the seed (domain-separated sender
//! randomness at heights no block will reach), so a restored seed can still
//! claim or refund. Only their hashes (the locks) ever leave the wallet.

use anyhow::{bail, ensure, Context, Result};
pub use htlc::hash_lock;
use htlc::{Htlc, Spend};
use neptune_consensus::transaction::lock_script::LockScriptAndWitness;
use neptune_consensus::transaction::primitive_witness::PrimitiveWitness;
use neptune_consensus::transaction::transaction_kernel::TransactionKernelField;
use neptune_consensus::transaction::utxo::{Coin, Utxo};
pub use neptune_mutator_set::addition_record::AdditionRecord;
use neptune_mutator_set::commit;
use neptune_mutator_set::mutator_set_accumulator::MutatorSetAccumulator;
use neptune_mutator_set::removal_record::absolute_index_set::AbsoluteIndexSet;
use neptune_primitives::block_height::BlockHeight;
use neptune_primitives::mast_hash::MastHash;
use neptune_primitives::timestamp::Timestamp;
use neptune_rpc_api::model::wallet::block::RpcWalletBlock;
use neptune_rpc_api::model::wallet::mutator_set::RpcMsMembershipSnapshot;
use neptune_consensus::block::block_kernel::BlockKernel;
use neptune_wallet::address::ReceivingAddress;
use neptune_wallet::transaction_details::TransactionDetails;
use neptune_wallet::transaction_output::TxOutput;
use neptune_wallet::unlocked_utxo::UnlockedUtxo;
use serde::{Deserialize, Serialize};
use tasm_lib::prelude::{Digest, Tip5};

use crate::send::{parse_amount, SpendPlan};
use crate::{npt, LightWallet};

/// Everything the counterparty needs to check and later spend the lock: the
/// terms (which fix the lock script) and the coin's opening data.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct HtlcNote {
    /// SHA-256 of the secret, hex.
    pub hash_lock: String,
    /// Kernel timestamp (ms) from which the refund opens.
    pub refund_after_ms: u64,
    pub claim_lock: String,
    pub refund_lock: String,
    pub amount: String,
    pub utxo: Utxo,
    pub sender_randomness: Digest,
    pub receiver_preimage: Digest,
}

impl HtlcNote {
    pub fn htlc(&self) -> Result<Htlc> {
        let hash_lock: [u8; 32] = hex_bytes(&self.hash_lock)?.try_into().map_err(|_| anyhow::anyhow!("a hash lock is 32 bytes"))?;
        Ok(Htlc {
            hash_lock,
            refund_after: self.refund_after_ms,
            claim_lock: Digest::try_from_hex(&self.claim_lock).context("claim lock")?,
            refund_lock: Digest::try_from_hex(&self.refund_lock).context("refund lock")?,
        })
    }

    pub fn addition_record(&self) -> AdditionRecord {
        commit(Tip5::hash(&self.utxo), self.sender_randomness, self.receiver_preimage.hash())
    }

    /// The note describes exactly the agreed lock: its UTXO is locked by the
    /// HTLC program of these terms and holds exactly one native-currency coin
    /// of `amount` NPT and no other coin. Another coin (under a type script the
    /// receiver's prover cannot satisfy) would make the lock unspendable while
    /// still adding up to the amount. (The full check, with the block, is
    /// swap-guard's.)
    pub fn check(&self) -> Result<()> {
        let htlc = self.htlc()?;
        ensure!(self.utxo.lock_script_hash() == htlc.program().hash(), "the coin is not locked by these terms");
        let expected = Coin::new_native_currency(parse_amount(&self.amount)?);
        ensure!(self.utxo.coins() == [expected], "the lock does not hold exactly one native coin of the stated amount");
        ensure!(!self.utxo.is_timelocked(), "the coin is time-locked");
        Ok(())
    }

    fn index_set(&self, aocl_index: u64) -> AbsoluteIndexSet {
        AbsoluteIndexSet::compute(Tip5::hash(&self.utxo), self.sender_randomness, self.receiver_preimage, aocl_index)
    }
}

/// Where a note's coin landed: its block and position in the AOCL.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Located {
    pub height: u64,
    pub aocl_index: u64,
}

/// Finds the note's coin among `blocks` (as `wallet_getBlocks` returns them).
pub fn locate(note: &HtlcNote, blocks: &[RpcWalletBlock]) -> Option<Located> {
    let record = note.addition_record();
    blocks.iter().find_map(|block| {
        let kernel: BlockKernel = block.kernel.clone().into();
        let outputs = &kernel.body.transaction_kernel.outputs;
        let position = outputs.iter().position(|o| *o == record)?;
        Some(Located {
            height: kernel.header.height.into(),
            aocl_index: kernel.body.num_aocl_leafs_prior() + position as u64,
        })
    })
}

/// The block among `blocks` that spends the note's coin (at `at`), if any: its height, and
/// whether that block also holds one of `ours`, the outputs of this wallet's own claims and
/// refunds (a coin is spent once, so the block holds no other spend of it).
pub fn spent_in(note: &HtlcNote, at: Located, blocks: &[RpcWalletBlock], ours: &[AdditionRecord]) -> Option<(u64, bool)> {
    let index_set = note.index_set(at.aocl_index);
    blocks.iter().find_map(|block| {
        let kernel: BlockKernel = block.kernel.clone().into();
        let tx = &kernel.body.transaction_kernel;
        tx.inputs
            .iter()
            .any(|rr| rr.absolute_indices == index_set)
            .then(|| (kernel.header.height.into(), tx.outputs.iter().any(|o| ours.contains(o))))
    })
}

/// How a lock is spent.
pub enum HtlcSpend {
    /// By the receiver, with the secret's preimage.
    Claim { preimage: [u8; 32], claim_index: u64 },
    /// By the giver, from the refund time on.
    Refund { refund_index: u64 },
}

const SWAP_DOMAIN: &[u8] = b"neptune-intents/htlc-secret/v1";
const SOLVER_DOMAIN: &[u8] = b"neptune-swap/solver-key/v1";

impl LightWallet {
    /// The secret behind swap key `index`. Derived from the seed at a block
    /// height no chain reaches, so it is never some output's sender randomness.
    fn swap_secret(&self, index: u64) -> Digest {
        let domain = Tip5::hash_varlen(&SWAP_DOMAIN.iter().map(|b| u64::from(*b).into()).collect::<Vec<_>>());
        // BlockHeight clamps at the field's largest element: count down from there.
        let height = BlockHeight::from(tasm_lib::triton_vm::prelude::BFieldElement::MAX - index);
        self.entropy.generate_sender_randomness(height, domain)
    }

    /// The seed of this wallet's long-lived solver signing key (Ed25519): the
    /// same seed always gives the same solver DID, so offers and reputation
    /// survive a restore. Its own domain, and heights below the swap keys'.
    pub fn solver_key_seed(&self) -> [u8; 32] {
        use sha2::Digest as _;
        let domain = Tip5::hash_varlen(&SOLVER_DOMAIN.iter().map(|b| u64::from(*b).into()).collect::<Vec<_>>());
        let height = BlockHeight::from(tasm_lib::triton_vm::prelude::BFieldElement::MAX - (1u64 << 40));
        let secret = self.entropy.generate_sender_randomness(height, domain);
        let mut hasher = sha2::Sha256::new();
        hasher.update(SOLVER_DOMAIN);
        for e in secret.values() {
            hasher.update(e.value().to_le_bytes());
        }
        hasher.finalize().into()
    }

    /// The lock (hash of the secret) of swap key `index`: what this wallet
    /// gives the counterparty as its claim or refund key.
    pub fn swap_lock(&self, index: u64) -> Digest {
        htlc::spending_lock(self.swap_secret(index))
    }

    /// Locks `plan.amount` in the HTLC with these terms (the refund lock is
    /// this wallet's swap key `refund_index`), change back to the wallet.
    pub fn build_htlc_lock(
        &mut self,
        plan: &SpendPlan,
        hash_lock: [u8; 32],
        refund_after_ms: u64,
        claim_lock: Digest,
        refund_index: u64,
        basis: crate::send::SpendBasis,
        now_ms: u64,
    ) -> Result<(TransactionDetails, HtlcNote)> {
        let htlc = Htlc { hash_lock, refund_after: refund_after_ms, claim_lock, refund_lock: self.swap_lock(refund_index) };
        let utxo = Utxo::new_native_currency(htlc.program().hash(), plan.amount);
        let receiver_preimage: Digest = rand::random();
        let height = BlockHeight::from(basis.height);
        let sender_randomness = self.entropy.generate_sender_randomness(height, receiver_preimage.hash());
        let note = HtlcNote {
            hash_lock: hex(&hash_lock),
            refund_after_ms,
            claim_lock: claim_lock.to_hex(),
            refund_lock: htlc.refund_lock.to_hex(),
            amount: npt(plan.amount),
            utxo: utxo.clone(),
            sender_randomness,
            receiver_preimage,
        };
        // No on-chain notification: the note goes to the counterparty directly.
        let output = TxOutput::no_notification_as_change(utxo, sender_randomness, receiver_preimage.hash());
        let (details, _) = self.build_spend(plan, basis, output, now_ms)?;
        Ok((details, note))
    }

    /// Spends a located lock to this wallet's receiving address, less `fee`.
    /// Returns the assembled primitive witness (prove it with
    /// `send::prove_witness`): a refund's witness commits to that kernel.
    pub fn build_htlc_spend(
        &self,
        note: &HtlcNote,
        located: Located,
        spend: HtlcSpend,
        fee: &str,
        snapshot: RpcMsMembershipSnapshot,
        now_ms: u64,
    ) -> Result<PrimitiveWitness> {
        note.check()?;
        let htlc = note.htlc()?;
        if let HtlcSpend::Refund { .. } = spend {
            ensure!(now_ms >= htlc.refund_after, "the refund opens at {} (ms)", htlc.refund_after);
        }
        let fee = parse_amount(fee)?;
        let amount = note.utxo.get_native_currency_amount();
        ensure!(amount > fee, "the fee would take the whole lock");
        ensure!(snapshot.membership_proofs.len() == 1, "expected one membership proof");
        let mutator_set: MutatorSetAccumulator = snapshot.synced_mutator_set.clone().into();
        let membership = snapshot.membership_proofs[0]
            .clone()
            .extract_ms_membership_proof(located.aocl_index, note.sender_randomness, note.receiver_preimage)
            .context("the node's membership proof does not cover the lock")?;
        ensure!(mutator_set.verify(Tip5::hash(&note.utxo), &membership), "the lock's membership proof does not verify");

        let me: ReceivingAddress = self.entropy.nth_generation_spending_key(0).to_address().into();
        let height = BlockHeight::from(snapshot.synced_height);
        let output = TxOutput::onchain_native_currency(
            amount + -fee,
            self.entropy.generate_sender_randomness(height, me.privacy_digest()),
            me,
            true,
        );
        let timestamp = Timestamp::millis(now_ms);
        let details_with = |witness| {
            let input = UnlockedUtxo::unlock(
                note.utxo.clone(),
                LockScriptAndWitness::new_with_nondeterminism(htlc.program(), witness),
                membership.clone(),
            );
            TransactionDetails::new(vec![input], vec![output.clone()], fee, None, timestamp, mutator_set.clone(), self.state.network)
        };
        Ok(match spend {
            HtlcSpend::Claim { preimage, claim_index } => {
                details_with(htlc.witness(&Spend::Claim { preimage, secret: self.swap_secret(claim_index) })).primitive_witness()
            }
            HtlcSpend::Refund { refund_index } => {
                // The witness proves the kernel's timestamp. Assemble the
                // witness once (each assembly re-encrypts the notifications, so
                // the kernel differs), read the path from its kernel, then put
                // the refund witness into that same object.
                let mut witness = details_with(Default::default()).primitive_witness();
                let path = witness.kernel.mast_path(TransactionKernelField::Timestamp);
                let timestamp = witness.kernel.timestamp.to_millis();
                witness.lock_scripts_and_witnesses[0] = LockScriptAndWitness::new_with_nondeterminism(
                    htlc.program(),
                    htlc.witness(&Spend::Refund { timestamp, path, secret: self.swap_secret(refund_index) }),
                );
                witness
            }
        })
    }

    /// The index set of a located lock, for the node's membership proof.
    pub fn htlc_index_set(note: &HtlcNote, located: Located) -> AbsoluteIndexSet {
        note.index_set(located.aocl_index)
    }
}

/// A fresh 32-byte secret and its SHA-256 hash lock (the NPT giver's).
pub fn new_secret() -> ([u8; 32], [u8; 32]) {
    let secret: [u8; 32] = rand::random();
    (secret, hash_lock(&secret))
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn hex_bytes(text: &str) -> Result<Vec<u8>> {
    let text = text.trim();
    if text.len() % 2 != 0 || !text.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("not hex");
    }
    Ok((0..text.len()).step_by(2).map(|i| u8::from_str_radix(&text[i..i + 2], 16).unwrap()).collect())
}
