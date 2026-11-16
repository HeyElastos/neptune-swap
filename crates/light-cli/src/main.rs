//! Development harness: a proving dry run.
//!
//!   light-cli prove-dry-run <wallet dir> <network> <node url> <amount> <fee> [prove-as]
//!
//! Loads a provider wallet (`<wallet dir>/phrase`, `state.json`) read-only,
//! catches up with the node, builds a payment of `amount` to the wallet's own
//! address, proves it with the real Triton VM prover (as `prove-as`, default
//! the wallet's network; a mock-proof network such as regtest can be proven as
//! `testnet` to exercise real proving on a local chain), verifies every proof,
//! prints the timing, and sends nothing. `neptune-prover` must sit next to
//! this binary.

use std::time::Instant;

use anyhow::{bail, Context, Result};
use light_core::{parse_network, rpc, LightWallet, WalletState};

fn call(url: &str, body: String) -> Result<String> {
    Ok(ureq::post(url).set("content-type", "application/json").send_string(&body)?.into_string()?)
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [cmd, dir, network, node, amount, fee, rest @ ..] = args.as_slice() else {
        bail!("usage: light-cli prove-dry-run <wallet dir> <network> <node url> <amount> <fee> [prove-as]");
    };
    if cmd != "prove-dry-run" {
        bail!("unknown command {cmd}");
    }
    let network = parse_network(network)?;
    let prove_as = rest.first().map(|n| parse_network(n)).transpose()?.unwrap_or(network);
    let dir = std::path::Path::new(dir);
    let phrase = std::fs::read_to_string(dir.join("phrase")).context("phrase")?;
    let state: WalletState = serde_json::from_slice(&std::fs::read(dir.join("state.json")).context("state")?)?;
    let mut wallet = LightWallet::resume(phrase.trim(), state)?;

    let tip = rpc::parse_height(&call(node, rpc::height_request())?)?;
    let mut from = wallet.state().sync.map_or(0, |s| s.height + 1);
    while from <= tip {
        let to = tip.min(from + rpc::MAX_BLOCKS_PER_CALL - 1);
        wallet.apply_blocks(rpc::parse_blocks(&call(node, rpc::get_blocks_request(from, to))?)?)?;
        from = to + 1;
    }
    println!("synced to {tip}; balance {}", wallet.balance(u64::MAX).spendable);

    let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_millis() as u64;
    let plan = wallet.plan_spend(amount, fee, now_ms)?;
    let basis = match wallet.own_basis(&plan) {
        Some(own) => own,
        None => wallet.node_basis(&plan, rpc::parse_membership_snapshot(&call(node, rpc::restore_membership_proof_request(plan.index_sets.clone()))?)?)?,
    };
    let proofs = if basis.own { "own" } else { "node's" };
    let me = wallet.receiving_address()?;
    let (details, built) = wallet.build_payment(&me, &plan, basis, now_ms)?;
    println!("built: {} input(s), {} NPT change, {proofs} membership proofs; proving as {prove_as}", built.inputs, built.change);

    let started = Instant::now();
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    let transaction = runtime.block_on(light_core::send::prove_collection(details.primitive_witness(), prove_as, tip))?;
    let elapsed = started.elapsed();
    let proofs = match &transaction.proof {
        light_core::neptune_consensus::transaction::TransactionProof::ProofCollection(pc) => pc.num_proofs(),
        _ => 0,
    };
    println!("proved and verified {proofs} proofs in {elapsed:.1?}; nothing was sent, no wallet file was written");
    Ok(())
}
