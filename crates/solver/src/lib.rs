//! The solver of Neptune swaps, over both legs. The Neptune leg runs through its own
//! `neptune-provider`; the EVM leg through `NeptuneHtlc` (`contracts/htlc-evm`). Messages
//! are `intents-protocol` envelopes over `carrier` (an elastOS node's gossip); the app's
//! end-to-end test (`crates/intents-ui/tests/swap_e2e.rs`) drives `deal` against the app's
//! own swap engine with the sealed bytes handed over directly.

#![forbid(unsafe_code)]

pub mod carrier;
pub mod deal;
pub mod neptune;
