#!/bin/sh
# A local regtest neptune-core for developing the wallet and swaps: mock proofs
# and proof of work (no real money), a block about every 100 ms, no peers, no
# incoming connections, JSON-RPC on 127.0.0.1:9797 (CLI on 9798).
#   scripts/regtest-node.sh            run in the foreground
# The node must compose and upgrade third-party proofs, or blocks never carry
# the wallet's ProofCollection transactions. Its mempool lives in memory: a
# restart drops unconfirmed transactions.
# Build first: (cd vendor/neptune-core && cargo build --release -p neptune-cash --bin neptune-core -p neptune-core-cli --bin neptune-cli)
# (leveldb-sys needs cmake; with cmake 4 set CMAKE_POLICY_VERSION_MINIMUM=3.5).
set -eu
cd "$(dirname "$0")/.."
DATA="${REGTEST_DATA:-$HOME/elastos-neptune-dev/neptune-regtest}"
mkdir -p "$DATA"
exec vendor/neptune-core/target/release/neptune-core --network regtest --data-dir "$DATA" \
  --no-incoming --max-num-peers 0 \
  --listen-rpc 127.0.0.1:9797 --rpc-modules node,chain,wallet,mining,archival,mempool --rpc-port 9798 \
  --compose --guess --guesser-threads 1 --tx-proving-capability=singleproof --tx-proof-upgrading
