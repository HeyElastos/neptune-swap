use htlc::spending_lock;
use neptune_consensus::block::Block;
use neptune_consensus::transaction::transaction_kernel::TransactionKernelModifier;
use neptune_consensus::transaction::utxo::Utxo;
use neptune_primitives::network::Network;
use neptune_primitives::timestamp::Timestamp;
use tasm_lib::triton_vm::prelude::*;

use super::*;

const MIN_DEPTH: u64 = 6;

fn digest(n: u64) -> Digest {
    Tip5::hash_varlen(&[bfe!(n)])
}

fn htlc() -> Htlc {
    Htlc { hash_lock: [7; 32], refund_after: 1_800_000_000_000, claim_lock: spending_lock(digest(1)), refund_lock: spending_lock(digest(2)) }
}

fn amount() -> NativeCurrencyAmount {
    NativeCurrencyAmount::coins(1000)
}

fn note(utxo: Utxo) -> IncomingUtxo {
    IncomingUtxo { utxo, sender_randomness: digest(3), receiver_preimage: digest(4), is_guesser_fee: false }
}

/// The notification for the agreed lock.
fn good() -> IncomingUtxo {
    note(Utxo::new_native_currency(htlc().program().hash(), amount()))
}

/// A block kernel (genesis, outputs replaced) whose transaction outputs are a stranger's, then
/// `records`.
fn block(records: &[AdditionRecord]) -> BlockKernel {
    let mut kernel = Block::genesis(Network::Main).kernel.clone();
    let outputs = [&[AdditionRecord::new(digest(5))], records].concat();
    kernel.body.transaction_kernel = TransactionKernelModifier::default().outputs(outputs).modify(kernel.body.transaction_kernel);
    kernel
}

/// The check with the agreed HTLC and amount.
fn run(note: &IncomingUtxo, record: AdditionRecord, outputs: &[AdditionRecord], depth: u64) -> Result<(), Error> {
    check_lock(note, &htlc(), amount(), record, &block(outputs), depth, MIN_DEPTH)
}

/// The check on `note`, with `note`'s own addition record in the block: only the UTXO can fail.
fn check(note: &IncomingUtxo) -> Result<(), Error> {
    let record = note.addition_record();
    run(note, record, &[record], MIN_DEPTH)
}

#[test]
fn accepts_the_agreed_lock() {
    assert_eq!(check(&good()), Ok(()));
    let record = good().addition_record();
    assert_eq!(run(&good(), record, &[record], MIN_DEPTH + 100), Ok(()));
}

#[test]
fn rejects_any_other_htlc() {
    let mut others = vec![htlc(); 5];
    others[0].hash_lock[31] ^= 1;
    others[1].refund_after += 1;
    others[2].claim_lock = spending_lock(digest(9));
    others[3].refund_lock = spending_lock(digest(9));
    (others[4].claim_lock, others[4].refund_lock) = (htlc().refund_lock, htlc().claim_lock);
    for other in others {
        assert_eq!(check(&note(Utxo::new_native_currency(other.program().hash(), amount()))), Err(Error::WrongLockScript));
    }
}

#[test]
fn rejects_wrong_amount() {
    let lock = htlc().program().hash();
    for nau in [amount().to_nau() - 1, amount().to_nau() + 1] {
        let utxo = Utxo::new_native_currency(lock, NativeCurrencyAmount::from_nau(nau));
        assert_eq!(check(&note(utxo)), Err(Error::WrongAmount));
    }
}

#[test]
fn rejects_any_other_coin() {
    let lock = htlc().program().hash();
    let half = Coin::new_native_currency(NativeCurrencyAmount::from_nau(amount().to_nau() / 2));
    let unknown = Coin { type_script_hash: digest(8), state: Coin::new_native_currency(amount()).state };
    let utxos = [
        good().utxo.with_time_lock(Timestamp::years(60)),
        Utxo::new(lock, vec![half.clone(), half]),
        Utxo::new(lock, vec![unknown]),
        Utxo::new(lock, vec![]),
    ];
    assert!(utxos[0].is_timelocked());
    for utxo in utxos {
        assert_eq!(check(&note(utxo)), Err(Error::WrongCoins));
    }
}

#[test]
fn rejects_wrong_sender_randomness() {
    let record = good().addition_record();
    let wrong = IncomingUtxo { sender_randomness: digest(9), ..good() };
    assert_eq!(run(&wrong, record, &[record], MIN_DEPTH), Err(Error::WrongCommitment));
}

#[test]
fn rejects_wrong_receiver_preimage() {
    let record = good().addition_record();
    let wrong = IncomingUtxo { receiver_preimage: digest(9), ..good() };
    assert_eq!(run(&wrong, record, &[record], MIN_DEPTH), Err(Error::WrongCommitment));
}

#[test]
fn rejects_a_record_not_in_the_block() {
    let record = good().addition_record();
    assert_eq!(run(&good(), record, &[], MIN_DEPTH), Err(Error::NotInBlock));
    let other = IncomingUtxo { sender_randomness: digest(9), ..good() }.addition_record();
    assert_eq!(run(&good(), record, &[other], MIN_DEPTH), Err(Error::NotInBlock));
}

#[test]
fn rejects_insufficient_depth() {
    let record = good().addition_record();
    for depth in [0, MIN_DEPTH - 1] {
        assert_eq!(run(&good(), record, &[record], depth), Err(Error::TooShallow));
    }
}
