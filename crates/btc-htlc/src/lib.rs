//! The Bitcoin half of a Neptune swap: a hashed timelock in a P2WSH output. The whole script:
//!
//! ```text
//! OP_IF
//!     OP_SIZE 32 OP_EQUALVERIFY OP_SHA256 <hash_lock> OP_EQUALVERIFY <receiver>
//! OP_ELSE
//!     <refund_after> OP_CHECKLOCKTIMEVERIFY OP_DROP <sender>
//! OP_ENDIF
//! OP_CHECKSIG
//! ```
//!
//! Two ways to spend, nothing else: the receiver with a secret of exactly 32 bytes whose
//! SHA-256 is the hash lock (the same secret and lock as the Neptune and EVM sides), or the
//! sender once the chain's median time past is past a Unix time (BIP 113: a refund with
//! `nLockTime = refund_after` is first mined in a block whose 11 blocks before it have a
//! median time above it). `OP_SIZE 32` rules out the length trick where one chain accepts a
//! longer preimage than the other.
//!
//! The refund counts in seconds, never in heights, like every other deadline of the swap: a
//! height drifts from its target time by every slow or fast block over the whole lock, the
//! median time only by its lag, about an hour behind the clock (later, never earlier, unless
//! most miners date their blocks ahead).
//!
//! **The claim path never closes on chain.** After the refund time both paths are valid, so
//! a claim broadcast late shows the secret while the sender can still refund. The receiver
//! reveals only while `intents_protocol::may_reveal` allows (three hours before the refund
//! time) and confirms deeply.
//!
//! The claim's witness carries the secret, so a claim on Bitcoin reveals it, as on the EVM
//! side: [`secret_from_witness`] reads it back. Signing here takes a key directly: tests and
//! a local solver only; a user's key belongs in elastOS Wallet.

#![forbid(unsafe_code)]

use bitcoin::absolute::LockTime;
use bitcoin::hashes::{sha256, Hash};
use bitcoin::opcodes::all::{OP_CHECKSIG, OP_CLTV, OP_DROP, OP_ELSE, OP_ENDIF, OP_EQUALVERIFY, OP_IF, OP_SHA256, OP_SIZE};
use bitcoin::script::Builder;
use bitcoin::secp256k1::{Message, Secp256k1, SecretKey};
use bitcoin::sighash::{EcdsaSighashType, SighashCache};
use bitcoin::transaction::Version;
use bitcoin::{Address, Amount, Network, OutPoint, PublicKey, ScriptBuf, Sequence, Transaction, TxIn, TxOut, Witness};

/// The smallest output a claim or refund may leave (P2WSH dust); below it nothing is sent.
pub const DUST: Amount = Amount::from_sat(330);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Htlc {
    /// SHA-256 of the 32-byte secret.
    pub hash_lock: [u8; 32],
    /// Who claims with the secret, and who takes it back once the median time passes
    /// `refund_after` (Unix seconds).
    pub receiver: PublicKey,
    pub sender: PublicKey,
    pub refund_after: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// Keys must be compressed and different; a refund time of at least 500,000,000 (smaller
    /// values mean heights).
    BadTerms,
    WrongSecret,
    /// The fee would leave less than dust (or more than half the value would go to fees).
    BadFee,
}

impl Htlc {
    pub fn new(hash_lock: [u8; 32], receiver: PublicKey, sender: PublicKey, refund_after: u32) -> Result<Self, Error> {
        if !receiver.compressed || !sender.compressed || receiver == sender || refund_after < 500_000_000 {
            return Err(Error::BadTerms);
        }
        Ok(Self { hash_lock, receiver, sender, refund_after })
    }

    pub fn script(&self) -> ScriptBuf {
        Builder::new()
            .push_opcode(OP_IF)
            .push_opcode(OP_SIZE)
            .push_int(32)
            .push_opcode(OP_EQUALVERIFY)
            .push_opcode(OP_SHA256)
            .push_slice(self.hash_lock)
            .push_opcode(OP_EQUALVERIFY)
            .push_key(&self.receiver)
            .push_opcode(OP_ELSE)
            .push_lock_time(LockTime::from_time(self.refund_after).expect("checked in new"))
            .push_opcode(OP_CLTV)
            .push_opcode(OP_DROP)
            .push_key(&self.sender)
            .push_opcode(OP_ENDIF)
            .push_opcode(OP_CHECKSIG)
            .into_script()
    }

    /// Where the sender pays the lock: P2WSH of the script.
    pub fn address(&self, network: Network) -> Address {
        Address::p2wsh(&self.script(), network)
    }

    pub fn script_pubkey(&self) -> ScriptBuf {
        ScriptBuf::new_p2wsh(&self.script().wscript_hash())
    }
}

pub fn hash_lock(secret: &[u8; 32]) -> [u8; 32] {
    sha256::Hash::hash(secret).to_byte_array()
}

/// One input spending the lock at `outpoint` (worth `value`) to `to`, less `fee`. Refused when
/// the fee would leave dust or take more than half the value: nothing goes to fees by accident.
fn spend(outpoint: OutPoint, value: Amount, to: ScriptBuf, fee: Amount, lock_time: LockTime, sequence: Sequence) -> Result<Transaction, Error> {
    let left = value.checked_sub(fee).filter(|left| *left >= DUST && fee <= value / 2).ok_or(Error::BadFee)?;
    Ok(Transaction {
        version: Version::TWO,
        lock_time,
        input: vec![TxIn { previous_output: outpoint, script_sig: ScriptBuf::new(), sequence, witness: Witness::new() }],
        output: vec![TxOut { value: left, script_pubkey: to }],
    })
}

fn sign(tx: &Transaction, htlc: &Htlc, value: Amount, key: &SecretKey) -> Vec<u8> {
    let sighash = SighashCache::new(tx)
        .p2wsh_signature_hash(0, &htlc.script(), value, EcdsaSighashType::All)
        .expect("one input");
    let sig = Secp256k1::signing_only().sign_ecdsa(&Message::from_digest(sighash.to_byte_array()), key);
    bitcoin::ecdsa::Signature { signature: sig, sighash_type: EcdsaSighashType::All }.to_vec()
}

/// The receiver's claim with the secret. Replaceable (RBF), so its fee can be raised near the
/// refund time.
pub fn claim(htlc: &Htlc, outpoint: OutPoint, value: Amount, to: ScriptBuf, fee: Amount, secret: &[u8; 32], key: &SecretKey) -> Result<Transaction, Error> {
    if hash_lock(secret) != htlc.hash_lock {
        return Err(Error::WrongSecret);
    }
    let mut tx = spend(outpoint, value, to, fee, LockTime::ZERO, Sequence::ENABLE_RBF_NO_LOCKTIME)?;
    let sig = sign(&tx, htlc, value, key);
    tx.input[0].witness = Witness::from_slice(&[sig, secret.to_vec(), vec![1], htlc.script().into_bytes()]);
    Ok(tx)
}

/// The sender's refund: its lock time is the refund time, so it can first be mined once the
/// median time past is above it (a non-final sequence makes the lock time count).
pub fn refund(htlc: &Htlc, outpoint: OutPoint, value: Amount, to: ScriptBuf, fee: Amount, key: &SecretKey) -> Result<Transaction, Error> {
    let lock_time = LockTime::from_time(htlc.refund_after).expect("checked in new");
    let mut tx = spend(outpoint, value, to, fee, lock_time, Sequence::ENABLE_RBF_NO_LOCKTIME)?;
    let sig = sign(&tx, htlc, value, key);
    tx.input[0].witness = Witness::from_slice(&[sig, vec![], htlc.script().into_bytes()]);
    Ok(tx)
}

/// The secret a claim of this lock reveals, if `tx` is one: the 32-byte witness item that
/// opens the hash lock, in an input whose witness script is this lock's. The branch flag is
/// not checked: under segwit v0 any non-zero flag takes the claim path by consensus, and the
/// hash check alone decides (no other transaction can yield a wrong secret). Watch the
/// mempool as well as blocks: a reveal is public as soon as it is broadcast.
pub fn secret_from_witness(tx: &Transaction, htlc: &Htlc) -> Option<[u8; 32]> {
    let script = htlc.script().into_bytes();
    tx.input.iter().find_map(|input| {
        let items: Vec<&[u8]> = input.witness.iter().collect();
        match items.as_slice() {
            [_, secret, _flag, witness_script] if *witness_script == script.as_slice() => {
                let secret: [u8; 32] = (*secret).try_into().ok()?;
                (hash_lock(&secret) == htlc.hash_lock).then_some(secret)
            }
            _ => None,
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitcoin::Txid;

    fn key(n: u8) -> (SecretKey, PublicKey) {
        let sk = SecretKey::from_slice(&[n; 32]).unwrap();
        (sk, PublicKey::new(sk.public_key(&Secp256k1::new())))
    }

    const SECRET: [u8; 32] = [7; 32];
    const VALUE: Amount = Amount::from_sat(100_000);
    const FEE: Amount = Amount::from_sat(1_000);
    /// 2027-01-15, Unix seconds.
    const REFUND: u32 = 1_800_000_000;

    fn setup() -> (Htlc, SecretKey, SecretKey, OutPoint) {
        let (rk, receiver) = key(1);
        let (sk, sender) = key(2);
        let htlc = Htlc::new(hash_lock(&SECRET), receiver, sender, REFUND).unwrap();
        (htlc, rk, sk, OutPoint { txid: Txid::all_zeros(), vout: 0 })
    }

    /// Bitcoin Core's own script check of `tx` spending the lock.
    fn verify(tx: &Transaction, htlc: &Htlc) -> bool {
        let spent = TxOut { value: VALUE, script_pubkey: htlc.script_pubkey() };
        tx.verify(|_| Some(spent.clone())).is_ok()
    }

    fn to() -> ScriptBuf {
        ScriptBuf::new_p2wpkh(&key(9).1.wpubkey_hash().unwrap())
    }

    #[test]
    fn the_receiver_claims_with_the_secret_and_the_claim_reveals_it() {
        let (htlc, rk, _, out) = setup();
        let tx = claim(&htlc, out, VALUE, to(), FEE, &SECRET, &rk).unwrap();
        assert!(verify(&tx, &htlc));
        assert_eq!(secret_from_witness(&tx, &htlc), Some(SECRET));
        assert_eq!(claim(&htlc, out, VALUE, to(), FEE, &[8; 32], &rk), Err(Error::WrongSecret));
    }

    #[test]
    fn only_the_receiver_and_only_with_exactly_32_bytes() {
        let (htlc, rk, sk, out) = setup();
        // The sender's signature on the claim path fails.
        let by_sender = claim(&htlc, out, VALUE, to(), FEE, &SECRET, &sk).unwrap();
        assert!(!verify(&by_sender, &htlc));
        // A wrong secret, forced into the witness, fails.
        let mut wrong = claim(&htlc, out, VALUE, to(), FEE, &SECRET, &rk).unwrap();
        let items: Vec<Vec<u8>> = wrong.input[0].witness.iter().map(|i| i.to_vec()).collect();
        wrong.input[0].witness = Witness::from_slice(&[items[0].clone(), vec![8; 32], vec![1], items[3].clone()]);
        assert!(!verify(&wrong, &htlc));
        // A 33-byte "secret" whose hash we pretend matches: a lock built on it refuses at OP_SIZE.
        let long = [7u8; 33];
        let long_lock = Htlc { hash_lock: sha256::Hash::hash(&long).to_byte_array(), ..htlc.clone() };
        let mut tx = spend(out, VALUE, to(), FEE, LockTime::ZERO, Sequence::ENABLE_RBF_NO_LOCKTIME).unwrap();
        let sig = sign(&tx, &long_lock, VALUE, &rk);
        tx.input[0].witness = Witness::from_slice(&[sig, long.to_vec(), vec![1], long_lock.script().into_bytes()]);
        assert!(!verify(&tx, &long_lock), "OP_SIZE 32 refuses a longer preimage");
    }

    #[test]
    fn the_sender_refunds_only_from_the_time_on() {
        let (htlc, rk, sk, out) = setup();
        let tx = refund(&htlc, out, VALUE, to(), FEE, &sk).unwrap();
        assert!(verify(&tx, &htlc));
        assert_eq!(tx.lock_time, LockTime::from_time(REFUND).unwrap());
        assert_eq!(secret_from_witness(&tx, &htlc), None, "a refund reveals nothing");
        // An earlier lock time is refused by CHECKLOCKTIMEVERIFY.
        let mut early = refund(&htlc, out, VALUE, to(), FEE, &sk).unwrap();
        early.lock_time = LockTime::from_time(REFUND - 1).unwrap();
        let sig = sign(&early, &htlc, VALUE, &sk);
        early.input[0].witness = Witness::from_slice(&[sig, vec![], htlc.script().into_bytes()]);
        assert!(!verify(&early, &htlc));
        // A final sequence switches the lock time off: refused.
        let mut fin = refund(&htlc, out, VALUE, to(), FEE, &sk).unwrap();
        fin.input[0].sequence = Sequence::MAX;
        let sig = sign(&fin, &htlc, VALUE, &sk);
        fin.input[0].witness = Witness::from_slice(&[sig, vec![], htlc.script().into_bytes()]);
        assert!(!verify(&fin, &htlc));
        // The receiver cannot take the refund path.
        assert!(!verify(&refund(&htlc, out, VALUE, to(), FEE, &rk).unwrap(), &htlc));
    }

    #[test]
    fn any_claim_flag_still_yields_the_secret_and_signatures_bind_the_amount() {
        let (htlc, rk, _, out) = setup();
        let mut tx = claim(&htlc, out, VALUE, to(), FEE, &SECRET, &rk).unwrap();
        let items: Vec<Vec<u8>> = tx.input[0].witness.iter().map(|i| i.to_vec()).collect();
        tx.input[0].witness = Witness::from_slice(&[items[0].clone(), items[1].clone(), vec![2], items[3].clone()]);
        assert!(verify(&tx, &htlc), "any non-zero flag takes the claim path by consensus");
        assert_eq!(secret_from_witness(&tx, &htlc), Some(SECRET), "and the secret is still read");
        // A signature made for another amount does not verify.
        let wrong_amount = claim(&htlc, out, VALUE + Amount::from_sat(1), to(), FEE, &SECRET, &rk).unwrap();
        assert!(!verify(&wrong_amount, &htlc));
    }

    #[test]
    fn fees_never_eat_the_lock() {
        let (htlc, rk, sk, out) = setup();
        assert_eq!(claim(&htlc, out, VALUE, to(), VALUE, &SECRET, &rk), Err(Error::BadFee));
        assert_eq!(claim(&htlc, out, VALUE, to(), VALUE - Amount::from_sat(100), &SECRET, &rk), Err(Error::BadFee), "dust left");
        assert_eq!(refund(&htlc, out, VALUE, to(), Amount::from_sat(60_000), &sk), Err(Error::BadFee), "more than half to fees");
    }

    #[test]
    fn terms_are_checked_and_the_script_is_exactly_the_two_paths() {
        let (_, receiver) = key(1);
        let (_, sender) = key(2);
        let uncompressed = PublicKey { compressed: false, ..receiver };
        assert!(Htlc::new([0; 32], uncompressed, sender, REFUND).is_err());
        assert!(Htlc::new([0; 32], receiver, sender, 499_999_999).is_err(), "a height, not a time");
        assert!(Htlc::new([0; 32], receiver, sender, 500_000_000).is_ok());
        assert!(Htlc::new([0; 32], receiver, receiver, REFUND).is_err(), "two different parties");
        let htlc = Htlc::new([0xab; 32], receiver, sender, REFUND).unwrap();
        let asm = htlc.script().to_asm_string();
        assert!(asm.starts_with("OP_IF OP_SIZE OP_PUSHBYTES_1 20 OP_EQUALVERIFY OP_SHA256 OP_PUSHBYTES_32 abab"), "{asm}");
        assert!(asm.contains("OP_ELSE OP_PUSHBYTES_4 00d2496b OP_CLTV OP_DROP OP_PUSHBYTES_33"), "{asm}");
        assert!(asm.ends_with("OP_ENDIF OP_CHECKSIG"), "{asm}");
        assert!(htlc.address(Network::Regtest).to_string().starts_with("bcrt1q"));
    }
}
