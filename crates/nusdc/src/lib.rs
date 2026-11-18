//! nUSDC on Neptune: the token's whole rule, in plain Rust. **A draft; test networks only.**
//!
//! nUSDC is a coin type on Neptune, like NPT: a UTXO holds an nUSDC coin when one of its coins
//! carries nUSDC's type script hash. Consensus runs that type script on every transaction that
//! spends or makes such a coin, and the type script enforces exactly this rule (see
//! docs/nusdc.md). The Triton VM program must agree with [`check`] on every input; tests hold
//! it to that, as the HTLC lock script is held to its own Rust rule.
//!
//! The rule, all of it:
//! 1. Every nUSDC coin's state is one canonical amount in units of 10^-6 USDC: a single field
//!    element below 2^63. A UTXO holds at most one nUSDC coin.
//! 2. With `in` the nUSDC spent and `out` the nUSDC made by the transaction:
//!    - `out == in`: an ordinary transfer, nothing else needed;
//!    - `out > in`: a **mint** of `out - in`, only with the issuers' authorization (M distinct
//!      keys of the N fixed in the program) and a public announcement saying exactly that;
//!    - `out < in`: a **burn** of `in - out` (a redemption), only with a public announcement
//!      saying exactly that.
//! 3. Mints and burns are announced, so anyone can add up the supply and hold it against the
//!    USDC the issuers keep in the reserve on Base. Transfers announce nothing.
//!
//! What it leaves out on purpose: no owner can change the rule (the program is fixed by its
//! hash), no fee, no pause, no blacklist, no upgrade. Spending a UTXO still needs its own lock
//! script (the owner's key); this rule only governs amounts.

#![forbid(unsafe_code)]

use neptune_consensus::transaction::announcement::Announcement;
use neptune_consensus::transaction::utxo::{Coin, Utxo};
use tasm_lib::prelude::{Digest, Tip5};
use tasm_lib::triton_vm::prelude::BFieldElement;

/// The largest amount one coin may hold: below 2^63, so sums of a transaction's coins never
/// reach the field's modulus (at most 2^63 per coin, and far fewer than 2^32 coins).
pub const MAX_AMOUNT: u64 = (1 << 63) - 1;

/// The first word of an nUSDC announcement; the second says which: mint or burn.
pub const ANNOUNCEMENT_TAG: u64 = 0x6e75_7364_63; // "nusdc"
pub const MINT: u64 = 1;
pub const BURN: u64 = 2;

/// The issuers: the hashes of their keys (Tip5 of a secret, Neptune's own way of making a
/// lock from a key) and how many must agree to mint. Fixed in the program, so in its hash.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Issuers {
    pub keys: Vec<Digest>,
    pub threshold: usize,
}

impl Issuers {
    /// Sound terms: at least one key, no key twice, a threshold between 1 and the key count.
    pub fn new(keys: Vec<Digest>, threshold: usize) -> Result<Self, Error> {
        let distinct = keys.iter().enumerate().all(|(i, k)| !keys[..i].contains(k));
        if keys.is_empty() || !distinct || threshold == 0 || threshold > keys.len() {
            return Err(Error::BadIssuers);
        }
        Ok(Self { keys, threshold })
    }
}

/// A mint's authorization: secrets for distinct issuer keys, by index. It travels only in the
/// type script's private witness (inside the proof), never on chain.
pub type Authorization = [(usize, Digest)];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    BadIssuers,
    /// A coin's state is not one canonical amount.
    BadState,
    /// A UTXO holds more than one nUSDC coin.
    TwoCoins,
    /// More nUSDC made than spent, without enough distinct, valid issuer keys.
    Unauthorized,
    /// A mint or burn without exactly one announcement of exactly that amount.
    NotAnnounced,
    /// An nUSDC announcement on a transfer, or one that does not match.
    StrayAnnouncement,
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

/// The announcement a mint or burn of `amount` carries.
pub fn announcement(kind: u64, amount: u64) -> Announcement {
    Announcement::new([ANNOUNCEMENT_TAG, kind, amount].map(BFieldElement::new).to_vec())
}

fn is_ours(a: &Announcement) -> bool {
    a.message.first().map(|w| w.value()) == Some(ANNOUNCEMENT_TAG)
}

/// The rule. `own` is the type script's own hash (the program's digest), `inputs`/`outputs`
/// the UTXOs the transaction spends and makes, `announcements` the kernel's public
/// announcements, `authorization` the mint witness (empty unless minting).
pub fn check(
    own: Digest,
    issuers: &Issuers,
    inputs: &[Utxo],
    outputs: &[Utxo],
    announcements: &[Announcement],
    authorization: &Authorization,
) -> Result<(), Error> {
    let sum = |utxos: &[Utxo]| -> Result<u128, Error> {
        utxos.iter().try_fold(0u128, |total, u| Ok(total + u128::from(amount_of(u, own)?)))
    };
    let (spent, made) = (sum(inputs)?, sum(outputs)?);
    let ours: Vec<&Announcement> = announcements.iter().filter(|a| is_ours(a)).collect();
    let announced = |kind: u64, amount: u128| -> Result<(), Error> {
        let expected = announcement(kind, u64::try_from(amount).map_err(|_| Error::NotAnnounced)?);
        match ours.as_slice() {
            [only] if **only == expected => Ok(()),
            [] => Err(Error::NotAnnounced),
            _ => Err(Error::StrayAnnouncement),
        }
    };
    if made == spent {
        return if ours.is_empty() { Ok(()) } else { Err(Error::StrayAnnouncement) };
    }
    if made < spent {
        return announced(BURN, spent - made);
    }
    // A mint: M distinct issuer keys, each opened by its secret.
    let mut used: Vec<usize> = vec![];
    for &(index, secret) in authorization {
        let Some(key) = issuers.keys.get(index) else { return Err(Error::Unauthorized) };
        if used.contains(&index) || Tip5::hash(&secret) != *key {
            return Err(Error::Unauthorized);
        }
        used.push(index);
    }
    if used.len() < issuers.threshold {
        return Err(Error::Unauthorized);
    }
    announced(MINT, made - spent)
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

    fn secret(n: u64) -> Digest {
        d(1000 + n)
    }

    /// Three issuers, two must agree.
    fn issuers() -> Issuers {
        Issuers::new((0..3).map(|n| Tip5::hash(&secret(n))).collect(), 2).unwrap()
    }

    fn utxo(amount: u64) -> Utxo {
        Utxo::new(d(7), vec![coin(own(), amount)])
    }

    #[test]
    fn transfers_keep_the_total_and_announce_nothing() {
        let ins = [utxo(70), utxo(30)];
        assert_eq!(check(own(), &issuers(), &ins, &[utxo(99), utxo(1)], &[], &[]), Ok(()));
        // Coins of other types (NPT) ride along and do not count.
        let npt = Coin::new_native_currency(NativeCurrencyAmount::coins(5));
        let with_npt = Utxo::new(d(7), vec![coin(own(), 100), npt]);
        assert_eq!(check(own(), &issuers(), &ins, &[with_npt], &[], &[]), Ok(()));
        // Neither nUSDC in nor out: nothing to check.
        assert_eq!(check(own(), &issuers(), &[], &[], &[], &[]), Ok(()));
        // An nUSDC announcement on a plain transfer is refused.
        assert_eq!(check(own(), &issuers(), &ins, &[utxo(100)], &[announcement(MINT, 0)], &[]), Err(Error::StrayAnnouncement));
    }

    #[test]
    fn minting_needs_the_threshold_of_distinct_valid_keys_and_an_exact_announcement() {
        let out = [utxo(500)];
        let mint = [announcement(MINT, 500)];
        let ok = [(0, secret(0)), (2, secret(2))];
        assert_eq!(check(own(), &issuers(), &[], &out, &mint, &ok), Ok(()));
        // Minting on top of a transfer: only the difference is minted and announced.
        assert_eq!(check(own(), &issuers(), &[utxo(100)], &out, &[announcement(MINT, 400)], &ok), Ok(()));
        // Not enough keys, a key twice, a wrong secret, an index out of range: refused.
        assert_eq!(check(own(), &issuers(), &[], &out, &mint, &[(0, secret(0))]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[], &out, &mint, &[(0, secret(0)), (0, secret(0))]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[], &out, &mint, &[(0, secret(0)), (1, secret(2))]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[], &out, &mint, &[(0, secret(0)), (3, secret(3))]), Err(Error::Unauthorized));
        assert_eq!(check(own(), &issuers(), &[], &out, &mint, &[]), Err(Error::Unauthorized));
        // Authorized but not announced, announced wrong, or announced twice: refused.
        assert_eq!(check(own(), &issuers(), &[], &out, &[], &ok), Err(Error::NotAnnounced));
        assert_eq!(check(own(), &issuers(), &[], &out, &[announcement(MINT, 499)], &ok), Err(Error::StrayAnnouncement));
        assert_eq!(check(own(), &issuers(), &[], &out, &[announcement(BURN, 500)], &ok), Err(Error::StrayAnnouncement));
        assert_eq!(check(own(), &issuers(), &[], &out, &[mint[0].clone(), mint[0].clone()], &ok), Err(Error::StrayAnnouncement));
        // Other announcements (not nUSDC's) are left alone.
        let other = Announcement::new(vec![BFieldElement::new(1), BFieldElement::new(2)]);
        assert_eq!(check(own(), &issuers(), &[], &out, &[other, mint[0].clone()], &ok), Ok(()));
    }

    #[test]
    fn burning_needs_an_exact_announcement_and_no_keys() {
        let ins = [utxo(300)];
        assert_eq!(check(own(), &issuers(), &ins, &[utxo(100)], &[announcement(BURN, 200)], &[]), Ok(()));
        assert_eq!(check(own(), &issuers(), &ins, &[], &[announcement(BURN, 300)], &[]), Ok(()));
        assert_eq!(check(own(), &issuers(), &ins, &[utxo(100)], &[], &[]), Err(Error::NotAnnounced));
        assert_eq!(check(own(), &issuers(), &ins, &[utxo(100)], &[announcement(BURN, 199)], &[]), Err(Error::StrayAnnouncement));
        assert_eq!(check(own(), &issuers(), &ins, &[utxo(100)], &[announcement(MINT, 200)], &[]), Err(Error::StrayAnnouncement));
    }

    #[test]
    fn coins_must_be_canonical_and_one_per_utxo() {
        let two = Utxo::new(d(7), vec![coin(own(), 1), coin(own(), 1)]);
        assert_eq!(check(own(), &issuers(), &[utxo(2)], &[two], &[], &[]), Err(Error::TwoCoins));
        let wide = Utxo::new(d(7), vec![Coin { type_script_hash: own(), state: vec![BFieldElement::new(1), BFieldElement::new(1)] }]);
        assert_eq!(check(own(), &issuers(), &[utxo(1)], &[wide], &[], &[]), Err(Error::BadState));
        let empty = Utxo::new(d(7), vec![Coin { type_script_hash: own(), state: vec![] }]);
        assert_eq!(check(own(), &issuers(), &[], &[empty], &[], &[]), Err(Error::BadState));
        let huge = Utxo::new(d(7), vec![Coin { type_script_hash: own(), state: vec![BFieldElement::new(1 << 63)] }]);
        assert_eq!(check(own(), &issuers(), &[], &[huge], &[announcement(MINT, 1 << 63)], &[(0, secret(0)), (1, secret(1))]), Err(Error::BadState));
        // A bad coin among the inputs is refused too (it cannot be spent as nUSDC).
        let bad_in = Utxo::new(d(7), vec![Coin { type_script_hash: own(), state: vec![] }]);
        assert_eq!(check(own(), &issuers(), &[bad_in], &[], &[], &[]), Err(Error::BadState));
    }

    #[test]
    fn issuer_terms_are_checked() {
        let k = Tip5::hash(&secret(0));
        assert!(Issuers::new(vec![], 1).is_err());
        assert!(Issuers::new(vec![k], 0).is_err());
        assert!(Issuers::new(vec![k], 2).is_err());
        assert!(Issuers::new(vec![k, k], 1).is_err(), "a key twice would let one secret count twice");
        assert!(Issuers::new(vec![k], 1).is_ok());
    }

    #[test]
    fn large_sums_never_wrap() {
        let max = [utxo(MAX_AMOUNT), utxo(MAX_AMOUNT), utxo(MAX_AMOUNT)];
        assert_eq!(check(own(), &issuers(), &max, &max, &[], &[]), Ok(()));
        // A burn larger than one coin can hold is not announceable: refused, never wrapped.
        assert_eq!(check(own(), &issuers(), &max, &[], &[announcement(BURN, MAX_AMOUNT)], &[]), Err(Error::NotAnnounced));
    }
}
