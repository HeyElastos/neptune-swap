//! Wire messages for private Neptune intents.
//!
//! A user's capsule sends an [`Intent`] (what it gives and the least it wants
//! back) to a few solver DIDs. Solvers answer with a [`Quote`]. The user picks
//! one with an [`Accept`]. Every message travels as a [`Signed`] envelope from a
//! `did:key` (Ed25519); a user signs each intent with a fresh key so solvers
//! cannot link one user's trades.
//!
//! # What binds what
//!
//! A signature covers the network ([`Net`]), the kind, the signer and the body's
//! canonical bytes; [`decode`] returns the SHA-256 of exactly those bytes as the
//! message's digest. A quote names its intent by that digest and an accept names
//! its quote by digest, so an accept commits to every term of the trade, both
//! signers and the network: nothing can be swapped underneath it.
//!
//! # Who holds the secret
//!
//! Both legs lock under one SHA-256 hash lock over a 32-byte secret. Claiming the
//! Neptune leg proves the preimage inside a STARK and makes nothing public, so the
//! preimage must first become public on the other chain. Hence one rule, in either
//! direction: **the NPT giver holds the secret.** It locks NPT first with the
//! longer timeout, claims the other asset (revealing the preimage), and the NPT
//! receiver claims last. The hash lock therefore travels in the NPT giver's
//! message: the [`Accept`] when the user gives NPT, the [`Quote`] when the solver
//! does.
//!
//! # Time
//!
//! Durations in a quote are offers. What protects funds is absolute: the NPT
//! receiver locks the other asset to refund at [`other_refund_at`], computed from
//! the Neptune HTLC's refund time as read from its lock script, and the NPT giver
//! reveals the secret only while [`may_reveal`] holds.
//!
//! Encryption to the recipients' DIDs happens in the transport layer, not here.

#![forbid(unsafe_code)]

mod canonical;
mod did;
mod error;
mod message;
mod signed;

#[cfg(test)]
mod tests;

pub use canonical::to_canonical_bytes;
pub use did::DidKey;
pub use error::Error;
pub use message::{may_reveal, other_refund_at, Accept, Amount, Asset, Chain, Intent, Message, Quote, ASSETS};
pub use signed::{decode, Decoded, Net, Signed};

/// The protocol version every message carries in its `v` field.
pub const PROTOCOL_VERSION: u16 = 2;

/// Largest encoded envelope a peer accepts. Anything larger is dropped unread.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024;

/// Intents and quotes expire at most this far ahead, so a signed offer cannot become a
/// long free option, whatever the sender's clock says.
pub const MAX_TTL_SECS: u64 = 3600;

/// Allowance for the NPT giver's lock to reach the confirmations the receiver waits for
/// (6 Neptune blocks at a 588 s target, with room for slow blocks).
pub const NEPTUNE_SETTLE_SECS: u64 = 2 * 3600;

/// No HTLC in a quote may lock funds longer than this.
pub const MAX_LOCK_SECS: u64 = 48 * 3600;
