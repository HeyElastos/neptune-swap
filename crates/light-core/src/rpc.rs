//! JSON-RPC 2.0 bodies for a neptune-core node (`POST /`), built from
//! neptune-rpc-api's own request and response types so the encoding always
//! matches the node. The caller sends them.

use anyhow::anyhow;
use anyhow::Context;
use anyhow::Result;
use neptune_primitives::announcement_flag::AnnouncementFlag;
use neptune_primitives::block_height::BlockHeight;
use neptune_rpc_api::model::message::BlockHeightsByAbsoluteIndexSetsRequest;
use neptune_rpc_api::model::message::BlockHeightsByAbsoluteIndexSetsResponse;
use neptune_rpc_api::model::message::BlockHeightsByFlagsRequest;
use neptune_rpc_api::model::message::BlockHeightsByFlagsResponse;
use neptune_rpc_api::model::message::GetBlocksRequest;
use neptune_rpc_api::model::message::GetBlocksResponse;
use neptune_rpc_api::model::message::HeightResponse;
use neptune_rpc_api::model::message::TipResponse;
use neptune_rpc_api::model::message::GetBlockResponse;
use neptune_rpc_api::model::message::RestoreMembershipProofRequest;
use neptune_rpc_api::model::message::RestoreMembershipProofResponse;
use neptune_rpc_api::model::message::SubmitTransactionRequest;
use neptune_rpc_api::model::message::SubmitTransactionResponse;
use neptune_rpc_api::model::wallet::mutator_set::RpcMsMembershipSnapshot;
use neptune_rpc_api::model::wallet::transaction::RpcTransaction;
use neptune_rpc_api::model::block::RpcBlock;
use neptune_rpc_api::model::wallet::block::RpcWalletBlock;
use neptune_mutator_set::removal_record::absolute_index_set::AbsoluteIndexSet;
use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::json;
use serde_json::Value;

/// The node's default cap on `wallet_getBlocks` (unless run with `--unsafe-rpc`).
pub const MAX_BLOCKS_PER_CALL: u64 = 100;

/// The node the desktop wallet uses by default. It answers CORS `*`, so a
/// sandboxed capsule (origin `null`) can call it.
pub const DEFAULT_NODE: &str = "https://wallet.neptunefundamentals.org";

fn body(method: &str, params: impl Serialize) -> String {
    json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params }).to_string()
}

pub fn height_request() -> String {
    body("chain_height", json!([]))
}

/// The tip as a full block with its proof (~2.5 MB of JSON on main net).
pub fn tip_request() -> String {
    body("chain_tip", json!([]))
}

/// One block with its proof (the node needs the archival module).
pub fn get_block_request(height: u64) -> String {
    body("archival_getBlock", json!([height]))
}

pub fn get_blocks_request(from: u64, to: u64) -> String {
    body(
        "wallet_getBlocks",
        GetBlocksRequest { from_height: BlockHeight::from(from), to_height: BlockHeight::from(to) },
    )
}

/// Membership proofs for coins about to be spent, with the mutator set they
/// are valid for. It tells the node which coins those are: own node only.
pub fn restore_membership_proof_request(sets: Vec<AbsoluteIndexSet>) -> String {
    body("wallet_restoreMembershipProof", RestoreMembershipProofRequest { absolute_index_sets: sets })
}

pub fn submit_transaction_request(transaction: RpcTransaction) -> String {
    body("wallet_submitTransaction", SubmitTransactionRequest { transaction })
}

pub fn heights_by_flags_request(flags: Vec<AnnouncementFlag>) -> String {
    body("utxoindex_blockHeightsByFlags", BlockHeightsByFlagsRequest { announcement_flags: flags })
}

pub fn heights_by_index_sets_request(sets: Vec<AbsoluteIndexSet>) -> String {
    body(
        "utxoindex_blockHeightsByAbsoluteIndexSets",
        BlockHeightsByAbsoluteIndexSetsRequest { absolute_index_sets: sets },
    )
}

fn result<T: DeserializeOwned>(response: &str) -> Result<T> {
    let mut v: Value = serde_json::from_str(response).context("node reply is not JSON")?;
    if let Some(err) = v.get("error") {
        return Err(anyhow!("node error: {err}"));
    }
    let r = v.get_mut("result").ok_or_else(|| anyhow!("node reply has no result"))?.take();
    serde_json::from_value(r).context("unexpected node reply")
}

pub fn parse_height(response: &str) -> Result<u64> {
    Ok(result::<HeightResponse>(response)?.height.into())
}

pub fn parse_block(response: &str) -> Result<RpcBlock> {
    result::<GetBlockResponse>(response)?.block.ok_or_else(|| anyhow!("the node does not have that block"))
}

pub fn parse_tip(response: &str) -> Result<RpcBlock> {
    Ok(result::<TipResponse>(response)?.block)
}

pub fn parse_blocks(response: &str) -> Result<Vec<RpcWalletBlock>> {
    Ok(result::<GetBlocksResponse>(response)?.blocks)
}

pub fn parse_membership_snapshot(response: &str) -> Result<RpcMsMembershipSnapshot> {
    Ok(result::<RestoreMembershipProofResponse>(response)?.snapshot)
}

/// The node checks the kernel only (fees, mutator set, timestamps); proofs are
/// checked later by its peers, so a caller verifies its proof before sending.
pub fn parse_submitted(response: &str) -> Result<bool> {
    Ok(result::<SubmitTransactionResponse>(response)?.success)
}

pub fn parse_heights_by_flags(response: &str) -> Result<Vec<u64>> {
    let r = result::<BlockHeightsByFlagsResponse>(response)?;
    Ok(r.block_heights.into_iter().map(u64::from).collect())
}

pub fn parse_heights_by_index_sets(response: &str) -> Result<Vec<u64>> {
    let r = result::<BlockHeightsByAbsoluteIndexSetsResponse>(response)?;
    Ok(r.block_heights.into_iter().map(u64::from).collect())
}
