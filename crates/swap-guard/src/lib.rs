//! The NPT receiver's check of the NPT giver's lock (PLAN.md, swap flow step 4).
//!
//! Before the NPT receiver locks its own asset on the other chain, it checks the UTXO
//! notification the NPT giver sent it off-chain against what the two agreed. [`check_lock`]
//! is pure: the caller fetches the block from its own node, checks that the block is on its
//! canonical chain, and counts the depth. Everything Neptune-specific is neptune-core's own
//! code: the UTXO and coin types, the native-currency coin encoding, and the addition record
//! commitment (`IncomingUtxo::addition_record`, which is `commit(Tip5(utxo),
//! sender_randomness, receiver_preimage.hash())`).

#![forbid(unsafe_code)]

#[cfg(test)]
mod tests;

use htlc::Htlc;
use neptune_consensus::block::block_kernel::BlockKernel;
use neptune_consensus::transaction::utxo::Coin;
use neptune_consensus::type_scripts::native_currency_amount::NativeCurrencyAmount;
use neptune_mutator_set::addition_record::AdditionRecord;
use neptune_wallet::incoming_utxo::IncomingUtxo;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("the UTXO's lock script is not the agreed HTLC")]
    WrongLockScript,
    #[error("the UTXO does not hold exactly one native-currency coin and nothing else")]
    WrongCoins,
    #[error("the UTXO's native-currency amount is not the agreed amount")]
    WrongAmount,
    #[error("the notification does not open the given addition record")]
    WrongCommitment,
    #[error("the addition record is not a transaction output of the given block")]
    NotInBlock,
    #[error("the block is not deep enough")]
    TooShallow,
}

/// Ok only if `note` is a UTXO locked by exactly `htlc`, holding exactly `amount` and no other
/// coin, that `note` opens `record`, and that `record` is a transaction output of `block`,
/// which lies `depth` blocks deep with `depth >= min_depth`. Depth is counted by the caller,
/// in whatever convention it also uses for `min_depth`.
///
/// Only the block's transaction outputs count, not its guesser-fee outputs: a lock is made by
/// a transaction.
pub fn check_lock(
    note: &IncomingUtxo,
    htlc: &Htlc,
    amount: NativeCurrencyAmount,
    record: AdditionRecord,
    block: &BlockKernel,
    depth: u64,
    min_depth: u64,
) -> Result<(), Error> {
    if note.utxo.lock_script_hash() != htlc.program().hash() {
        return Err(Error::WrongLockScript);
    }
    let expected = Coin::new_native_currency(amount);
    match note.utxo.coins() {
        [coin] if coin.type_script_hash == expected.type_script_hash => {
            // Compares the encoded state: a non-canonical encoding of the amount is refused.
            if *coin != expected {
                return Err(Error::WrongAmount);
            }
        }
        _ => return Err(Error::WrongCoins),
    }
    if note.addition_record() != record {
        return Err(Error::WrongCommitment);
    }
    if !block.body.transaction_kernel.outputs.contains(&record) {
        return Err(Error::NotInBlock);
    }
    if depth < min_depth {
        return Err(Error::TooShallow);
    }
    Ok(())
}
