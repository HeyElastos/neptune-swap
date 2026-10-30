//! The Neptune half of an atomic swap: a hashed timelock (HTLC) lock script for Triton VM.
//!
//! Two ways to spend, nothing else:
//! - claim: a 32-byte `p` with SHA-256(p) = H, and the claimer's spending secret;
//! - refund: the transaction timestamp is at least T, and the refunder's spending secret.
//!
//! H, T and both locks are constants of the program, so its hash (the UTXO's lock script
//! hash) commits to them. A spend's proof reveals neither `p` nor a secret.
//!
//! Lock script contract (neptune-core `proof_collection.rs`): read the kernel MAST hash
//! (5 words) as public input, write nothing, accept by halting. The timestamp is the
//! kernel's leaf 5 of a height-3 tree (`transaction_kernel.rs`); a proven kernel's
//! timestamp never goes down, and nodes admit it at most ~60 s ahead of their clock.

#![forbid(unsafe_code)]

mod sha256;
#[cfg(test)]
mod tests;

use tasm_lib::arithmetic::u64::lt::Lt;
use tasm_lib::hashing::{hash_from_stack::HashFromStack, merkle_verify::MerkleVerify};
use tasm_lib::twenty_first::util_types::merkle_tree::MerkleTreeInclusionProof;
use tasm_lib::{library::Library, prelude::DataType};
use triton_vm::prelude::*;

pub use sha256::{ERROR_DIGEST, ERROR_NOT_U32};
pub const ERROR_SELECTOR: i128 = 2_000;
pub const ERROR_CLAIM_SECRET: i128 = 2_001;
pub const ERROR_TOO_EARLY: i128 = 2_002;
pub const ERROR_REFUND_SECRET: i128 = 2_003;
pub const TIMESTAMP_LEAF: usize = 5;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Htlc {
    pub hash_lock: [u8; 32],
    /// Kernel timestamp (ms since the Unix epoch) from which the refund opens.
    pub refund_after: u64,
    pub claim_lock: Digest,
    pub refund_lock: Digest,
}

pub enum Spend {
    Claim { preimage: [u8; 32], secret: Digest },
    /// `path`: the kernel's authentication path for the timestamp leaf.
    Refund { timestamp: u64, path: Vec<Digest>, secret: Digest },
}

/// The lock a spending secret opens: Neptune's standard hash lock.
pub fn spending_lock(secret: Digest) -> Digest {
    secret.hash()
}

pub fn public_input(kernel_mast_hash: Digest) -> PublicInput {
    PublicInput::new(kernel_mast_hash.reversed().values().to_vec())
}

impl Htlc {
    pub fn program(&self) -> Program {
        let mut lib = Library::new();
        let leaf = lib.import(Box::new(HashFromStack::new(DataType::Bfe)));
        let verify = lib.import(Box::new(MerkleVerify));
        let lt = lib.import(Box::new(Lt));
        let (t_hi, t_lo) = (self.refund_after >> 32, self.refund_after & 0xffff_ffff);
        let lock = |d: Digest| d.values().iter().rev().map(|e| format!("push {} ", e.value())).collect::<String>();
        let (claim, refund) = (lock(self.claim_lock), lock(self.refund_lock));
        let snippets: Vec<String> = lib.all_imports().iter().map(|i| i.to_string()).collect();
        let code = format!(
            "read_io 5 divine 1 dup 0 dup 0 mul dup 1 eq assert error_id {ERROR_SELECTOR}
             skiz call htlc_claim call htlc_refund halt
             htlc_claim: pop 5 call htlc_sha256
               push 0 push 0 push 0 push 0 push 0 divine 5 hash {claim}
               assert_vector error_id {ERROR_CLAIM_SECRET} halt
             htlc_refund: divine 1 place 5 push 3 push {TIMESTAMP_LEAF} dup 7 call {leaf} call {verify}
               split push {t_hi} push {t_lo} pick 3 pick 3 call {lt}
               push 0 eq assert error_id {ERROR_TOO_EARLY}
               push 0 push 0 push 0 push 0 push 0 divine 5 hash {refund}
               assert_vector error_id {ERROR_REFUND_SECRET} halt
             {}
             {}",
            sha256::check("htlc_sha256", words(&self.hash_lock), 0),
            snippets.join("\n"),
        );
        Program::from_code(&code).expect("the HTLC program parses")
    }

    /// The secret inputs of a spend, in the order the program divines them.
    pub fn witness(&self, spend: &Spend) -> NonDeterminism {
        match spend {
            Spend::Claim { preimage, secret } => {
                let p = words(preimage).map(|w| bfe!(w));
                NonDeterminism::new([&[bfe!(1)], &p[..], &secret.reversed().values()].concat())
            }
            Spend::Refund { timestamp, path, secret } => {
                NonDeterminism::new([&[bfe!(0), bfe!(*timestamp)][..], &secret.reversed().values()].concat())
                    .with_digests(path.clone())
            }
        }
    }

    /// The same rule in plain Rust (with the audited `sha2`): tests hold the program to it.
    pub fn accepts(&self, kernel_mast_hash: Digest, spend: &Spend) -> bool {
        use sha2::Digest as _;
        match spend {
            Spend::Claim { preimage, secret } => {
                sha2::Sha256::digest(preimage)[..] == self.hash_lock && spending_lock(*secret) == self.claim_lock
            }
            Spend::Refund { timestamp, path, secret } => {
                let ts = bfe!(*timestamp);
                let leaf = (TIMESTAMP_LEAF, Tip5::hash_varlen(&[ts]));
                let proof = MerkleTreeInclusionProof { tree_height: 3, indexed_leafs: vec![leaf], authentication_structure: path.clone() };
                proof.verify(kernel_mast_hash) && ts.value() >= self.refund_after && spending_lock(*secret) == self.refund_lock
            }
        }
    }
}

/// 32 bytes as 8 big-endian u32 words, SHA-256's word order.
fn words(bytes: &[u8; 32]) -> [u32; 8] {
    core::array::from_fn(|i| u32::from_be_bytes([bytes[4 * i], bytes[4 * i + 1], bytes[4 * i + 2], bytes[4 * i + 3]]))
}
