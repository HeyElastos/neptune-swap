use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{to_canonical_bytes, DidKey, Error, Message, MAX_MESSAGE_BYTES};

/// Which networks a message is for. Bound into every signature, so a testnet message
/// never verifies on mainnet and the other way round.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Net {
    Main,
    Test,
}

impl Net {
    /// Domain separation: a signature over these bytes means nothing elsewhere.
    fn domain(self) -> &'static [u8] {
        match self {
            Net::Main => b"neptune-intents/v2/main\n",
            Net::Test => b"neptune-intents/v2/test\n",
        }
    }
}

/// A message body as canonical JSON text, with its kind, its signer's `did:key` and
/// an Ed25519 signature over all three. The body travels as a string so that every
/// reader verifies and parses the very same bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signed {
    pub kind: String,
    pub signer: String,
    pub body: String,
    pub sig: String,
}

/// A verified message: its body, its signer, and the SHA-256 (lowercase hex) of the
/// bytes its signature covers, by which later messages name it. A party's own journal keeps
/// it as checked (serde), never re-decoded from bytes: those would have expired.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decoded<T> {
    pub body: T,
    pub signer: DidKey,
    pub digest: String,
}

impl Signed {
    pub fn sign<T: Message>(body: &T, key: &SigningKey, net: Net) -> Result<Self, Error> {
        let signer = DidKey::new(key.verifying_key()).to_string();
        let body = String::from_utf8(to_canonical_bytes(body)?).map_err(|e| Error::Malformed(e.to_string()))?;
        let sig = URL_SAFE_NO_PAD.encode(key.sign(&signing_bytes(net, T::KIND, &signer, &body)).to_bytes());
        Ok(Self { kind: T::KIND.into(), signer, body, sig })
    }

    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        serde_json::to_vec(self).map_err(malformed)
    }
}

/// Size-checks, parses and verifies an envelope for `net`, then checks its body. The
/// signature covers the body string exactly as received, and that string must be the
/// canonical form of what it parses to: no duplicate keys, no whitespace, no
/// alternative spellings that another reader could see differently.
pub fn decode<T: Message>(bytes: &[u8], now: u64, net: Net) -> Result<Decoded<T>, Error> {
    if bytes.len() > MAX_MESSAGE_BYTES {
        return Err(Error::TooLarge(bytes.len()));
    }
    let envelope: Signed = serde_json::from_slice(bytes).map_err(malformed)?;
    if envelope.kind != T::KIND {
        return Err(Error::WrongKind { expected: T::KIND, got: envelope.kind });
    }
    let signer = DidKey::parse(&envelope.signer)?;
    let sig: [u8; 64] = URL_SAFE_NO_PAD
        .decode(&envelope.sig)
        .ok()
        .and_then(|s| s.try_into().ok())
        .ok_or(Error::BadSignature)?;
    let signed = signing_bytes(net, T::KIND, &envelope.signer, &envelope.body);
    signer
        .verifying_key()
        .verify_strict(&signed, &Signature::from_bytes(&sig))
        .map_err(|_| Error::BadSignature)?;
    let value: Value = serde_json::from_str(&envelope.body).map_err(malformed)?;
    if to_canonical_bytes(&value)? != envelope.body.as_bytes() {
        return Err(Error::Malformed("body is not canonical JSON".into()));
    }
    let body: T = serde_json::from_value(value).map_err(malformed)?;
    body.validate(now)?;
    let digest = Sha256::digest(&signed).iter().map(|b| format!("{b:02x}")).collect();
    Ok(Decoded { body, signer, digest })
}

fn signing_bytes(net: Net, kind: &str, signer: &str, body: &str) -> Vec<u8> {
    [net.domain(), kind.as_bytes(), b"\n", signer.as_bytes(), b"\n", body.as_bytes()].concat()
}

fn malformed(e: serde_json::Error) -> Error {
    Error::Malformed(e.to_string())
}
