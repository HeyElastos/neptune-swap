//! Succinct validation of a tip (softfork epsilon, main net from height 62,500).
//!
//! A block's proof covers a `PredecessorIsValid` claim: its parent satisfies
//! the block validity rules, recursively, down to the parent of the activation
//! block. Verifying one block therefore validates the chain from there, without
//! trusting the node that served it. What the proof cannot cover:
//!
//! - its own proof of work, which is found after proving: checked here against
//!   the parent's difficulty from the proven claim;
//! - which chain it extends below the activation height: the claim names that
//!   block's hash, and it must equal the checkpoint compiled in here;
//! - whether a heavier valid chain exists: compare
//!   [`VerifiedTip::work_since_activation`] across the tips you know.
//!
//! The parent's mutator set comes from the node, but `Block::validate_tip`
//! checks its hash against the proven claim, so a wrong one fails.

use anyhow::bail;
use anyhow::ensure;
use anyhow::Context;
use anyhow::Result;
use neptune_consensus::block::block_kernel::BlockKernel;
use neptune_consensus::block::Block;
use neptune_mutator_set::mutator_set_accumulator::MutatorSetAccumulator;
use neptune_primitives::network::Network;
use neptune_primitives::timestamp::Timestamp;
use neptune_rpc_api::model::block::RpcBlock;
use neptune_rpc_api::model::wallet::block::RpcWalletBlock;
use serde::Serialize;
use tasm_lib::prelude::Digest;

/// Main net block 62,499, the parent of softfork epsilon's activation block:
/// the chain every verified tip must be built on. Read on 2026-11-16 from
/// wallet.neptunefundamentals.org (`archival_getBlockDigest`, the only block
/// at that height) and equal to block 62,500's `prev_block_digest` there.
/// Confirm it against a node of your own before relying on it.
pub const MAIN_ACTIVATION_PARENT: &str =
    "402f082ed5ea26cfa0818e167f83db12e4ffd2ace28ec6d3e6f5daf1712d74b0dcf1010000000000";

fn activation_parent(network: Network) -> Result<Digest> {
    match network {
        Network::Main => Ok(Digest::try_from_hex(MAIN_ACTIVATION_PARENT)?),
        other => bail!("no activation checkpoint for {other}"),
    }
}

/// A tip whose proof, proof of work and lineage were checked.
#[derive(Clone, Debug, Serialize)]
pub struct VerifiedTip {
    pub height: u64,
    pub hash: Digest,
    pub timestamp_ms: u64,
    /// Proof of work since activation, proven: how light clients compare tips.
    pub work_since_activation: String,
    /// The mutator set after the tip, which coins' membership proofs and the
    /// swap checks are relative to.
    #[serde(skip)]
    pub mutator_set: MutatorSetAccumulator,
    /// The tip's block MMR: every earlier block's hash is a leaf of it.
    #[serde(skip)]
    pub block: Block,
}

/// Validate `tip` (a full block with its proof, as `chain_tip` returns it)
/// given its `parent` as `wallet_getBlocks` returns it. `now_ms` is the
/// caller's clock, for the rule that a block is not from the future.
pub async fn verify_tip(tip: RpcBlock, parent: RpcWalletBlock, network: Network, now_ms: u64) -> Result<VerifiedTip> {
    let block: Block = tip.into();
    let parent_hash = parent.hash();
    let parent_kernel: BlockKernel = parent.kernel.into();
    ensure!(
        block.header().prev_block_digest == parent_hash,
        "the parent block is not the tip's parent"
    );
    let guesser_records = parent_kernel
        .guesser_fee_addition_records(parent_hash)
        .context("parent block's guesser fee records")?;
    let parent_mutator_set = parent_kernel.body.mutator_set_accumulator_after(guesser_records);

    // Proof (with the predecessor claim) and every rule the host checks.
    let parent_info = block
        .validate_tip(&parent_mutator_set, Timestamp::millis(now_ms), network)
        .await
        .map_err(|e| anyhow::anyhow!("tip is not valid: {e}"))?;

    // Proof of work, against the parent's difficulty from the proven claim.
    ensure!(
        block.has_proof_of_work_against(network, parent_info.timestamp, parent_info.difficulty),
        "tip lacks proof of work"
    );

    // Lineage below the activation height.
    ensure!(
        parent_info.activation_parent_hash == activation_parent(network)?,
        "tip is built on another chain below the activation height"
    );

    let mutator_set = block.mutator_set_accumulator_after().context("tip's mutator set")?;
    Ok(VerifiedTip {
        height: block.header().height.into(),
        hash: block.hash(),
        timestamp_ms: block.header().timestamp.to_millis(),
        work_since_activation: parent_info.work_since_activation.to_string(),
        mutator_set,
        block,
    })
}
