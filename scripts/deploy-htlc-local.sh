#!/bin/sh
# Puts NeptuneHtlc at its one address (evm_htlc::HTLC_ADDRESS) on a LOCAL chain: CREATE2
# from the deterministic deployment proxy (anvil has it at the same address as Base) with
# salt 0 and the checked-in creation code, then checks the deployed code's hash. Refuses
# any RPC that is not on this machine. Prints the address.
#   scripts/deploy-htlc-local.sh [rpc]      default http://127.0.0.1:8545
set -eu
cd "$(dirname "$0")/.."
RPC="${1:-${EVM_RPC:-http://127.0.0.1:8545}}"
case "$RPC" in http://127.0.0.1:*|http://localhost:*) ;; *) echo "refusing non-local RPC $RPC" >&2; exit 1 ;; esac
CAST=~/.foundry/bin/cast
KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 # anvil's published test account 0
DEPLOYER=0x4e59b44847b379578588920ca78fbf26c0b4956c
CONTRACT=0xdd09a1ceac5cbaf9460f07c261755a434e8b85d2
if [ "$($CAST code $CONTRACT --rpc-url "$RPC")" = "0x" ]; then
  $CAST send $DEPLOYER 0x$(printf '%064d' 0)$(cat contracts/htlc-evm/NeptuneHtlc.init.hex) \
    --private-key $KEY --rpc-url "$RPC" >/dev/null
fi
[ "$($CAST keccak "$($CAST code $CONTRACT --rpc-url "$RPC")")" = 0x3dcc7f166631c04d08722536f16312285a401e60317d09d0b9e64be0cf572ee0 ] \
  || { echo "NeptuneHtlc is not at $CONTRACT" >&2; exit 1; }
echo $CONTRACT
