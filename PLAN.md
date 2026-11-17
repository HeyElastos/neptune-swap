# Neptune Swap: private swaps on elastOS

**Neptune Swap** (renamed from Neptune Intents, user 2026-11-16; app id `neptune-swap`, was
`neptune-intents`; the crypto domain tags keep `neptune-intents/…`, since swap secrets and
signed/sealed messages depend on them). A private RFQ/swap app built as an elastOS capsule, with Neptune Cash as the settlement
layer. All code here is this project's own, apart from the vendored neptune-core (patched),
the elastOS runtime fork (a patch) and ordinary open-source crates.

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

1. Capsule creates the intent and seals it (hybrid PQ planned) to 3–5 solver DIDs.
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

## Resume here (2026-11-17)

Done today: the EVM leg's building blocks (see "EVM leg" below): Base in the protocol, the
contract at one address fixed by its creation code (CREATE2), `crates/intents-ui/src/evm_leg.rs` (Wallet bridge
transactions, chain reads, lock checks, the secret from `Claimed` logs), the form on Base,
and a runtime fork change that lets the app make typed chain reads. Not yet: the swap flow
that drives these steps in the app (taker first), and a live run in the dev Home.

Next, in order:
1. **Taker flow in the app** (Review → Send to solvers): seal the intent to the offer's
   solver, take the quote, accept, then per direction: user gives USDC/ETH → check the
   solver's NPT lock (a provider op `htlc_check` against the agreed terms, so the capsule
   needs no neptune-core), lock on Base, wait for the solver's claim, read the secret, claim
   the NPT; refund on Base when the solver never claims. User gives NPT → lock NPT, check the
   solver's Base lock (`evm_leg::check_lock`, contract code first), claim it.
2. **Solver side in the app** (Offer page answers sealed intents).
3. **Live run in the dev Home**: rebuild the fork (`cargo build --release -p elastos-server
   --bin elastos`), restart `scripts/dev-home.sh`, a local anvil chain as Base with the
   contract deployed (`scripts/evm-htlc-local.sh`), the native solver as counterparty.
4. **ELA main chain** (below): the Neptune lock script with a revealing claim, P-256 in tasm.
5. **Provider vaults** (below).

## Paused 2026-11-16 evening (kept for reference)

State: everything committed in this repo; the runtime fork's changes live as an uncommitted
diff on branch `neptune-dev` in `the elastOS runtime checkout` and are saved in
`patches/elastos-runtime-v2.0.1-neptune.patch` (regenerate with `git diff` there).

First, tomorrow:
1. **Run the fork's tests**: in the runtime fork `cargo test --release -p elastos-server neptune_swap`
   (still building at pause time). This repo's tests passed over 03177b6: protocol 26, app 26,
   light-core 23, provider 17.
2. **Rebuild and restart the dev Home**: `cargo build --release -p elastos-server --bin elastos`
   in the fork, `scripts/build-intents-capsule.sh`, `scripts/install-dev-home.sh`, then
   `scripts/dev-home.sh` (the dev Home was stopped by the rebuild, exit 75).
3. **Check the manifest loads**: the app at http://localhost:61180/apps/neptune-swap/ (200),
   and that Home lists it (the manifest gained methods; `CapsuleManifest::validate` runs at load).
4. **Live tests in the dev Home** (unlock it in the browser first): Show USD prices
   (approve in Home), Connect elastOS Wallet (Base), Show open offers, publish an offer on the
   Offer page, then the carrier test between the gateway's node and the managed runtime's node
   (`target/release/examples/carrier_probe`, see `docs/carrier-route.md`).
5. **Main-net send by the user** (small amount, fee 0.1) with the light node's own proofs.

Next to build: answering takers' sealed requests from the app (solver side), the capsule's EVM
leg calls through the Wallet bridge (Base/ESC in the protocol, not Ethereum main net), proof of
work or a stake for public offers, reputation and a signed solver list, an encrypted seed.

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
- `capsules/neptune-provider/capsule.json`, `capsules/neptune-swap/capsule.json`:
  both pass the Runtime's own `CapsuleManifest::validate` (elastos-common).
- `crates/intents-ui`: the capsule's UI in **Slint and Rust** (user 2026-10-30: write in Slint
  and Rust as much as we can). Design tokens (graphite/white, gold the only
  accent) and frosted glass: Rust draws a Neptune wallpaper and a blurred copy once
  (tiny-skia, three box passes); each glass panel shows its part of
  the frost under the sheet tint. The form builds the intent with `intents-protocol` itself
  (exact `Amount::parse_decimal`), so page and solvers share one rule set. Runtime calls, the
  Home launch token and the `home:app-ready` handshake are Rust (web-sys). 7 tests.
  - Capsule page: `scripts/build-intents-capsule.sh` builds wasm32 (Slint 1.18.1, winit +
    femtovg on WebGL2) into `capsules/neptune-swap/browser/pkg/`;
    `index.html` only starts the wasm. ~1.5 MB gzipped.
  - Screenshots: WebKit snapshots do not capture WebGL, so the native build saves the window
    with Slint's `take_snapshot`: `NEPTUNE_SWAP_LOOK=dark|light NEPTUNE_SWAP_DEMO=asset|node|review
    NEPTUNE_SWAP_SHOT=out.png target/debug/intents-ui`. On the web: `?look=…&demo=…`.
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
| M2 | Runtime fork (`patches/elastos-runtime-v2.0.1-neptune.patch`): `neptune` scheme registered and routed to the capsule | **done**: the dev Home (`scripts/dev-home.sh`) verifies and registers `elastos://neptune` (the registry needs `neptune` among its reserved names); calls need a Home launch token, so the passkey comes first |
| M3 | Local regtest neptune-core node from the vendored source, provider pointed at it | **done**: `scripts/regtest-node.sh` (needs `--compose --guess --tx-proving-capability=singleproof --tx-proof-upgrading`, or blocks never carry the wallet's transactions) |
| M4 | Wallet in the provider: create/import seed, receive address, sync with light-core, balance; persisted state | **done**, verified on regtest: received 25 NPT from the node |
| M5 | Send: membership proofs from the node (`wallet_restoreMembershipProof`), `TransactionDetails` → `ProofCollection` → `wallet_submitTransaction` | **done on regtest**; **real proving built** (2026-11-16): `light_core::send::prove_collection` makes a real ProofCollection for the tip's rule set and verifies every proof before submitting; `neptune-prover` runs out of process beside the provider. One-input payment: 6 proofs in 27.8 s, prover peak ~2.6 GB (`light-cli prove-dry-run`, proven as testnet over regtest). The Runtime waits 30 s per provider reply, so `wallet_send` is a background job (`wallet_send_status`). **Not yet run against a real network**: no testnet node here (testnet-0 has no public server), and a main-net send would spend real NPT. The HTLC ops still prove inside the request and would hit the 30 s limit on a real network |
| M6 | Wallet screens in the capsule (Slint): address + QR, balance, history, send | **built**: create/back up/import, sync, balance, send (form → review → confirm), receive with Private (Generation) or Short (EC Hybrid, QR) addresses and New address, every address handed out listed per kind (payments and NPT received; open one to copy it or show its QR; `wallet_addresses` returns `issued`), activity (received/sent/pending, block and time). Seen in the dev Home: balance 14.95 NPT after sync (2026-11-16); addresses, QR and activity verified on regtest through the provider (an EC Hybrid payment shows as such), not yet clicked through in Home |
| M7 | Neptune HTLC lock/claim/refund in the provider (`crates/htlc`), swap-guard on the verified tip | **lock → claim and lock → refund done on regtest** (2026-11-16, release provider, two wallets): see "HTLC on regtest" below. swap-guard on the verified tip not wired yet; the capsule does not call the htlc ops yet (gateway list unchanged) |
| M8 | Other leg through elastOS Wallet: the fork lets the capsule request `wallet`/`chain` approvals; EVM HTLC contract and Bitcoin HTLC script on their testnets | **EVM contract done** (`contracts/htlc-evm`, 9 forge tests; `crates/evm-htlc`, 5 tests, builds for wasm32; `scripts/evm-htlc-local.sh` lock→claim and lock→refund on local anvil, chain id 8453). **Route done**: the fork lets `neptune-swap` use the Browser app's Wallet bridge (request-accounts → request-transaction → Wallet passkey approval → broadcast-transaction, reads), bound to the calling app; the dev Home's Base is the local anvil chain (`BASE_RPC_URL`). Call sequence for the capsule: `docs/wallet-bridge.md`. Not yet: the capsule's calls (UI session), the NPT↔ETH flow through it. Bitcoin: Wallet has BTC accounts and node reads but cannot send BTC, even on develop 2026-11-16 (`wallet-send-flow.js`: EVM only) |
| M9 | Solver + transport over the carrier; end-to-end swap on regtest/testnets | **Swap done over the carrier on local chains** (`crates/solver`, `CARRIER=1 scripts/swap-e2e.sh`): every message signed, sealed to its recipient's DID and sent over the dev Home's gossip after the operator approved `elastos://peer/*` in Home; user −2.05 NPT +1 ETH less gas, solver +1.95 NPT. Not yet: two separate nodes (tickets), the capsule as the user side (peer route in the fork's gateway), the reverse direction |

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
  A changed proof, PoW root, parent, clock or network fails (6 tests). Used by the wallet
  sync (see "Light node" below); not yet by swap-guard.
- **Not succinct asset management.** A wallet still has to find its coins (scan blocks, or
  the UTXO index, which reveals them) and keep membership proofs current (process each block
  while online, ask an archival node, or download the blocks it missed). Neptune's docs call
  this support "limited". A new wallet starting at the tip avoids history; a restore does not.
- **Swaps.** swap-guard today takes the block kernel from the caller. With a verified tip,
  it could check the lock's block against the tip's block MMR (an MMR membership proof of
  that block's hash) and the lock's addition record against that block: the receiver would
  then trust no node for the lock it checks. Not built.

## Light node (2026-11-16)

The provider is a light node over any node (the public one on main net, behind the dev flag):
it trusts the node neither for validity nor with which coins are the user's.

1. **Tip verified on every sync** (commit 5abcdce). When `wallet_sync` reaches the tip on
   main net, it fetches that block with its proof (`archival_getBlock`) and its parent and
   runs `verify_tip`; every block applied links to it by hash, so a valid tip vouches for
   all of them. The work must not fall below an earlier verified tip. `wallet_sync` returns
   `verified_height`; the capsule shows "Up to date at block N; its chain proof checked
   here." Main net block 63,002 verified in ~3 s. Testnet/regtest: no checkpoint, not verified.
2. **Own membership proofs.** `WalletState.mutator_set` is the mutator set after the synced
   block; each `Coin.membership_proof` is kept current block by block
   (`LightWallet::advance_proofs`: proof made at the coin's turn among the block's additions,
   then `AuthenticatedItem`/`RemovalRecord::batch_update_from_addition` per addition, then the
   removals), and the result must equal the block's own mutator set, or every proof is
   dropped (fallback). A wallet without the set (old state, a bare sync point) takes it from
   the next block, or the provider's sync adopts it at the sync point; create/import start
   with it (`start_at`). `wallet_send` uses `own_basis` when every planned coin has a proof,
   else asks the node (`wallet_restoreMembershipProof`) as before, and says which (`proofs`:
   `own`/`node`). Tests: proofs hold through receipts, others' blocks and a real removal
   record, and through JSON. Regtest end to end: a fresh wallet received 3, paid 1 (one
   input) and 2.5 (two inputs) with its own proofs, both mined.

Left: HTLC lock/claim still use the node's proofs; the swap-guard check against the verified
tip's block MMR; comparing `work_since_activation` across nodes to pick the heaviest chain;
a coin received before the wallet kept the set still asks the node once.

## Prices, wallet choice (2026-11-16)

- **USD reference prices.** `Show USD prices` in the swap form calls `neptune/prices`. The runtime
  fork's gateway first asks the user in Home (capability `elastos://neptune/prices`, read; granted
  once per gateway run); the provider then asks CoinGecko (NPT, BTC, ETH, USDC) and Kraken (BTC,
  ETH, USDC), takes the mean, flags sources more than 2 % apart, caches a minute, and only when
  its config sets `price_sources` (the dev Home does). The form shows each amount in USD, the
  unit prices and how the asked amount compares with the market; the review shows USD too.
  Display only. Deliberate exception to rule 1, opt-in per Home: the sources see this
  computer's address and when it asks, never what it swaps. Later: signed price reports over
  the carrier, so no Home asks the sources itself.
- **Wallet choice for the other leg.** elastOS Wallet owns the accounts and the approval methods
  (its own passkey, MetaMask; UniSat for Bitcoin), so the app does not pick a wallet itself: it
  asks Wallet for the account (`request-accounts`, approved in the Wallet app, 12 h per page) and
  every payment is approved the way the user set up that account. **The app bridge serves only
  Elastos ESC (`eip155:20`) and Base (`eip155:8453`), not Ethereum main net**: the protocol's
  `Chain::Ethereum` (and the USDC contract in `ASSETS`) must gain Base/ESC before a real EVM leg
  runs through Wallet. The form connects on Base (the dev Home's local chain). BTC: Wallet cannot
  send Bitcoin yet.

## Standing offers and solver mode (2026-11-16)

- **Offer** (`intents-protocol::offer`): a solver's pair, rate as two exact amounts, min/max,
  expiry (≤ 1 h), `withdrawn`. Signed with the solver's long-lived `did:key`, not sealed, on
  `neptune-swap/offers/main` (other networks: `/test`). It binds nothing: a taker still sends a
  sealed Intent to that solver and the HTLC rules protect both. `price_of` rounds up for the
  solver, `amount_for` down; 256-bit `mul_div` (NPT units exceed 128-bit products).
- **Solver identity**: Ed25519 seed from the wallet seed (`LightWallet::solver_key_seed`, own
  domain), so a restore keeps the same DID. Provider ops `solver_identity`, `offer_sign`; the key
  never leaves the provider.
- **App**: the swap page lists open offers for its pair (verified before read, expired and
  withdrawn dropped, ≤ 5 per solver, ≤ 200 in all, best rate first); tapping one fills the form
  at that rate. The Offer page publishes (copies live 20 min, republished every 10 min while the
  page is open) and withdraws.
- Not built: answering takers' sealed requests from the app (a solver still runs `crates/solver`),
  the live carrier test between the gateway's node and the managed runtime's node.

## Risks: DDoS, spam, cheating, and what elastOS gives (2026-11-16)

| Risk | Defence | elastOS part |
|---|---|---|
| Flood of intents/quotes/offers on a topic | Drop before parse (rule 5): size cap (64 KiB at the gateway, 16 KiB per message), signature first; per-DID rate limit and a small proof of work (or a stake) for intents and public offers | The gateway's peer route already caps size, forces our topic prefix and owns sender ids and cursors; rate limits per launch session belong there |
| Sybil solvers, fake offers | Long-lived solver DIDs with reputation (completed swaps, refunds), a signed solver list as the bootstrap, offers only from DIDs that can show a lock history | DIDs and the carrier resolve parties without domains; DID documents can carry the reputation pointer |
| A solver that never locks | HTLC timing: the NPT giver's lock refunds later than the other chain's; a no-show only wastes time; reputation drops | Runtime capability approvals keep every payment the user's own decision |
| Griefing locks (lock, never claim) | Short quote TTLs; refunds automatic after T; solvers price in the capital lockup | |
| DoS on the user's Home | Nothing listens (rule 2); the node runs `--no-incoming`; the provider talks only to the local runtime | Capsule sandbox (CSP, opaque origin, Home-only connect-src); providers out of process; least capability per app |
| Price manipulation | Several sources, flagged when apart; display only, never in amounts | Approval before the app may ask public sources |
| Stolen launch token | Tokens bound to the launch session; keys never in the capsule (rule 3) | Home launch tokens, Wallet passkey per payment |
| Seed theft on disk | Encrypted seed (not built: dev stores it plain, 0600) | Runtime keystore when it lands |

Not built yet: per-DID rate limits and proof of work, reputation, the signed solver list,
encrypted seed. elastOS 0.8.0 isolation gaps (see Known limits) still apply.

**Security review, 2026-11-16 (independent pass over everything since the rename), fixed:**
- Offers: tombstones for withdrawn offers (no replay), terms fixed per id (a copy may only live
  longer), taking uses exactly the row seen, assets match on chain and symbol, a full list
  evicts the soonest-ending offer, each poll drains the topic, listening only on request,
  solver IDs shown 8 + 8 characters (the fixed `z6Mk` prefix is skipped), five own offers at
  most, at most 60 offer signatures an hour in the provider.
- Gateway: topics `neptune-swap`, `neptune-swap/…`, `neptune-swap-…` only; `get_ticket` and
  `gossip_join_exact` removed; at most 4 topics per launch; sender id per launch and topic.
  Wallet bridge: apps other than the Browser are named by the gateway
  (`https://<app>.app.elastos`), never by themselves. Prices approval per user, 12 h, a no
  holds 10 min without asking again, failed polls stay pending, timeouts.
- Prices: both sources at once with 4 s timeouts, failures cached a minute; the market note
  says when a price has one source, sources disagree or it is old (no comparison then).
- Addresses: at most 25 unused per kind (`KEY_GAP`).
- Manifest declares wallet, prices, offer signing and the carrier topics.

**Still open from the review:** Sybil flooding of the public offers topic can still crowd the
list (needs proof of work or a stake per offer, and per-peer limits in the runtime's carrier);
reputation and a signed solver list (unknown solvers are not marked yet); `send` falls back to
the node's membership proofs silently, and fast restore (`apply_unordered_blocks`) does not
check blocks against the verified chain; the gateway and managed runtime carrier nodes are
still untested live.

## M8 design: the other leg through elastOS Wallet (read from runtime v2.0.1)

What elastOS 2.0.1 offers (`docs/CHAIN_PROVIDER.md`, `docs/WALLET_PROVIDER.md`):
- **EVM, yes.** The built-in managed wallet has one EVM key for ESC (chain 20) and Base (8453).
  A transaction goes chain-provider `prepare_transaction` (unsigned EIP-155 legacy intent with
  nonce, gas, chain id, to, value and data bound; contract calls allowed) → a Wallet approval the
  user signs with a fresh passkey in the Wallet app → chain-provider `broadcast_transaction`.
  Reads (`receipt`, `transaction`, `contract_call`) let the NPT side watch the other lock.
- **Bitcoin, no.** The managed Bitcoin key (P2WPKH) only signs BIP-322 proofs; chain-provider only
  reads Bitcoin. A BTC leg needs Elacity to add transaction signing (or a UniSat PSBT handoff).
- **Main nets only** in the default list (ESC, Base, BTC). chain-provider takes `BASE_RPC_URL`
  and accepts loopback without configuration, so a local EVM chain started with chain id 8453
  can stand in for Base in the dev Home: Wallet signs as usual, no real money.
- **App route:** the Browser app already has `/api/apps/browser/wallet/{request-transaction,
  read, broadcast-transaction, approvals/:id}`, gated by `require_runtime_wallet_authority(…,
  &[BROWSER_CAPSULE_ID])`. The fork can let `neptune-swap` use the same handlers.

Tooling: Foundry 1.9.0 installed in `~/.foundry/bin` (2026-11-16; binaries verified against their sigstore attestations); solc 0.8.28 is fetched by forge. Local chain: `anvil --chain-id 8453 --port 8545 --host 127.0.0.1`.

Plan: (1) an EVM HTLC contract (SHA-256 hash lock, claim to the claimer, refund after T) with
tests on a local chain; (2) ABI encoding for lock/claim/refund in Rust; (3) the fork route so
the capsule asks Wallet for those transactions; (4) the swap flow ETH ↔ NPT on the local chain,
then USDC (ERC-20 approve + lock). Tooling needed: Foundry (anvil, forge, solc), not installed.

## M9 transport (2026-11-16)

**Done:** the lock announcements are signed protocol messages (`npt_locked`, `evm_locked`,
bound to the Accept's digest and the right signer), and every message is sealed to its
recipient's `did:key` (`intents_protocol::seal`/`open`: X25519 from Ed25519, HKDF-SHA256,
ChaCha20-Poly1305 with the DID as associated data; not post-quantum yet). `scripts/swap-e2e.sh`
runs the swap with both: user −2.05 NPT +1 ETH, solver +1.95 NPT. The harness hands the sealed
bytes over; the carrier hop is what is left.

**Carrier hop done (one node).** `solver::carrier::Carrier` attaches to the node's managed
Runtime API (`home-runtime-coords.json`, shell scope), asks for `elastos://peer/*` with action
`message` (the action every `peer` op maps to), and waits while Home shows the request in its
notifications; the operator approves it there. The token is bound to that attach session, so
one `Carrier` lives for the run and each run asks again. Then `gossip_join` on a fresh topic,
`gossip_send` (`message` = sealed bytes in base64url, `sender_id` per party) and `gossip_recv`
(`consumer_id` and `skip_sender_id` per party). Run: all eight steps over the carrier, same
balances as the direct run.

Notes:
- Both parties sit on the dev Home's one node here, so messages pass through its local gossip
  buffer, not between two nodes. Two nodes: `get_ticket` on one, `gossip_join_exact` on the other.
- A shell session without a capability token may call `peer` ops directly (orchestrator
  privilege for non-write ops); the solver does not use that and goes through the approval.
- The capsule side still needs `peer` in the fork's gateway provider routes for `neptune-swap`.
- `wallet_send` proves in the background: a provider started per call exits before it sends.
  Fund test wallets with one long-lived provider process.

## HTLC on regtest (2026-11-16)

Provider ops (per network, state in `<network>/swaps.json`, 0600, plain text, development
only): `htlc_claim_lock`, `htlc_lock {amount, fee, claim_lock, refund_after_ms}` (makes and
stores the secret first: the NPT giver holds it), `htlc_status {swap_id | note + from_height}`,
`htlc_secret {swap_id}`, `htlc_claim {note, preimage, claim_index, fee, from_height}`,
`htlc_refund {swap_id, fee}`. The lock output has no on-chain notification; the counterparty
gets an `HtlcNote` (terms + coin opening). Swap keys come from the seed.

Results: lock 5 NPT → claim with the revealed secret, twice (receiver +4.95 each); lock 3 NPT →
refund before T refused → refund after T (giver 4.75 → 4.65). What this does and does not show:

- **Regtest mocks proofs, so the node never ran the HTLC script.** It accepted and mined the
  transactions (kernel rules, upgrader, composer). The script itself is checked locally before
  submitting (`light_core::send::check_witness`) and by the htlc crate's real-STARK test. Real
  enforcement by a node needs testnet/main net with real `ProofCollection` proving (not built).
- **`PrimitiveWitness::validate` cannot check scripts that read the kernel**: it gives lock
  scripts the hash of the whole witness, while `ProofCollection` gives the kernel MAST hash.
  `check_witness` runs lock scripts with the kernel hash and leaves the rest to `validate`.
  Worth reporting upstream.
- Each `TransactionDetails::primitive_witness()` call rebuilds the kernel with freshly
  encrypted notifications: a refund's timestamp path is read from the one assembled witness
  that is then proven (`prove_witness`).
- A lock submitted while the regtest node made ~100 blocks between upgrader ticks stayed in the
  mempool ("Could not find mutator set update for update job"); restarting the node cleared it
  and the next attempt went through. With real block times this should not bite, but the
  wallet needs **rebroadcast/rebuild of a stuck transaction** (not built).
- The dev Home has the new provider binary and checksum; its running providers switch at the
  next dev Home restart.

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
- Capsule I/O via `POST /api/provider/<scheme>/<op>` with capability tokens (the Runtime's gateway provider proxy).

## EVM leg (2026-11-17)

- The other leg runs on **Base** (`Chain::Base`, eip155:8453), where elastOS Wallet's app
  bridge pays; ETH and Circle's USDC there are their own assets (`asset_row(chain, symbol)`),
  distinct from Ethereum main net's. Ethereum main net stays in the protocol for native
  solvers; the app offers Base only. ESC was tried and dropped (user 2026-11-17: ELA on the
  main chain, not ESC).
- **One contract address on every chain, fixed by its creation code.** `contracts/htlc-evm`
  builds without metadata (`foundry.toml`), so its creation and runtime code are
  reproducible (`NeptuneHtlc.init.hex`, `NeptuneHtlc.runtime.hex`; a test compares a fresh
  Foundry build with them). It is deployed by `CREATE2` from the deterministic deployment
  proxy (`0x4e59…956c`) with salt 0, which puts it at `evm_htlc::HTLC_ADDRESS`
  (`0xc858…2822`) wherever that proxy exists (Base, a local anvil). Anyone may deploy it;
  nobody can put anything else there. A lock counts only at that address, and an
  `evm_locked` naming another is refused (`evm_leg::check_named_contract`). The independent
  review (2026-11-17) found that matching the runtime code alone is not enough: other
  creation code can pre-fill storage with fake locks and then leave identical runtime code,
  so a solver could make the user reveal the secret into an empty contract. The code hash
  stays as a check that the contract is deployed. MIT now (was UNLICENSED).
- Review follow-ups: logs are read in pages of 2,000 blocks from the user's own lock block
  (public RPCs cap `eth_getLogs`); a claim counts as a reveal once it leaves the page, so the
  swap flow must keep claiming and never fall back to waiting for the Neptune refund;
  `saturating_add` in the native solver's refund check. Open: the chain's own clock for
  `may_reveal` (needs a block-header read in the bridge), a block-range cap on the fork's
  `eth_getLogs`, a fork test for `APP_CHAIN_READS`.
- `crates/intents-ui/src/evm_leg.rs`: `lock_txs` (ETH: one call with value; USDC: an exact
  `approve` then `lockToken`), `claim_tx`, `refund_tx`, `check_lock` (terms, unspent, refund
  at least the chain's gap before the Neptune refund, `may_reveal`), `secret_from_logs`;
  through the bridge: `send` (request-transaction → Wallet approval → broadcast → receipt),
  `read`, `check_contract`, `lock_view`, `claimed_secret`. The Wallet connection now keeps
  the account id with the address (transactions need both).
- **Runtime fork**: the bridge's chain reads needed the Browser's internet consent, which
  only the Browser app can hold. For the other bridge apps (`neptune-swap`) reads now skip
  that consent but are limited to `eth_blockNumber`, `eth_call`, `eth_getCode`,
  `eth_getLogs`, `eth_getTransactionReceipt`, through the chain provider, audited as before
  (`gateway_browser_wallet_reads.rs`, `APP_CHAIN_READS`). Not yet covered by a fork test.
- The EVM chain's clock: the bridge has no block-header read, so `may_reveal` uses the
  local clock; the claim margin (1 h) dwarfs clock drift.

## ELA main chain: a scriptless swap (design, 2026-11-17)

The user's goal: NPT ↔ ELA on the **main chain**, not ESC. The main chain accepts only fixed
signature programs (standard, multisig, cross-chain, deposit; anything else is "unknown
signature type" in `RunPrograms`), so it has no hash lock and no refund path. ELA's
standard signatures are ECDSA on P-256. The swap can still be atomic with no change to ELA,
the way swaps with chains that have no scripts are done: all logic lives on the scripted
side, here Neptune, and the ELA side is just a key nobody holds alone.

- ELA is paid to the standard address of the point `P = Pe + Pn` (P-256): `xe` belongs to
  the ELA giver, `xn` to the NPT giver. Neither can spend it alone; whoever learns the other
  share spends with `xe + xn`.
- The NPT giver locks NPT first, in a Neptune lock script with three paths:
  - **claim** (before T1): the ELA giver's spending key, a release value `r` with
    SHA-256(r) = R, and the transaction must **publish `xe`** in a public announcement, with
    `xe·G = Pe` checked in the script;
  - **refund** (T1 to T2): the NPT giver's key, and it must **publish `xn`** (`xn·G = Pn`);
  - **punish** (after T2): the ELA giver takes the NPT.
- Steps: NPT locked → the ELA giver checks it and pays ELA to `P` → the NPT giver sees the
  ELA confirmed and sends `r` → the ELA giver claims the NPT, publishing `xe` → the NPT giver
  spends the ELA. If `r` never comes, the NPT giver must refund before T2 (publishing `xn`,
  so the ELA giver takes its ELA back) or lose the NPT to the punish path.
- Unlike today's HTLC (whose claim publishes nothing on Neptune), these paths must publish a
  value: Neptune transaction kernels carry public announcements, and the lock script can
  require one.
- **Rogue keys:** before any ELA is paid, each party proves it knows its share (a
  signature with `xe` over `Pe`, with `xn` over `Pn`, bound to the deal). Without that, the
  NPT giver could pick `Pn = Q − Pe` for a `q` it knows, making `P = Q` its key alone, and
  take the ELA. The same check guards the other direction.
- The cost: P-256 scalar multiplication in tasm (256-bit field arithmetic on u32 limbs;
  millions of cycles, proved by the claimer). That is the main work, and the main thing to
  review. To check first: that kernel announcements are bound into what the lock script
  sees (the kernel MAST hash), ELA's exact signature hash and address derivation for a
  point, and a lighter way to tie `xe` to `Pe` (a proof made off-chain once per swap).

## Provider vaults instead of pools (idea, 2026-11-17)

AMM pools put everyone's funds in one contract with an on-chain price that can be pushed
(flash loans, sandwiches) and one bug that empties it. Here each provider keeps its own
funds and quotes its own price; the board of offers is the liquidity, and every trade is a
bilateral atomic swap. A **vault** makes that liquidity provable: a provider locks X ETH or
USDC on Base (and NPT in a Neptune lock script) until a set time; the vault can only open
swap locks or return to its owner when the time runs out. Offers that name a vault are
backed on chain and rank first, which also answers spam on the public board. A watchtower
in the provider's node claims for it when it is offline.

