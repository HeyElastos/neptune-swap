# Neptune Swap

Private swaps between Neptune Cash (NPT) and other assets, and a Neptune light wallet, as an
[elastOS](https://elastos.elacitylabs.com) capsule.

A user says what they want ("250 USDC for at least 1,600 NPT"). The request goes sealed to a few
solvers, which answer with signed quotes. The two legs settle with hashed timelocks: Neptune's
own HTLC lock script on one side, an HTLC contract on the other chain on the other. The Neptune
leg keeps amounts, senders and receivers private.

> **Status: development.** Nothing here has had an outside security audit. Swaps have run end to
> end only on local chains (Neptune regtest and a local EVM chain). Do not trust this with
> funds you cannot afford to lose.

## What works

- **Neptune light wallet** (seed, Generation and EC hybrid addresses, address list, QR codes,
  history, send with real proving). It runs as a light node: it checks the tip block's proof
  on every sync (Neptune's succinct validation, main net from height 62,500), and keeps its own
  mutator-set membership proofs, so the node it reads from learns neither validity nor which
  coins it spends.
- **Swap protocol**: intents, quotes, accepts and standing offers as signed, canonical
  envelopes from `did:key` identities, sealed to their recipients; the NPT giver holds the
  secret; timeouts follow from the Neptune lock.
- **HTLCs**: a Triton VM lock script for Neptune and a Solidity contract for EVM chains,
  each with tests; lock, claim and refund run on regtest and a local chain.
- **Solver** and user roles over the elastOS carrier (gossip).
- **Capsule UI** in Slint and Rust, compiled to wasm: swap form, wallet, standing offers,
  solver mode, optional USD reference prices (approved by the user in Home), the other leg
  through elastOS Wallet.

## Layout

| Path | What |
|---|---|
| `crates/light-core` | Wallet core: keys, block scanning, membership proofs, tip verification, spending. No I/O. |
| `crates/neptune-provider` | The elastOS provider for `elastos://neptune/*`: holds the seed, proves, talks to a node. |
| `crates/intents-protocol` | Signed wire messages: intent, quote, accept, offer; sealing. |
| `crates/intents-ui` | The capsule's UI (Slint + Rust, wasm32 for the capsule, native for development). |
| `crates/htlc`, `crates/swap-guard` | The Neptune HTLC lock script, and the receiver's check of a lock. |
| `crates/evm-htlc`, `contracts/htlc-evm` | The EVM HTLC contract and its calldata. |
| `crates/solver`, `crates/evm-signer` | A solver and user over both legs; a local EVM key for tests. |
| `crates/light-wasm`, `crates/light-cli` | Browser tip verification; a proving dry run. |
| `capsules/` | The capsule and provider manifests and the page shell. |
| `patches/` | The wasm32 patch for neptune-core v0.20.1 and the elastOS runtime v2.0.1 patch. |
| `scripts/` | Vendoring, builds, a local regtest node, an isolated development Home. |
| `docs/` | The Wallet bridge and carrier routes. |
| `PLAN.md` | Design, rules, state and findings in detail. |

## Building

neptune-core is not checked in: `scripts/vendor-neptune.sh` fetches v0.20.1 into `vendor/`
and applies `patches/neptune-core-wasm32.patch`.

```bash
scripts/vendor-neptune.sh
cargo test --release -p light-core -p neptune-provider -p intents-protocol -p intents-ui
scripts/build-intents-capsule.sh
```

Running it needs elastOS with the runtime patch applied (the `neptune` scheme, the app's
gateway routes): see PLAN.md, "Architecture" and "M8"/"M9". `scripts/install-dev-home.sh` and
`scripts/dev-home.sh` set up an isolated development Home; they refuse to touch a real one.

## Security model

The rules the code holds to are in PLAN.md ("Rules"): keys never enter the capsule, nothing
listens on a port, inbound messages are dropped unless signed (and, for private messages,
sealed) and bounded, a fresh `did:key` per intent. The risks that remain, and what is not
built yet, are listed there too ("Risks").

## License

MIT, see `LICENSE`. neptune-core (Apache-2.0) and the elastOS runtime (MIT) keep their own
licenses; the files in `patches/` are changes to them, under those licenses.
