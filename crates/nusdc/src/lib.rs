//! nUSDC on Neptune: the token's whole rule, in plain Rust. **A draft; test networks only.**
//!
//! nUSDC is a coin type on Neptune, like NPT: a UTXO holds an nUSDC coin when one of its coins
//! carries nUSDC's type script hash. Consensus runs that type script on every transaction that
//! spends or makes such a coin, and the type script enforces exactly this rule (see
//! docs/nusdc.md). The Triton VM program must agree with [`check`] on every input; tests will
//! hold it to that, as the HTLC lock script is held to its own Rust rule.
//!
//! The rule, all of it:
//! 1. Every nUSDC coin's state is one canonical amount in units of 10^-6 USDC: a single field
//!    element below 2^63. A UTXO holds at most one nUSDC coin.
//! 2. With `in` the nUSDC spent and `out` the nUSDC made (both summed as integers, never in
//!    the field): `out == in` is a transfer and needs nothing more. Any other change, a mint
//!    or a burn, needs the transaction to **spend UTXOs locked by at least M distinct issuer
//!    lock scripts** out of the N fixed in the program.
//!
//! Why issuers authorize by spending, not by a shared secret: each issuer's lock script is
//! proved against this very transaction (its kernel), by that issuer alone, so no secret is
//! ever handed to whoever builds the proof, and an authorization cannot be reused in another
//! transaction (the issuer's UTXO is spent). M-of-N then really means M independent parties.
//!
//! What it leaves out on purpose: no announcements (anyone could forge one in a transaction
//! that never runs this script, so they could not prove the supply), no owner who can change
//! the rule (the program is fixed by its hash), no fee, no pause, no blacklist, no upgrade.
//! Spending a holder's UTXO still needs that UTXO's own lock script; this rule governs amounts.

#![forbid(unsafe_code)]

use neptune_consensus::transaction::utxo::{Coin, Utxo};
use tasm_lib::prelude::Digest;
use tasm_lib::triton_vm::prelude::BFieldElement;

/// The largest amount one coin may hold. Below 2^63, so every amount is one canonical field
/// element. Sums of several coins do exceed the field's modulus (2^64 − 2^32 + 1), so they must
/// be added as integers (128 bits here, u32 limbs in the type script), never as field elements.
pub const MAX_AMOUNT: u64 = (1 << 63) - 1;

/// The issuers: the lock script hashes of their UTXOs, and how many must take part in a mint
/// or burn. Fixed in the program, so in its hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issuers {
    pub locks: Vec<Digest>,
    pub threshold: usize,
}

impl Issuers {
    /// Sound terms: at least one lock, none twice, a threshold between 1 and the lock count.
    pub fn new(locks: Vec<Digest>, threshold: usize) -> Result<Self, Error> {
        let distinct = locks.iter().enumerate().all(|(i, l)| !locks[..i].contains(l));
        if locks.is_empty() || !distinct || threshold == 0 || threshold > locks.len() {
            return Err(Error::BadIssuers);
        }
        Ok(Self { locks, threshold })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BadIssuers,
    /// A coin's state is not one canonical amount.
    BadState,
    /// A UTXO holds more than one nUSDC coin.
    TwoCoins,
    /// The nUSDC total changed without M distinct issuers spending in the transaction.
    Unauthorized,
}

/// The nUSDC amount a UTXO holds (0 if none), checking the coin.
pub fn amount_of(utxo: &Utxo, own: Digest) -> Result<u64, Error> {
    let mine: Vec<&Coin> = utxo.coins().iter().filter(|c| c.type_script_hash == own).collect();
    match mine.as_slice() {
        [] => Ok(0),
        [coin] => match coin.state.as_slice() {
            [word] if word.value() <= MAX_AMOUNT => Ok(word.value()),
            _ => Err(Error::BadState),
        },
        _ => Err(Error::TwoCoins),
    }
}

/// The coin that holds `amount` nUSDC.
pub fn coin(own: Digest, amount: u64) -> Coin {
    assert!(amount <= MAX_AMOUNT, "an amount below 2^63");
    Coin { type_script_hash: own, state: vec![BFieldElement::new(amount)] }
}

/// The rule. `own` is the type script's own hash (the program's digest); `inputs` and
/// `outputs` are the UTXOs the transaction spends and makes (the salted UTXOs the type script
/// checks against its standard input).
pub fn check(own: Digest, issuers: &Issuers, inputs: &[Utxo], outputs: &[Utxo]) -> Result<(), Error> {
    let sum = |utxos: &[Utxo]| -> Result<u128, Error> {
        utxos.iter().try_fold(0u128, |total, u| Ok(total + u128::from(amount_of(u, own)?)))
    };
    if sum(inputs)? == sum(outputs)? {
        return Ok(());
    }
    // A mint or a burn: M distinct issuers each spend a UTXO here.
    let present = issuers.locks.iter().filter(|lock| inputs.iter().any(|u| u.lock_script_hash() == **lock)).count();
    if present >= issuers.threshold {
        Ok(())
    } else {
        Err(Error::Unauthorized)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use neptune_consensus::type_scripts::native_currency_amount::NativeCurrencyAmount;

    fn d(n: u64) -> Digest {
        Digest::new([n, 0, 0, 0, 0].map(BFieldElement::new))
    }

    fn own() -> Digest {
        d(42)
    }

    const HOLDER: u64 = 7;

    /// Three issuers (lock script hashes 100, 101, 102), two must take part.
    fn issuers() -> Issuers {
        Issuers::new((100..103).map(d).collect(), 2).unwrap()
    }

    fn utxo(amount: u64) -> Utxo {
        Utxo::new(d(HOLDER), vec![coin(own(), amount)])
    }

    /// An issuer's UTXO (it holds some NPT; any coins will do).
    fn issuer_utxo(n: u64) -> Utxo {
        Utxo::new(d(100 + n), vec![Coin::new_native_currency(NativeCurrencyAmount::coins(1))])
    }

    #[test]
    fn transfers_keep_the_total_and_need_no_issuer() {
        let ins = [utxo(70), utxo(30)];
        assert_eq!(check(own(), &issuers(), &ins, &[utxo(99), utxo(1)]), Ok(()));
        // Coins of other types (NPT) ride along and do not count.
        let npt = Coin::new_native_currency(NativeCurrencyAmount::coins(5));
        let with_npt = Utxo::new(d(HOLDER), vec![coin(own(), 100), npt]);
        assert_eq!(check(own(), &issuers(), &ins, &[with_npt]), Ok(()));
        assert_eq!(check(own(), &issuers(), &[], &[]), Ok(()));
    }

    #[test]
    fn mints_and_burns_need_m_distinct_issuers_spending_in_the_transaction() {
        let both = [issuer_utxo(0), issuer_utxo(2)];
        assert_eq!(check(own(), &issuers(), &both, &[utxo(500)]), Ok(()), "a mint");
        let burn_in = [utxo(300), issuer_utxo(1), issuer_utxo(2)];
        assert_eq!(check(own(), &issuers(), &burn_in, &[utxo(100)]), Ok(()), "a burn");
        // One issuer, the same issuer twice, an issuer only among the outputs, nobody: refused.
        assert_eq!(check(own(), &issuers(), &[issuer_utxo(0)], &[utxo(500)]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[issuer_utxo(0), issuer_utxo(0)], &[utxo(500)]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[issuer_utxo(0)], &[utxo(500), issuer_utxo(1)]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[], &[utxo(500)]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[utxo(300)], &[utxo(100)]), Err(Error::Unauthorized), "a burn without issuers");
    }

    #[test]
    fn coins_must_be_canonical_and_one_per_utxo() {
        let two = Utxo::new(d(HOLDER), vec![coin(own(), 1), coin(own(), 1)]);
        assert_eq!(check(own(), &issuers(), &[utxo(2)], &[two]), Err(Error::TwoCoins));
        let wide = Utxo::new(d(HOLDER), vec![Coin { type_script_hash: own(), state: vec![BFieldElement::new(1), BFieldElement::new(1)] }]);
        assert_eq!(check(own(), &issuers(), &[utxo(1)], &[wide]), Err(Error::BadState));
        let empty = Utxo::new(d(HOLDER), vec![Coin { type_script_hash: own(), state: vec![] }]);
        assert_eq!(check(own(), &issuers(), &[], &[empty]), Err(Error::BadState));
        let huge = Utxo::new(d(HOLDER), vec![Coin { type_script_hash: own(), state: vec![BFieldElement::new(1 << 63)] }]);
        let both = [issuer_utxo(0), issuer_utxo(1)];
        assert_eq!(check(own(), &issuers(), &both, &[huge]), Err(Error::BadState), "even issuers cannot make a malformed coin");
        let bad_in = Utxo::new(d(HOLDER), vec![Coin { type_script_hash: own(), state: vec![] }]);
        assert_eq!(check(own(), &issuers(), &[bad_in], &[]), Err(Error::BadState));
    }

    #[test]
    fn sums_are_integers_never_field_elements() {
        // Two coins of MAX_AMOUNT add up past the field's modulus p = 2^64 − 2^32 + 1. Summed in
        // the field, 2^32 − 3 units in would "equal" two MAX_AMOUNT coins out: a mint without
        // issuers. As integers they differ, so it needs issuers.
        let p: u128 = (1u128 << 64) - (1u128 << 32) + 1;
        let two_max = 2 * u128::from(MAX_AMOUNT);
        let wrapped = (two_max % p) as u64;
        assert_eq!(wrapped, (1u64 << 32) - 3);
        assert_eq!(check(own(), &issuers(), &[utxo(wrapped)], &[utxo(MAX_AMOUNT), utxo(MAX_AMOUNT)]), Err(Error::Unauthorized));
        let max = [utxo(MAX_AMOUNT), utxo(MAX_AMOUNT), utxo(MAX_AMOUNT)];
        assert_eq!(check(own(), &issuers(), &max, &max), Ok(()));
    }

    #[test]
    fn issuer_terms_are_checked() {
        assert!(Issuers::new(vec![], 1).is_err());
        assert!(Issuers::new(vec![d(1)], 0).is_err());
        assert!(Issuers::new(vec![d(1)], 2).is_err());
        assert!(Issuers::new(vec![d(1), d(1)], 1).is_err(), "a lock twice would let one issuer count twice");
        assert!(Issuers::new(vec![d(1)], 1).is_ok());
    }
}
