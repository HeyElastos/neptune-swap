//! The Neptune HTLC leg without a node: notes, locks, locating and the early
//! refusals. The full lock → claim / refund round trip runs on regtest.

use neptune_consensus::block::Block;
use neptune_consensus::transaction::utxo::Utxo;
use neptune_consensus::type_scripts::native_currency_amount::NativeCurrencyAmount;
use neptune_mutator_set::mutator_set_accumulator::MutatorSetAccumulator;
use neptune_primitives::network::Network;
use neptune_rpc_api::model::block::body::RpcMutatorSetAccumulator;
use neptune_rpc_api::model::wallet::block::RpcWalletBlock;
use neptune_rpc_api::model::wallet::mutator_set::RpcMsMembershipSnapshot;
use neptune_wallet::mock_block::block_with_puts;
use tasm_lib::prelude::{Digest, Tip5};

use crate::swap::{hex, locate, new_secret, HtlcNote, HtlcSpend, Located};
use crate::LightWallet;

const PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon abandon \
                      abandon abandon abandon abandon abandon abandon abandon abandon agent";
const T: u64 = 1_800_000_000_000;

fn wallet(phrase: &str) -> LightWallet {
    LightWallet::from_phrase(phrase, Network::Main).unwrap()
}

/// Another seed, fresh each run.
fn other() -> LightWallet {
    wallet(&crate::new_phrase().join(" "))
}

/// A note as the giver's wallet would make it: 5 NPT, claimable by `receiver`'s
/// swap key 0, refundable to `giver`'s swap key 0 from `T`.
fn note(giver: &LightWallet, receiver: &LightWallet) -> HtlcNote {
    let (_, hash_lock) = new_secret();
    let mut note = HtlcNote {
        hash_lock: hex(&hash_lock),
        refund_after_ms: T,
        claim_lock: receiver.swap_lock(0).to_hex(),
        refund_lock: giver.swap_lock(0).to_hex(),
        amount: "5".into(),
        utxo: Utxo::new_native_currency(Digest::default(), NativeCurrencyAmount::coins(5)),
        sender_randomness: rand::random(),
        receiver_preimage: rand::random(),
    };
    note.utxo = Utxo::new_native_currency(note.htlc().unwrap().program().hash(), NativeCurrencyAmount::coins(5));
    note
}

#[test]
fn swap_locks_come_from_the_seed() {
    let (a, a_again, b) = (wallet(PHRASE), wallet(PHRASE), other());
    assert_eq!(a.swap_lock(0), a_again.swap_lock(0), "a restored seed has the same swap keys");
    assert_ne!(a.swap_lock(0), a.swap_lock(1));
    assert_ne!(a.swap_lock(0), b.swap_lock(0));
    // One stable solver key per seed, apart from the swap keys.
    assert_eq!(a.solver_key_seed(), a_again.solver_key_seed());
    assert_ne!(a.solver_key_seed(), b.solver_key_seed());
}

#[test]
fn a_note_checks_its_lock_script_and_amount() {
    let (giver, receiver) = (wallet(PHRASE), other());
    let good = note(&giver, &receiver);
    good.check().unwrap();

    let mut more = good.clone();
    more.amount = "6".into();
    assert!(more.check().is_err(), "the stated amount must be the coin's");

    let mut other_terms = good.clone();
    other_terms.refund_after_ms += 1;
    assert!(other_terms.check().is_err(), "other terms make another lock script");

    let json = serde_json::to_string(&good).unwrap();
    assert_eq!(serde_json::from_str::<HtlcNote>(&json).unwrap(), good);
}

#[test]
fn locate_finds_the_lock_in_its_block() {
    let (giver, receiver) = (wallet(PHRASE), other());
    let note = note(&giver, &receiver);
    let genesis = Block::genesis(Network::Main);
    let empty = block_with_puts(Network::Main, &genesis, vec![], vec![]);
    let with_lock = block_with_puts(Network::Main, &empty, vec![rand::random(), note.addition_record()], vec![]);
    let blocks: Vec<RpcWalletBlock> = [&empty, &with_lock].map(RpcWalletBlock::from).to_vec();

    let found = locate(&note, &blocks).expect("the lock is in the second block");
    assert_eq!(found.height, 2);
    let prior = with_lock.body().num_aocl_leafs_prior();
    let outputs = &with_lock.body().transaction_kernel.outputs;
    let position = outputs.iter().position(|o| *o == note.addition_record()).unwrap() as u64;
    assert_eq!(found.aocl_index, prior + position);
    assert!(locate(&note, &blocks[..1]).is_none());
}

#[test]
fn a_refund_before_its_time_is_refused_before_anything_else() {
    let (giver, receiver) = (wallet(PHRASE), other());
    let note = note(&giver, &receiver);
    let snapshot = RpcMsMembershipSnapshot {
        synced_height: 0u64.into(),
        synced_hash: Digest::default(),
        synced_mutator_set: RpcMutatorSetAccumulator::from(&MutatorSetAccumulator::default()),
        membership_proofs: vec![],
    };
    let at = Located { height: 1, aocl_index: 0 };
    let err = giver
        .build_htlc_spend(&note, at, HtlcSpend::Refund { refund_index: 0 }, "0.05", snapshot.clone(), T - 1)
        .unwrap_err()
        .to_string();
    assert!(err.contains("refund opens"), "{err}");
    // A claim is not time-bound; it gets as far as the missing membership proof.
    let err = receiver
        .build_htlc_spend(&note, at, HtlcSpend::Claim { preimage: [0; 32], claim_index: 0 }, "0.05", snapshot, T - 1)
        .unwrap_err()
        .to_string();
    assert!(err.contains("membership proof"), "{err}");
    let _ = Tip5::hash(&note.utxo);
}
