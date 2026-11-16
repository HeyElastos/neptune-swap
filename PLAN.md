# Neptune capsule: private intents on elastOS

A private RFQ/swap app in the spirit of NEAR Intents, built as an elastOS capsule, with
Neptune Cash as the settlement layer. This repo is its own project, separate from
Hyper and hey-engine (decided 2026-10-29).

## Goal

A user says what they want ("1000 NPT for at least X USDC"). Solvers compete in private.
The trade settles atomically. Nobody outside learns who traded, what or how much on the
Neptune side.

## Rules (non-negotiable)

1. **No domains, no URLs.** The capsule and its providers address every peer by DID.
   The runtime resolves a DID to a carrier node id. No HTTP to named hosts from any
   component of this project.
2. **Nothing listens.** No open port anywhere. The Neptune node runs with
   `--no-incoming` (listens on no port, accepts no connections, dials out only), and
   with `--restrict-peers-to-list` when we choose its peers.
3. **Keys never enter the capsule.** The native `neptune` provider holds the seed and
   spend keys and does the proving. The capsule asks "pay X to DID Y" with a
   capability token and never sees key material.
4. **Least capability.** The capsule manifest asks only for: the `neptune` provider
   ops, one gossip topic for intents, and DID resolve. No raw network, no key storage.
5. **Drop before parse.** Every inbound message must be encrypted to us and carry a
   valid DID signature, under a hard size limit, or it is dropped unread. Parsing of
   intents and quotes happens inside the WASM sandbox, never in the provider.
6. **One-time user DIDs.** Each intent uses a fresh `did:key`, so solvers cannot link a
   user's trades. Solvers keep long-lived DIDs (their reputation).
7. **Nothing on-chain that need not be.** UTXO notifications go off-chain to the
   recipient's DID (Neptune `EncryptedUtxoNotification`), with a fresh receiving
   address per payment. A static address in a DID document would make all payments
   to it linkable through the public `receiver_identifier`.
8. **Never hand out a witness.** The provider proves at least a `ProofCollection`
   locally. Only that goes to an upgrader for the `SingleProof`; a
   `PrimitiveWitness` would leak the secrets.
9. **The lock script is the crown jewel.** HTLC code becomes public when spent and a
   bug costs money: spec, rust shadow, tests on testnet and an outside review before
   mainnet. Small amounts until then.

## Architecture

| Part | Where it runs | Does |
|---|---|---|
| `capsule/` | WASM, runtime sandbox | UI: create intent, compare quotes, follow the swap. Parses all inbound messages. |
| `neptune` provider | native, behind `POST /api/provider/neptune/<op>` | Wallet, scanning, tx building, proving, HTLC scripts, off-chain notifications. Ops: `balance`, `new_receive`, `build_tx`, `prove`, `submit`, `htlc_lock`, `htlc_claim`, `htlc_refund`. |
| Solver kit | native daemon with its own DID | Listens on the intent topic, quotes, runs both legs. We run the first solver ourselves (no liquidity otherwise). |
| Chain providers | native, DID-addressed | BTC/ETH watching so a user can check the solver's lock before revealing the preimage. Light client (BTC SPV, Ethereum light client), not a trusted RPC. |

## Swap flow (NPT ↔ ELA/BTC/ETH/USDC)

**One rule in both directions: the NPT giver holds the secret.** Claiming a Neptune HTLC
proves the preimage inside a STARK and publishes nothing, so the preimage must become public
on the other chain first. If the NPT receiver held the secret it could take the NPT and the
other side could never claim (found and fixed 2026-10-30; `intents-protocol` enforces it:
the hash lock travels only in the NPT giver's message).

1. Capsule creates the intent and seals it (hybrid PQ, like Hey) to 3–5 solver DIDs.
2. Solvers answer with DID-signed quotes; the user picks one. The NPT giver's message carries
   H = SHA-256(secret): the Accept if the user gives NPT, the Quote if the solver does.
3. The NPT giver locks NPT in a Neptune HTLC lock script: claimable by the receiver with the
   preimage of H, refundable to the giver after `neptune_lock_secs`.
4. The receiver verifies that lock off-chain (UTXO notification: amount, lock script = the
   HTLC with H and both keys, membership in the mutator set), then locks its asset on the other
   chain under H, refundable after `other_lock_secs` (at least `MIN_TIMEOUT_GAP_SECS` shorter).
5. The NPT giver claims the other asset, which makes the preimage public.
6. The NPT receiver claims the NPT with that preimage.

Nobody can steal. A counterparty can only grief (hold funds until the timeout): keep timeouts
short; later, bonds.

**Key dependency:** SHA-256 in tasm. tasm-lib has no SHA-256 (only Tip5). Bitcoin
script, the EVM precompile and Solana all have SHA-256, so one hashlock works against
all of them.

## Current state (2026-11-16)

First slice, built and tested:

- `crates/intents-protocol`: Intent / Quote / Accept messages in a signed envelope
  (Ed25519 `did:key`, domain-separated, canonical sorted-key JSON). Decoding checks
  size, kind, signature over the body as received, then the body's rules (NPT on
  one side, canonical positive amounts, expiry, user HTLC timeout at least
  `MIN_TIMEOUT_GAP_SECS` after the solver's, SHA-256 hash lock). 20 tests.
- `crates/swap-guard`: swap step 4, the NPT receiver's check of the giver's lock before it
  locks its own asset: lock script = the agreed `Htlc`, exactly one NativeCurrency coin of the
  agreed amount (no TimeLock, nothing else), the notification opens the addition record, the
  record is a transaction output of a caller-supplied block at least `min_depth` deep. Pure
  (no chain I/O), neptune-core's own types and `commit`. 8 tests.
- `crates/neptune-provider`: provider-stdio/v1 binary for `elastos://neptune/*`.
  Ops `init`, `networks`, `status` (node height via `chain_height`), `shutdown`.
  Node address must be a loopback IP literal (no DNS); replies never contain it.
  6 tests with a fake loopback node.
- `capsules/neptune-provider/capsule.json`, `capsules/neptune-intents/capsule.json`:
  both pass the Runtime's own `CapsuleManifest::validate` (elastos-common).
- `crates/intents-ui`: the capsule's UI in **Slint and Rust** (user 2026-10-30: write in Slint
  and Rust as much as we can). Hyper's Modern design tokens (graphite/white, gold the only
  accent) and Hyper's frosted glass: Rust draws a Neptune wallpaper and a blurred copy once
  (tiny-skia, three box passes, like hyper_app_core::frost); each glass panel shows its part of
  the frost under the sheet tint. The form builds the intent with `intents-protocol` itself
  (exact `Amount::parse_decimal`), so page and solvers share one rule set. Runtime calls, the
  Home launch token and the `home:app-ready` handshake are Rust (web-sys). 7 tests.
  - Capsule page: `scripts/build-intents-capsule.sh` builds wasm32 (Slint 1.18.1, winit +
    femtovg on WebGL2, as Hyper's web build) into `capsules/neptune-intents/browser/pkg/`;
    `index.html` only starts the wasm. ~1.5 MB gzipped.
  - Screenshots: WebKit snapshots do not capture WebGL, so the native build saves the window
    with Slint's `take_snapshot`: `NEPTUNE_INTENTS_LOOK=dark|light NEPTUNE_INTENTS_DEMO=asset|node|review
    NEPTUNE_INTENTS_SHOT=out.png target/debug/intents-ui`. On the web: `?look=…&demo=…`.
- Assets: NPT (Neptune), **ELA on the Elastos main chain** (`Chain::Elastos`, 8 decimals,
  1 ELA = 10^8 sela; not ESC), USDC and ETH (Ethereum), BTC. NPT, ELA and BTC are all UTXO
  chains. Whether the ELA main chain's script supports a SHA-256 hashlock with a timelock
  refund (the HTLC the swap needs) is not checked yet.

**elastOS 2.0.1 (installed on this Mac 2026-11-16; source read at v2.0.1, bde18c7b):**
- `elastos run` refuses web projections ("opens through Home") and native providers outside
  a fixed supported set (`run_cmd.rs`). `elastos capsule` does not run Home apps either.
- Home serves `/apps/<name>/` only for apps listed under `external` in `components.json`
  (`capsule_inventory.rs`). On an installed Home that file is pinned by SHA-256 in the
  signed release descriptor (`installed_release.rs`); editing it breaks that binding
  ("Installed release inputs require repair"). Unsigned capsules run only from the signed
  release; `dev_mode = true` applies only to a source/development Home (`runtime.rs`).
- The gateway still forwards provider calls only for `documents`, `object`, `chain`, `net`,
  `inspect`, `model`, each to named first-party apps; any other scheme, `neptune` included,
  is 404 (`gateway_provider_proxy.rs`). Third-party apps get no `chain` or `wallet` ops.
- So on the installed Home the capsule cannot be added without breaking its signed release,
  and even when shown it reaches no provider. Testing needs a source Home (own data dir,
  `dev_mode`), and the `neptune` provider needs a Runtime change there or upstream.

**Runtime registration (answered for 57eb779):** the Runtime hard-codes its providers. Adding
`neptune` needs a Runtime change: a scheme arm in
`elastos-server/src/provider_resource.rs`, a launch in `server_infra.rs`, and a
`components.json` entry with the binary's checksum. Until then the provider runs
standalone over stdio. Runtime source: `~/AI/ref/elastos-runtime` (develop, shallow).

Spike from 2026-10-28:

- `crates/light-core`: Neptune light wallet core. Scans blocks for a seed's coins
  (Generation, Symmetric, EC Hybrid keys, gap 25) and computes the balance. Does no
  I/O. Trusts the node for block proofs, PoW and the canonical chain. 6 tests on fake
  blocks pass (first run 2026-11-16), plus an ignored one that chains real
  `wallet_getBlocks` replies (`NEPTUNE_BLOCKS=a.json,b.json`): passed on main net
  62495–62509 (across epsilon's activation) and 62960–62975.
- `crates/light-core/src/rpc.rs`: JSON-RPC bodies for a neptune-core node.
  **Breaks rule 1:** `DEFAULT_NODE = https://wallet.neptunefundamentals.org`. Move node
  access behind the `neptune` provider.
- `crates/light-wasm`, `crates/light-cli`: empty stubs. `capsule/browser`: empty.
- `vendor/neptune-core`: v0.20.1 (404f7379) via `scripts/vendor-neptune.sh`, plus
  `patches/neptune-core-wasm32.patch` (same 40 lines as on v0.19.3; only the Cargo.toml
  context moved). triton-vm 9.0.1 / tasm-lib 9.0.0 / twenty-first 3.1.1 are unchanged, so
  the HTLC's pinned VM still matches consensus; its real-STARK test passes (41.6 s,
  release). Was v0.19.3 until 2026-11-16; upstream was newer (hardfork delta,
  transaction chaining): rebase before building anything on top.

## Goal (user, 2026-11-16) and milestones

A fully working Neptune wallet in the capsule, and swaps that run through that wallet and
elastOS's own ETH/BTC/USDC wallet. Keys stay in the provider (rule 3): "wallet in the
capsule" is the wallet's UI. Everything is developed against a local **regtest** node
(mock proofs and PoW, no real money) in the isolated dev Home; main net only after review.

| # | Milestone | State |
|---|---|---|
| M1 | Capsule served by an isolated elastOS 2.0.1 dev Home (`~/elastos-neptune-dev`, `scripts/install-dev-home.sh`) | served; the user creates the dev Home's passkey |
| M2 | Runtime fork (`patches/elastos-runtime-v2.0.1-neptune.patch`): `neptune` scheme registered and routed to the capsule | written, building |
| M3 | Local regtest neptune-core node from the vendored source, provider pointed at it | next |
| M4 | Wallet in the provider: create/import seed, receive address, sync with light-core, balance; persisted state | next |
| M5 | Send: membership proofs from the own node (`wallet_restoreMembershipProof`), `TransactionDetails` → `ProofCollection` (mock on regtest) → `wallet_submitTransaction` | |
| M6 | Wallet screens in the capsule (Slint): address + QR, balance, history, send | |
| M7 | Neptune HTLC lock/claim/refund in the provider (`crates/htlc`), swap-guard on the verified tip | |
| M8 | Other leg through elastOS Wallet: the fork lets the capsule request `wallet`/`chain` approvals; EVM HTLC contract and Bitcoin HTLC script on their testnets | |
| M9 | Solver + transport over the carrier; end-to-end swap on regtest/testnets | |

## Order of work

1. **Protocol spec:** intent / quote / accept / status messages, encryption, signatures,
   size limits, versioning.
2. **`neptune` provider + wallet:** reuse `light-core`. NPT balance, pay-to-DID,
   off-chain notifications. Neptune testnet. Measure proving time and RAM on this Mac.
3. **HTLC lock script in tasm:** SHA-256 snippet, claim and refund branches, rust shadow,
   tests.
4. **Capsule + our own solver:** NPT ↔ USDC on Ethereum first.
5. **Custom type scripts (tokens):** fully private same-chain swaps in one Neptune tx.
   Needs witness generators in the wallet; today it only knows `NativeCurrency` and
   `TimeLock`.
6. **Neptune p2p over the carrier:** a fully domain-free node. Not yet looked into how
   pluggable `neptune-p2p` is.

## Known limits

- The other chain's leg is public. Timing between the two locks can link them.
- The solver sees the amount of an intent sent to it (but not who sent it).
- Outbound connections show our IP to Neptune peers and carrier relays. The carrier's
  relay (`relay.elastos.app`) is itself a domain at the runtime's transport layer.
- elastOS isolation is unfinished in 0.8.0 (changelog, 0.8.0-alpha.26): providers run
  with the user account's rights, Home can still get every app's permissions, and
  approvals are not yet shown on a runtime-only screen (S1/S2 of #173, planned 0.8.1).
  Until then a payment approval could be spoofed by Home: keep amounts small.

## Softfork epsilon: succinct validation (main net from height 62,500)

Active on main net (tip 62,978 on 2026-11-16; post-activation blocks carry 4 appendix
claims instead of 1). Each block proof now also covers `PredecessorIsValid`, the SWBF update
tree and `KnowsMutatorSet` (neptune-core `docs/src/consensus/succinctness.md`). What it gives
this project, and what it does not:

- **Validation without trusting the node.** Download one block and verify its proof: that
  certifies the chain's validity, the mutator set and block MMR accumulators, and the
  cumulative proof-of-work used to pick the canonical fork. The public node serves the tip
  proof (`chain_tipProof`, ~2.4 MB hex JSON).
  **Built 2026-11-16:** `light_core::tip::verify_tip` (tip from `chain_tip`, parent from
  `wallet_getBlocks`) runs neptune-core's own `Block::validate_tip` against the parent's
  mutator set (its hash is bound by the proven claim), checks the tip's PoW against the
  parent's proven difficulty, and requires the proven chain to sit on main-net block 62,499
  (`402f082e…`). The recursion stops at the activation height, so that hash is a compiled-in
  checkpoint, read from wallet.neptunefundamentals.org (only block at that height, equal to
  62,500's parent): confirm it against an own node. Real tip 62,981: 1.46 s natively;
  in the browser (wasm, `scripts/build-tip-bench.sh`) 1.6 s the first time, then ~120 ms.
  A changed proof, PoW root, parent, clock or network fails (6 tests). Not yet used by
  the wallet scan or swap-guard. A light client must also compare `work_since_activation`
  across the tips it sees to pick the heaviest chain.
- **Not succinct asset management.** A wallet still has to find its coins (scan blocks, or
  the UTXO index, which reveals them) and keep membership proofs current (process each block
  while online, ask an archival node, or download the blocks it missed). Neptune's docs call
  this support "limited". A new wallet starting at the tip avoids history; a restore does not.
- **Swaps.** swap-guard today takes the block kernel from the caller. With a verified tip,
  it could check the lock's block against the tip's block MMR (an MMR membership proof of
  that block's hash) and the lock's addition record against that block: the receiver would
  then trust no node for the lock it checks. Not built.

## Open questions

- Mempool policy for non-standard type scripts. Consensus (`SingleProof`) looks generic
  over type-script hashes; not tested.
- How proof upgraders are found and paid without domains.
- Bootstrapping of solver DIDs: registry, gossip, or a signed list.

## Verified facts and where

- Lock/type scripts, `CollectTypeScripts`, SingleProof verifying each type-script proof:
  neptune-core `docs/src/consensus/{utxo,transaction}.md`, `single_proof.rs`.
- Wallet knows only NativeCurrency/TimeLock: `neptune-consensus/src/type_scripts/known_type_scripts.rs`.
- Public `receiver_identifier` in announcements: `docs/src/neptune-core/utxo_notification.md`.
- `--no-incoming`, `--restrict-peers-to-list`: `neptune-core/src/application/config/cli_args.rs`.
- Proving tiers PrimitiveWitness / LockScript / ProofCollection / SingleProof:
  `neptune-consensus/src/proof_abstractions/tx_proving_capability.rs`.
- tasm-lib hashing: Tip5 only, no SHA-256 (`tasm-lib/src/hashing/`).
- Capsule I/O via `POST /api/provider/<scheme>/<op>` with capability tokens: hey-core
  `src/runtime.rs`.
