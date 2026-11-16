//! One NPT → ETH swap from start to finish on local chains: the regtest Neptune
//! node (scripts/regtest-node.sh) and anvil (chain id 8453) with NeptuneHtlc
//! deployed. Run by scripts/swap-e2e.sh, which deploys the contract.
//!
//!   swap-e2e <contract> <user-npt-data> <solver-npt-data> [<runtime-coords.json>]
//!
//! With the coords of an elastOS node, every message goes over that node's
//! carrier (gossip on a fresh topic) after the operator approves the peer
//! capability in Home; both parties use the one node. Without, the sealed
//! bytes are handed over directly.
//!
//! The user's EVM key is anvil's test account 2, standing in for elastOS Wallet
//! on this local chain; the solver's is account 1.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use evm_htlc::{hex, parse_address};
use evm_signer::{Node, Wallet};
use intents_protocol::{open, seal, DidKey};
use solver::carrier::Carrier;
use solver::neptune::Provider;
use solver::roles::{EvmChain, Solver, User};

const SOLVER_EVM_KEY: &str = "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d";
const USER_EVM_KEY: &str = "0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a";
const NAU_PER_NPT: u128 = 4 * 10u128.pow(30);

fn provider(data: &str) -> Provider {
    Provider {
        binary: PathBuf::from("target/release/neptune-provider"),
        data: PathBuf::from(data),
        network: "neptune-regtest".into(),
        config: r#"{"networks":[{"id":"neptune-regtest","rpc":"http://127.0.0.1:9797"}]}"#.into(),
    }
}

fn eth(wei: u128) -> String {
    format!("{}.{:018}", wei / 10u128.pow(18), wei % 10u128.pow(18)).trim_end_matches('0').trim_end_matches('.').to_string()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let contract = parse_address(args.first().context("contract address")?).context("contract address")?;
    let evm = EvmChain { node: Node::local("http://127.0.0.1:8545")?, contract };
    let mut user = User::new(provider(args.get(1).context("user data")?), Wallet::from_hex(USER_EVM_KEY)?);
    // The solver's DID is long-lived (its reputation); fixed here for the run.
    let mut solver = Solver::new(
        SigningKey::from_bytes(&[0x51; 32]),
        provider(args.get(2).context("solver data")?),
        Wallet::from_hex(SOLVER_EVM_KEY)?,
        5 * 10u128.pow(17), // 0.5 ETH per NPT
    );

    let user_npt = user.neptune.balance()?;
    let solver_npt = solver.neptune.balance()?;
    let user_eth = evm.node.balance(&user.eth.address)?;
    println!("before: user {user_npt} NPT, {} ETH; solver {solver_npt} NPT", eth(user_eth));

    // Every message is sealed to its recipient's DID and opened with its key: the
    // only hand-over here is the bytes (the carrier comes next).
    let solver_did = DidKey::new(solver.did.verifying_key());
    let carrier = match args.get(3) {
        Some(coords) => {
            let c = Carrier::connect(
                coords.as_ref(),
                "neptune-swap",
                "solver",
                "Neptune swaps: the solver's intents topic",
                Duration::from_secs(280),
            )?;
            let topic = format!("neptune-swap-dev-{:x}", rand::random::<u64>());
            c.join(&topic)?;
            let user = c.party("user");
            println!("   carrier: joined {topic}");
            Some((c, user, topic))
        }
        None => None,
    };
    // Sends `sealed` from one party and returns what the other receives.
    let hop = |sealed: Vec<u8>, to_solver: bool| -> Result<Vec<u8>> {
        let Some((solver_side, user_side, topic)) = &carrier else { return Ok(sealed) };
        let (from, to) = if to_solver { (user_side, solver_side) } else { (solver_side, user_side) };
        from.send(topic, &sealed)?;
        wait("a carrier message", || to.recv(topic)?.pop().context("nothing yet"))
    };
    let to_solver = |bytes: &[u8], key: &SigningKey| -> Result<Vec<u8>> { Ok(open(key, &hop(seal(&solver_did, bytes)?, true)?)?) };

    // 1. The user asks: 2 NPT for at least 0.9 ETH, sealed to the solver.
    let intent = user.intent(2 * NAU_PER_NPT, 9 * 10u128.pow(17))?;
    let sealed_len = seal(&solver_did, &intent)?.len();
    let intent = to_solver(&intent, &solver.did)?;
    println!("1. intent ({} bytes, {sealed_len} sealed; one-time DID)", intent.len());
    // 2. The solver quotes, sealed to the intent's one-time DID.
    let quote = solver.quote(&intent)?;
    let user_did = solver.user_did().context("no intent")?;
    let quote = open(&user.did, &hop(seal(&user_did, &quote)?, false)?)?;
    println!("2. quote ({} bytes): 1 ETH, ETH lock 2 h, NPT lock 7 h", quote.len());
    // 3. The user accepts with the hash lock (it holds the secret).
    let accept = user.accept(&quote)?;
    solver.take_accept(&to_solver(&accept, &solver.did)?)?;
    println!("3. accept ({} bytes) checked by the solver", accept.len());
    // 4. The user locks 2 NPT and sends the signed npt_locked note to the solver.
    let npt_locked = to_solver(&user.lock_npt()?, &solver.did)?;
    println!("4. user locked 2 NPT; npt_locked ({} bytes) to the solver", npt_locked.len());
    // 5. The solver checks that lock on its own node, three blocks deep.
    let depth = wait("the NPT lock", || solver.check_npt_lock(&npt_locked, 3))?;
    println!("5. solver checked the NPT lock: this deal, agreed terms, {depth} blocks deep");
    // 6. The solver locks the ETH for the user and says where (evm_locked).
    let evm_locked = open(&user.did, &hop(seal(&user_did, &solver.lock_eth(&evm)?)?, false)?)?;
    println!("6. solver locked 1 ETH; evm_locked ({} bytes) to the user", evm_locked.len());
    // 7. The user checks the ETH lock on chain and claims it: the secret goes public.
    let claim = user.claim_eth(&evm, &evm_locked)?;
    println!("7. user claimed the ETH ({}…): the secret is public in Claimed", &claim[..18]);
    // 8. The solver reads the secret and claims the NPT.
    solver.claim_npt(&evm)?;
    println!("8. solver claimed the NPT with the secret");

    let after_npt = wait("the solver's NPT", || {
        let b = solver.neptune.balance()?;
        anyhow::ensure!(b != solver_npt, "not yet");
        Ok(b)
    })?;
    let user_npt_after = user.neptune.balance()?;
    let user_eth_after = evm.node.balance(&user.eth.address)?;
    println!(
        "after: user {user_npt_after} NPT, {} ETH (+{} ETH less gas); solver {after_npt} NPT",
        eth(user_eth_after),
        eth(user_eth_after.saturating_sub(user_eth).max(0))
    );
    println!("contract {}", hex(&contract));
    Ok(())
}

/// Retries `f` until it succeeds (about two minutes).
fn wait<T>(what: &str, mut f: impl FnMut() -> Result<T>) -> Result<T> {
    let mut last = None;
    for _ in 0..60 {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) => last = Some(e),
        }
        std::thread::sleep(Duration::from_secs(2));
    }
    Err(last.unwrap()).with_context(|| format!("waiting for {what}"))
}
