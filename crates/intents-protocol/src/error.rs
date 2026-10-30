#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    #[error("message is {0} bytes, over the limit")]
    TooLarge(usize),
    #[error("malformed message: {0}")]
    Malformed(String),
    #[error("expected a {expected} message, got {got}")]
    WrongKind { expected: &'static str, got: String },
    #[error("unsupported protocol version {0}")]
    Version(u16),
    #[error("invalid did:key: {0}")]
    Did(&'static str),
    #[error("signature does not match the signer")]
    BadSignature,
    #[error("message expired at {0}")]
    Expired(u64),
    #[error("invalid field {field}: {reason}")]
    Invalid { field: &'static str, reason: &'static str },
}

impl Error {
    pub(crate) fn invalid(field: &'static str, reason: &'static str) -> Self {
        Self::Invalid { field, reason }
    }
}
