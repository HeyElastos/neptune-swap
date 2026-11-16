#!/bin/sh
# One NPT -> ETH swap from start to finish on local chains (no real money):
# the regtest Neptune node (scripts/regtest-node.sh, 127.0.0.1:9797) and anvil
# with Base's chain id (anvil --chain-id 8453 --port 8545 --host 127.0.0.1).
# Deploys NeptuneHtlc, then runs crates/solver's swap-e2e with two provider
# wallets that hold regtest NPT.
#   scripts/swap-e2e.sh [user-npt-data] [solver-npt-data]
set -eu
cd "$(dirname "$0")/.."
USER_DATA="${1:-$HOME/elastos-neptune-dev/neptune-provider-data}"
SOLVER_DATA="${2:-$HOME/elastos-neptune-dev/neptune-provider-data-2}"
RPC=http://127.0.0.1:8545
DEPLOYER_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 # anvil test account 0
cargo build -q --release -p neptune-provider
cargo build -q -p solver --bin swap-e2e
CONTRACT=$(cd contracts/htlc-evm && ~/.foundry/bin/forge create src/NeptuneHtlc.sol:NeptuneHtlc --rpc-url $RPC \
  --private-key $DEPLOYER_KEY --broadcast 2>/dev/null | awk '/Deployed to:/ {print $3}')
echo "NeptuneHtlc at $CONTRACT"
exec target/debug/swap-e2e "$CONTRACT" "$USER_DATA" "$SOLVER_DATA"
