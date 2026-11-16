//! Tip verification on a real main-net tip (block 62,981, fetched 2026-11-16).

use std::io::Read;
use std::time::Instant;

use neptune_rpc_api::model::block::RpcBlock;
use neptune_rpc_api::model::wallet::block::RpcWalletBlock;

use super::*;
use crate::tip::verify_tip;

const TIP: &[u8] = include_bytes!("../fixtures/main-tip.json.gz");
const PARENT: &[u8] = include_bytes!("../fixtures/main-parent.json.gz");

fn gunzip(bytes: &[u8]) -> String {
    let mut out = String::new();
    flate2::read::GzDecoder::new(bytes).read_to_string(&mut out).unwrap();
    out
}

fn fixtures() -> (RpcBlock, RpcWalletBlock) {
    let tip = rpc::parse_tip(&gunzip(TIP)).unwrap();
    let parent = rpc::parse_blocks(&gunzip(PARENT)).unwrap().remove(0);
    (tip, parent)
}

/// A little after the tip was found, as a client would see it.
fn now_ms(tip: &RpcBlock) -> u64 {
    let block: neptune_consensus::block::Block = tip.clone().into();
    block.header().timestamp.to_millis() + 60_000
}

#[tokio::test]
async fn a_real_tip_verifies() {
    let (tip, parent) = fixtures();
    let now = now_ms(&tip);
    let started = Instant::now();
    let verified = verify_tip(tip, parent, Network::Main, now).await.unwrap();
    eprintln!("verified tip {} in {:?}", verified.height, started.elapsed());
    assert_eq!(verified.height, 62_981);
    assert!(!verified.work_since_activation.is_empty());
}

#[tokio::test]
async fn a_changed_proof_fails() {
    let (mut tip, parent) = fixtures();
    let now = now_ms(&tip);
    let proof = tip.proof.as_mut().expect("tip has a proof");
    let i = proof.0.len() / 2;
    proof.0[i] = proof.0[i] + neptune_consensus::tasm_lib::triton_vm::prelude::BFieldElement::new(1);
    let err = verify_tip(tip, parent, Network::Main, now).await.unwrap_err().to_string();
    assert!(err.contains("not valid"), "{err}");
}

#[tokio::test]
async fn a_changed_header_fails() {
    let (mut tip, parent) = fixtures();
    let now = now_ms(&tip);
    // The PoW root, which guessers vary (the nonce stays zero since v0.19.4):
    // the proof does not cover it, the proof-of-work check does.
    tip.kernel.header.pow.root = Default::default();
    let err = verify_tip(tip, parent, Network::Main, now).await.unwrap_err().to_string();
    assert!(err.contains("proof of work"), "{err}");
}

#[tokio::test]
async fn a_parent_that_is_not_the_parent_fails() {
    let (tip, mut parent) = fixtures();
    let now = now_ms(&tip);
    parent.proof_leaf = Default::default();
    let err = verify_tip(tip, parent, Network::Main, now).await.unwrap_err().to_string();
    assert!(err.contains("not the tip's parent"), "{err}");
}

#[tokio::test]
async fn a_tip_from_the_future_fails() {
    let (tip, parent) = fixtures();
    let block: neptune_consensus::block::Block = tip.clone().into();
    let long_before = block.header().timestamp.to_millis() - 3_600_000;
    assert!(verify_tip(tip, parent, Network::Main, long_before).await.is_err());
}

#[tokio::test]
async fn no_checkpoint_means_no_verdict() {
    let (tip, parent) = fixtures();
    let now = now_ms(&tip);
    assert!(verify_tip(tip, parent, Network::Testnet(0), now).await.is_err());
}
