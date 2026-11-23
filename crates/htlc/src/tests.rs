use sha2::{Digest as _, Sha256};
use tasm_lib::twenty_first::prelude::MerkleTree;
use tasm_lib::twenty_first::util_types::merkle_tree::MerkleTreeInclusionProof;
use triton_vm::isa::instruction::InstructionError;

use super::*;

const T: u64 = 1_800_000_000_000;

fn secret(n: u64) -> Digest {
    Tip5::hash_varlen(&[bfe!(n), bfe!(7)])
}

fn htlc(preimage: &[u8; 32]) -> Htlc {
    Htlc {
        hash_lock: Sha256::digest(preimage).into(),
        refund_after: T,
        claim_lock: spending_lock(secret(1)),
        refund_lock: spending_lock(secret(2)),
    }
}

/// A kernel-shaped MAST: 8 leaves, height 3, leaf 5 the timestamp. Returns its root and the
/// timestamp's authentication path.
fn kernel(timestamp: u64) -> (Digest, Vec<Digest>) {
    let mut leafs: Vec<Digest> = (0..8u64).map(|i| Tip5::hash_varlen(&[bfe!(1000 + i)])).collect();
    leafs[TIMESTAMP_LEAF] = Tip5::hash_varlen(&[bfe!(timestamp)]);
    let tree = MerkleTree::sequential_new(&leafs).expect("tree");
    (tree.root(), tree.authentication_structure(&[TIMESTAMP_LEAF]).expect("path"))
}

/// Runs the lock script: Ok when it halts, else the error id of the check that failed.
fn run(htlc: &Htlc, kernel: Digest, witness: NonDeterminism) -> Result<(), Option<i128>> {
    match VM::run(htlc.program(), public_input(kernel), witness) {
        Ok(output) => {
            assert!(output.is_empty(), "a lock script writes no output");
            Ok(())
        }
        Err(e) => Err(match e.source {
            InstructionError::AssertionFailed(a) | InstructionError::VectorAssertionFailed(_, a) => a.id,
            _ => None,
        }),
    }
}

/// The lock script's rule in plain Rust (with the audited `sha2`): the program is held to it.
fn accepts(htlc: &Htlc, kernel_mast_hash: Digest, spend: &Spend) -> bool {
    match spend {
        Spend::Claim { preimage, secret } => hash_lock(preimage) == htlc.hash_lock && spending_lock(*secret) == htlc.claim_lock,
        Spend::Refund { timestamp, path, secret } => {
            let ts = bfe!(*timestamp);
            let leaf = (TIMESTAMP_LEAF, Tip5::hash_varlen(&[ts]));
            let proof = MerkleTreeInclusionProof { tree_height: 3, indexed_leafs: vec![leaf], authentication_structure: path.clone() };
            proof.verify(kernel_mast_hash) && ts.value() >= htlc.refund_after && spending_lock(*secret) == htlc.refund_lock
        }
    }
}

/// Spends by the program and by the Rust model, and checks they agree.
fn spend(htlc: &Htlc, kernel: Digest, how: &Spend) -> Result<(), Option<i128>> {
    let result = run(htlc, kernel, htlc.witness(how));
    assert_eq!(result.is_ok(), accepts(htlc, kernel, how), "program and model disagree");
    result
}

/// Deterministic test bytes (xorshift), so failures reproduce.
fn bytes(seed: u64) -> [u8; 32] {
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    core::array::from_fn(|_| {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x as u8
    })
}

#[test]
fn sha256_matches_the_sha2_crate_and_known_vectors() {
    // `shasum -a 256` of 32 zero bytes, and of "abcdefghijklmnopqrstuvwxyz012345".
    let vectors: [([u8; 32], &str); 2] = [
        ([0; 32], "66687aadf862bd776c8fc18b8e9f8e20089714856ee233b3902a591d0d5f2925"),
        (*b"abcdefghijklmnopqrstuvwxyz012345", "653bb1245e828fcda4fa53fcd5a3def5bd7654e651f54b4132b73d74e64435c4"),
    ];
    for (preimage, hex) in vectors {
        let digest: [u8; 32] = Sha256::digest(preimage).into();
        assert_eq!(digest.iter().map(|b| format!("{b:02x}")).collect::<String>(), hex);
        let h = htlc(&preimage);
        assert_eq!(spend(&h, kernel(0).0, &Spend::Claim { preimage, secret: secret(1) }), Ok(()));
    }
    for seed in 0..64 {
        let preimage = bytes(seed);
        let h = htlc(&preimage);
        assert_eq!(spend(&h, kernel(0).0, &Spend::Claim { preimage, secret: secret(1) }), Ok(()), "seed {seed}");
        // One flipped bit anywhere in the preimage fails on the digest.
        let mut wrong = preimage;
        wrong[(seed % 32) as usize] ^= 1 << (seed % 8);
        let failed = spend(&h, kernel(0).0, &Spend::Claim { preimage: wrong, secret: secret(1) });
        assert!(matches!(failed, Err(Some(id)) if (ERROR_DIGEST..ERROR_DIGEST + 8).contains(&id)), "seed {seed}: {failed:?}");
    }
}

#[test]
fn a_claim_needs_the_claimers_secret() {
    let preimage = bytes(1);
    let h = htlc(&preimage);
    for wrong in [secret(2), secret(3)] {
        assert_eq!(
            spend(&h, kernel(0).0, &Spend::Claim { preimage, secret: wrong }),
            Err(Some(ERROR_CLAIM_SECRET))
        );
    }
}

#[test]
fn a_preimage_word_beyond_u32_is_refused() {
    let preimage = bytes(2);
    let h = htlc(&preimage);
    let mut tokens = h.witness(&Spend::Claim { preimage, secret: secret(1) }).individual_tokens;
    // Same low 32 bits, one more 2^32: equal modulo 2^32, so only the range check stops it.
    tokens[1] = bfe!(tokens[1].value() + (1 << 32));
    assert_eq!(run(&h, kernel(0).0, NonDeterminism::new(tokens)), Err(Some(ERROR_NOT_U32)));
}

#[test]
fn the_selector_must_be_zero_or_one() {
    let preimage = bytes(3);
    let h = htlc(&preimage);
    for selector in [2, 3, u64::MAX - 1] {
        let mut tokens = h.witness(&Spend::Claim { preimage, secret: secret(1) }).individual_tokens;
        tokens[0] = bfe!(selector);
        assert_eq!(run(&h, kernel(0).0, NonDeterminism::new(tokens)), Err(Some(ERROR_SELECTOR)), "selector {selector}");
    }
}

#[test]
fn a_refund_opens_at_t_and_not_a_millisecond_before() {
    let h = htlc(&bytes(4));
    for (timestamp, opens) in [(T, true), (T + 1, true), (T + (1 << 33), true), (T - 1, false), (0, false)] {
        let (root, path) = kernel(timestamp);
        let result = spend(&h, root, &Spend::Refund { timestamp, path, secret: secret(2) });
        assert_eq!(result.is_ok(), opens, "timestamp {timestamp}: {result:?}");
        if !opens {
            assert_eq!(result, Err(Some(ERROR_TOO_EARLY)));
        }
    }
}

#[test]
fn a_refund_cannot_claim_a_timestamp_the_kernel_does_not_hold() {
    let h = htlc(&bytes(5));
    let (root, path) = kernel(T - 1);
    // A later timestamp with the real kernel's path does not authenticate.
    let forged = spend(&h, root, &Spend::Refund { timestamp: T + 10, path: path.clone(), secret: secret(2) });
    assert!(forged.is_err());
    // Nor does the right timestamp with another kernel's path.
    let (_, other_path) = kernel(T + 10);
    assert!(spend(&h, root, &Spend::Refund { timestamp: T + 10, path: other_path, secret: secret(2) }).is_err());
}

#[test]
fn a_refund_needs_the_refunders_secret() {
    let h = htlc(&bytes(6));
    let (root, path) = kernel(T + 5);
    assert_eq!(
        spend(&h, root, &Spend::Refund { timestamp: T + 5, path, secret: secret(1) }),
        Err(Some(ERROR_REFUND_SECRET))
    );
}

#[test]
fn the_claimer_cannot_take_the_refund_branch_nor_the_refunder_the_claim() {
    let preimage = bytes(7);
    let h = htlc(&preimage);
    let (root, path) = kernel(T + 5);
    assert!(spend(&h, root, &Spend::Refund { timestamp: T + 5, path, secret: secret(1) }).is_err());
    assert!(spend(&h, root, &Spend::Claim { preimage, secret: secret(2) }).is_err());
}

#[test]
fn the_lock_script_hash_commits_to_every_parameter() {
    let h = htlc(&bytes(8));
    let base = h.program().hash();
    let changed = [
        Htlc { hash_lock: [0; 32], ..h.clone() },
        Htlc { refund_after: T + 1, ..h.clone() },
        Htlc { claim_lock: spending_lock(secret(9)), ..h.clone() },
        Htlc { refund_lock: spending_lock(secret(9)), ..h.clone() },
    ];
    for other in changed {
        assert_ne!(other.program().hash(), base);
    }
    assert_eq!(htlc(&bytes(8)).program().hash(), base, "the program is deterministic");
}

#[test]
fn missing_witness_is_a_crash_not_an_accept() {
    let preimage = bytes(9);
    let h = htlc(&preimage);
    let tokens = h.witness(&Spend::Claim { preimage, secret: secret(1) }).individual_tokens;
    for cut in 0..tokens.len() {
        assert!(run(&h, kernel(0).0, NonDeterminism::new(tokens[..cut].to_vec())).is_err(), "cut at {cut}");
    }
}

/// A real STARK for each branch, verified against the claim consensus checks. Slow (proving).
#[test]
#[ignore = "proves; run with --ignored"]
fn both_branches_prove_and_verify_as_neptune_checks_them() {
    let preimage = bytes(10);
    let h = htlc(&preimage);
    let (root, path) = kernel(T);
    for how in [Spend::Claim { preimage, secret: secret(1) }, Spend::Refund { timestamp: T, path, secret: secret(2) }] {
        let program = h.program();
        let claim = Claim::about_program(&program).with_input(public_input(root).individual_tokens);
        let proof = triton_vm::prove(Stark::default(), &claim, program, h.witness(&how)).expect("proves");
        assert!(triton_vm::verify(Stark::default(), &claim, &proof));
    }
}
