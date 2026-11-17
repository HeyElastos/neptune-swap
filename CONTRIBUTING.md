# Contributing

Welcome. Neptune Swap is one capsule of many; every piece that makes it safer, more private or
easier to use helps the whole elastOS world computer.

## Getting started

```bash
scripts/vendor-neptune.sh        # neptune-core v0.20.1 into vendor/, with the wasm32 patch
cargo test --release -p light-core -p neptune-provider -p intents-protocol -p intents-ui
scripts/build-intents-capsule.sh # the capsule page (wasm32)
scripts/shots.sh                 # screenshots of every screen from the native build
```

To run it inside elastOS you need the runtime with `patches/elastos-runtime-v2.0.1-neptune.patch`
applied; `scripts/install-dev-home.sh` and `scripts/dev-home.sh` set up an isolated development
Home (set `ELASTOS_RUNTIME` to the patched binary). `scripts/regtest-node.sh` runs a local
Neptune node. Read PLAN.md first: it has the design, the rules and the current state.

## Where help is needed

- **Solver side in the app**: answering takers' sealed requests from the Offer page.
- **The swap flow in the app**: driving `evm_leg.rs` (Base, through elastOS Wallet) and the
  Neptune HTLC ops from the swap form (see `docs/wallet-bridge.md`).
- **Spam resistance for public offers**: proof of work or a stake, solver reputation, a signed
  solver list.
- **Price reports over the carrier**, signed, so no Home has to ask public sources itself.
- **ELA on the Elastos main chain**: a scriptless swap (PLAN.md, "ELA main chain"); the big
  piece is P-256 scalar multiplication in tasm for the Neptune lock script.
- **Provider vaults**: provable liquidity instead of pools (PLAN.md, "Provider vaults").
- **Bitcoin**: the HTLC script and a Wallet that can send BTC.
- **Review**: the HTLC lock script, the timelock rules and the runtime patch deserve more eyes.

## Rules the code keeps

These are not style preferences; a change that breaks one will not be merged (details in
PLAN.md, "Rules"):

1. No domains or URLs between parties: peers are DIDs, messages go over the carrier.
2. Nothing listens on a port.
3. Keys never enter the capsule; the provider holds them.
4. The capsule asks only for what its manifest declares.
5. Inbound messages are dropped unless signed (and, if private, sealed) and size-bounded,
   before anything parses them.
6. A fresh `did:key` per intent.

## Pull requests

- Keep a change to one thing, with tests (`cargo test --release` for the crates you touch).
- Explain in the description what changes for a user and what you checked.
- Security problems: see SECURITY.md, not a public issue.

By contributing you agree that your work is released under the MIT license (see LICENSE).
