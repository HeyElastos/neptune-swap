# Neptune Swap

Private swaps between Neptune Cash (NPT) and other assets, and a Neptune light wallet, as an
[elastOS](https://elastos.elacitylabs.com) capsule.

A user says what they want ("250 USDC for at least 1,600 NPT"). The request goes sealed to a few
solvers, which answer with signed quotes. The two legs settle with hashed timelocks: Neptune's
own HTLC lock script on one side, an HTLC contract on the other chain on the other. The Neptune
leg keeps amounts, senders and receivers private.

> **Experimental concept, not a finished product.** This is a working concept to show what an
> elastOS capsule can do, not software to rely on. It has not been tested in real use or
> audited: swaps have run end to end only on local test chains (Neptune regtest and a local
> EVM chain), and parts of the design are still open. Do not use it with real funds.

## Why elastOS

Most apps today are a page on someone's server, holding your keys or your data, reachable at a
domain that can be seized, censored or spoofed. elastOS turns that around: the app runs on
**your** computer, inside a runtime that decides what it may touch, and it talks to the world
through identities, not domains. Neptune Swap leans on that at every step:

- **Capsules, sandboxed.** The app is a capsule: a web page compiled from Rust to wasm that can
  reach only its own Home. No raw network, no file system, no keys. What it may do is declared
  in its manifest and enforced by the runtime.
- **Providers hold the power, behind typed doors.** Keys, proving and node access live in a
  native provider (`elastos://neptune/*`) that answers a short list of typed operations. The
  capsule asks "pay X to Y"; it never sees a seed.
- **The user approves, in Home.** Anything with consequences goes through a capability
  approval in Home or a passkey prompt in Wallet: showing USD prices, sharing an account,
  every payment. A page cannot approve for itself.
- **Identities instead of domains.** Parties are `did:key` identities. Messages travel over the
  elastOS carrier, peer to peer, signed and sealed to their recipient. No server in the middle
  to trust, block or impersonate.
- **One wallet for every chain.** The other leg of a swap (ETH, USDC; Bitcoin when Wallet can
  send it) goes through elastOS Wallet and whatever approval method the user chose there:
  its own passkey, MetaMask, UniSat.
- **Signed software.** A Home runs a signed release whose components are pinned by hash.

Each capsule does one thing well and leans on the runtime for the rest: identity, transport,
approvals, wallets, storage. Put enough of them together, run by people on their own machines
and talking to each other directly, and you get a world computer that nobody owns and anybody
can extend.

**Build capsules with us.** A swap needs solvers, price feeds, chain watchers, reputation;
a world computer needs far more. If you can write Rust, Slint or a provider, pick something,
open an issue or a pull request, and let us build it together.

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
| `docs/` | Swap safety rules, the general Runtime features this app needs, the Wallet bridge and carrier routes. |
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
gateway routes): see PLAN.md, "Architecture" and "M8"/"M9". `docs/runtime-hooks.md` lists the
patch as general Runtime features, so that one day no patch is needed. `scripts/install-dev-home.sh` and
`scripts/dev-home.sh` set up an isolated development Home; they refuse to touch a real one.

## Security model

How a swap stays atomic (timeouts, the secret, lock checks, reorgs, fees) is in
`docs/swap-safety.md`, with what is still open. The rules the code holds to are in PLAN.md ("Rules"): keys never enter the capsule, nothing
listens on a port, inbound messages are dropped unless signed (and, for private messages,
sealed) and bounded, a fresh `did:key` per intent. The risks that remain, and what is not
built yet, are listed there too ("Risks").

## License

MIT, see `LICENSE`. neptune-core (Apache-2.0) and the elastOS runtime (MIT) keep their own
licenses; the files in `patches/` are changes to them, under those licenses.
