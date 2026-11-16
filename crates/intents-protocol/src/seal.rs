//! Sealing a message to one recipient's `did:key`, so only that recipient can
//! read it and everyone else drops it unread (rule 5: drop before parse).
//!
//! Construction: the recipient's Ed25519 key converted to X25519 (as
//! libsodium's `crypto_sign_ed25519_pk_to_curve25519`), an ephemeral X25519
//! key per message, HKDF-SHA256 over the shared secret with both public keys
//! as salt and a domain string as info, ChaCha20-Poly1305 with the recipient's
//! DID as associated data. A fresh ephemeral key gives each message its own
//! key, so the nonce is fixed. Not post-quantum: a hybrid (as Hey's) is the
//! next step.
//!
//! On the wire: `{"v":1,"to":"did:key:…","epk":"<b64url>","ct":"<b64url>"}`.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::ChaCha20Poly1305;
use ed25519_dalek::SigningKey;
use hkdf::Hkdf;
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use x25519_dalek::{EphemeralSecret, PublicKey, StaticSecret};

use crate::{DidKey, Error, MAX_MESSAGE_BYTES};

const INFO: &[u8] = b"neptune-intents/seal/v1";
/// A sealed message: the envelope limit plus the sealing overhead.
pub const MAX_SEALED_BYTES: usize = MAX_MESSAGE_BYTES * 2;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sealed {
    v: u8,
    to: String,
    epk: String,
    ct: String,
}

fn key_for(shared: &[u8; 32], epk: &[u8; 32], rpk: &[u8; 32]) -> ChaCha20Poly1305 {
    let salt = [epk.as_slice(), rpk.as_slice()].concat();
    let mut key = [0u8; 32];
    Hkdf::<Sha256>::new(Some(&salt), shared).expand(INFO, &mut key).expect("32 bytes");
    ChaCha20Poly1305::new(&key.into())
}

/// Seals `plaintext` (an encoded envelope) to `to`.
pub fn seal(to: &DidKey, plaintext: &[u8]) -> Result<Vec<u8>, Error> {
    if plaintext.len() > MAX_MESSAGE_BYTES {
        return Err(Error::TooLarge(plaintext.len()));
    }
    let recipient = PublicKey::from(to.verifying_key().to_montgomery().to_bytes());
    let ephemeral = EphemeralSecret::random_from_rng(rand_core::OsRng);
    let epk = PublicKey::from(&ephemeral);
    let shared = ephemeral.diffie_hellman(&recipient);
    let did = to.to_string();
    let cipher = key_for(shared.as_bytes(), epk.as_bytes(), recipient.as_bytes());
    let ct = cipher
        .encrypt(&[0u8; 12].into(), Payload { msg: plaintext, aad: did.as_bytes() })
        .map_err(|_| Error::Malformed("sealing failed".into()))?;
    let sealed = Sealed { v: 1, to: did, epk: URL_SAFE_NO_PAD.encode(epk.as_bytes()), ct: URL_SAFE_NO_PAD.encode(ct) };
    serde_json::to_vec(&sealed).map_err(|e| Error::Malformed(e.to_string()))
}

/// Opens a message sealed to `me`. Anything too large, malformed, addressed to
/// someone else or not opening under `me`'s key is refused before its content
/// is looked at.
pub fn open(me: &SigningKey, sealed: &[u8]) -> Result<Vec<u8>, Error> {
    if sealed.len() > MAX_SEALED_BYTES {
        return Err(Error::TooLarge(sealed.len()));
    }
    let s: Sealed = serde_json::from_slice(sealed).map_err(|e| Error::Malformed(e.to_string()))?;
    let my_did = DidKey::new(me.verifying_key()).to_string();
    if s.v != 1 || s.to != my_did {
        return Err(Error::Malformed("not sealed to this key".into()));
    }
    let epk: [u8; 32] = URL_SAFE_NO_PAD
        .decode(&s.epk)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(|| Error::Malformed("bad ephemeral key".into()))?;
    let ct = URL_SAFE_NO_PAD.decode(&s.ct).map_err(|_| Error::Malformed("bad ciphertext".into()))?;
    let secret = StaticSecret::from(me.to_scalar_bytes());
    let recipient = PublicKey::from(&secret);
    let shared = secret.diffie_hellman(&PublicKey::from(epk));
    let cipher = key_for(shared.as_bytes(), &epk, recipient.as_bytes());
    cipher
        .decrypt(&[0u8; 12].into(), Payload { msg: &ct, aad: s.to.as_bytes() })
        .map_err(|_| Error::Malformed("does not open".into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    fn only_the_recipient_opens_it() {
        let (alice, bob) = (key(1), key(2));
        let sealed = seal(&DidKey::new(bob.verifying_key()), b"intent bytes").unwrap();
        assert_eq!(open(&bob, &sealed).unwrap(), b"intent bytes");
        assert!(open(&alice, &sealed).is_err(), "addressed to someone else");
        let again = seal(&DidKey::new(bob.verifying_key()), b"intent bytes").unwrap();
        assert_ne!(sealed, again, "a fresh ephemeral key each time");
    }

    #[test]
    fn tampering_and_readdressing_are_refused() {
        let (alice, bob) = (key(1), key(2));
        let sealed = seal(&DidKey::new(bob.verifying_key()), b"quote").unwrap();
        let mut s: Sealed = serde_json::from_slice(&sealed).unwrap();
        let mut ct = URL_SAFE_NO_PAD.decode(&s.ct).unwrap();
        ct[0] ^= 1;
        s.ct = URL_SAFE_NO_PAD.encode(ct);
        assert!(open(&bob, &serde_json::to_vec(&s).unwrap()).is_err(), "a flipped bit");
        // Readdressed to Alice: the DID is authenticated, and Alice's key does not open it.
        let mut s: Sealed = serde_json::from_slice(&sealed).unwrap();
        s.to = DidKey::new(alice.verifying_key()).to_string();
        assert!(open(&alice, &serde_json::to_vec(&s).unwrap()).is_err());
        assert!(open(&bob, &vec![b' '; MAX_SEALED_BYTES + 1]).is_err(), "too large");
        assert!(seal(&DidKey::new(bob.verifying_key()), &vec![0; MAX_MESSAGE_BYTES + 1]).is_err());
    }
}
