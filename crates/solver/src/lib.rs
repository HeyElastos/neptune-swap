//! Neptune swaps beyond the wallet: the solver and the user, over both legs.
//! The Neptune leg runs through each party's own `neptune-provider`; the EVM
//! leg through `NeptuneHtlc` (`contracts/htlc-evm`). Messages are
//! `intents-protocol` envelopes; the transport between the parties is the
//! caller's (see `bin/swap-e2e.rs` for a local run).

#![forbid(unsafe_code)]

pub mod neptune;
pub mod roles;
