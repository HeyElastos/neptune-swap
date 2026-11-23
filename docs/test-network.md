# Test network: run Neptune Swap with no real money

Everything here runs on one machine, on chains that exist only there: a Neptune **regtest**
node (mock proofs and proof of work, a block about every 100 ms) and a local **anvil** chain
that takes Base's chain id (8453), with the swap contract at its fixed address. No main net,
no real funds, no ports open to the outside (every service binds 127.0.0.1).

Needs: Rust (stable), [Foundry](https://getfoundry.sh) (`anvil`, `cast`, `forge`), cmake
(for neptune-core's leveldb; with cmake 4 set `CMAKE_POLICY_VERSION_MINIMUM=3.5`), macOS
or Linux.

## 1. Build

```bash
scripts/vendor-neptune.sh            # neptune-core v0.20.1 into vendor/, with the wasm32 patch
(cd vendor/neptune-core && cargo build --release -p neptune-cash --bin neptune-core -p neptune-core-cli --bin neptune-cli)
cargo build --release -p neptune-provider
cargo test --release -p light-core -p neptune-provider -p intents-protocol -p intents-ui -p evm-htlc
(cd contracts/htlc-evm && forge test)
```

## 2. The two chains

```bash
scripts/regtest-node.sh                                   # terminal 1: Neptune regtest, RPC 127.0.0.1:9797
anvil --chain-id 8453 --port 8545 --host 127.0.0.1        # terminal 2: "Base", local only
scripts/deploy-htlc-local.sh                              # puts NeptuneHtlc at 0x87a6…b2e4
scripts/evm-htlc-local.sh                                 # lock → claim, lock → refund on anvil
```

The contract's address is fixed by its creation code (CREATE2 from the deterministic
deployment proxy, salt 0), so it is the same on anvil as it would be on Base. The app accepts
a lock only there (docs/swap-safety.md, section 4).

## 3. A whole swap without elastOS

Two provider wallets swap NPT and ETH end to end, both ways: the app's own swap engine as
the user against the solver's `Deal` (`crates/intents-ui/tests/swap_e2e.rs`), with the
provider binary driven over stdio exactly as the Runtime drives it:

```bash
cargo build --release -p neptune-provider && scripts/deploy-htlc-local.sh
NEPTUNE_SWAP_E2E=/tmp/npt-user,/tmp/npt-solver \
  cargo test --release -p intents-ui --test swap_e2e -- --ignored --nocapture
```

Both wallets need regtest NPT first. Create each and get an address from the provider:

```bash
for who in user solver; do
  for op in wallet_create wallet_receive; do
    echo "{\"op\":\"$op\",\"network\":\"neptune-regtest\"}" |
      NEPTUNE_PROVIDER_CONFIG='{"networks":[{"id":"neptune-regtest","rpc":"http://127.0.0.1:9797"}]}' \
      NEPTUNE_PROVIDER_DATA=/tmp/npt-$who target/release/neptune-provider
  done
done
```

then send regtest NPT to each address from the regtest node's own wallet (`neptune-cli`,
port 9798), which mines its own blocks.
Each run goes intent, quote, accept, NPT lock (checked on the receiver's own node), ETH
lock, the claim on Base (the secret goes public), the NPT claim; in the NPT → ETH run the
solver also restarts from its journal right after its ETH lock.

## 4. Inside elastOS

This part needs the Runtime patch until the general features in `docs/runtime-hooks.md`
exist; with them it should need none (roadmap check P2-10).

1. Apply `patches/elastos-runtime-v2.0.1-neptune.patch` to elastos-runtime v2.0.1 and build:
   `cargo build --release -p elastos-server --bin elastos`.
2. A development Home in its own folder (`DEV_HOME`, default `~/elastos-neptune-dev`): a
   Home data folder whose release files can be changed (ours is a copy of an installed
   2.0.1's `bin`, `capsules`, `config` and `components.json`; a source Home of your fork
   works the same). The scripts refuse to touch the real Home.
3. `scripts/build-intents-capsule.sh` (the page, wasm32), then `scripts/install-dev-home.sh`
   (lists the app and the provider, with the provider binary's checksum).
4. `ELASTOS_RUNTIME=<the patched elastos binary> scripts/dev-home.sh`, then open
   `http://localhost:61180/apps/home/` (exactly `localhost`: passkeys), create the dev
   Home's passkey, and open Neptune Swap.
5. In the app: pick the `neptune-regtest` network, create a wallet (Wallet tab), fund it
   from the regtest node, connect elastOS Wallet (anvil serves as Base: `BASE_RPC_URL`
   defaults to `http://127.0.0.1:8545`), and import one of anvil's published test keys into
   that Wallet for play money.

## What to expect, and what is not there yet

- Works: the Neptune light wallet on regtest (send, receive, history), offers on the carrier,
  USD reference prices (after the Home approval), the Wallet connection on Base, the swap
  contract on anvil, the full swap both ways in `swap_e2e`.
- In progress: the swap flow inside the app (the engine is in `crates/intents-ui/src/swap.rs`;
  the screen that drives it comes next) and a solver that answers the app over the carrier.
- Open before any main net use: see `docs/swap-safety.md`.
