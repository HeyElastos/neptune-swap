use std::fmt;

use ed25519_dalek::VerifyingKey;

use crate::Error;

/// Multicodec prefix for an Ed25519 public key (varint 0xed).
const ED25519_PUB: [u8; 2] = [0xed, 0x01];
const PREFIX: &str = "did:key:z";

/// An Ed25519 `did:key`, the only signer type the protocol accepts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DidKey(VerifyingKey);

impl DidKey {
    pub fn new(key: VerifyingKey) -> Self {
        Self(key)
    }

    pub fn parse(did: &str) -> Result<Self, Error> {
        let encoded = did.strip_prefix(PREFIX).ok_or(Error::Did("not a base58btc did:key"))?;
        let bytes = bs58::decode(encoded).into_vec().map_err(|_| Error::Did("bad base58"))?;
        let key = bytes.strip_prefix(&ED25519_PUB).ok_or(Error::Did("not an Ed25519 key"))?;
        let key: [u8; 32] = key.try_into().map_err(|_| Error::Did("wrong key length"))?;
        VerifyingKey::from_bytes(&key).map(Self).map_err(|_| Error::Did("not a curve point"))
    }

    pub fn verifying_key(&self) -> &VerifyingKey {
        &self.0
    }
}

impl fmt::Display for DidKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut bytes = ED25519_PUB.to_vec();
        bytes.extend_from_slice(self.0.as_bytes());
        write!(f, "{PREFIX}{}", bs58::encode(bytes).into_string())
    }
}
