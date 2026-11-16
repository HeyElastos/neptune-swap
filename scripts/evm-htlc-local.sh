#!/bin/sh
# End to end on a local anvil chain: deploy NeptuneHtlc, then lock -> claim and
# lock -> refund, with the calldata from crates/evm-htlc (the same bytes the
# capsule will hand to elastOS Wallet). Uses anvil's published test accounts on
# a chain that exists only on this machine; refuses any other RPC.
#   anvil --chain-id 8453 --port 8545 --host 127.0.0.1 &   # stands in for Base
#   scripts/evm-htlc-local.sh
set -eu
cd "$(dirname "$0")/.."
RPC="${EVM_RPC:-http://127.0.0.1:8545}"
case "$RPC" in http://127.0.0.1:*|http://localhost:*) ;; *) echo "refusing non-local RPC $RPC" >&2; exit 1 ;; esac
CAST=~/.foundry/bin/cast
FORGE=~/.foundry/bin/forge
cargo build -q -p evm-htlc --example calldata
CD=target/debug/examples/calldata

# anvil's documented default dev accounts 0, 1, 2 (test keys, local chain only).
ALICE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80
BOB_KEY=0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d
CAROL_KEY=0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a
ALICE=$($CAST wallet address --private-key $ALICE_KEY)
BOB=$($CAST wallet address --private-key $BOB_KEY)
ZERO=0x0000000000000000000000000000000000000000
echo "chain $($CAST chain-id --rpc-url "$RPC"), Alice $ALICE locks, Bob $BOB receives"

CONTRACT=$(cd contracts/htlc-evm && $FORGE create src/NeptuneHtlc.sol:NeptuneHtlc --rpc-url "$RPC" \
  --private-key $ALICE_KEY --broadcast 2>/dev/null | awk '/Deployed to:/ {print $3}')
echo "NeptuneHtlc at $CONTRACT"

now() { $CAST block latest --field timestamp --rpc-url "$RPC"; }
bal() { $CAST balance "$1" --rpc-url "$RPC"; }

# --- lock -> claim ----------------------------------------------------------
SECRET=0x$(openssl rand -hex 32)
H=$($CD hash-lock $SECRET)
T=$(( $(now) + 3600 ))
AMOUNT=1000000000000000000
$CAST send "$CONTRACT" $($CD lock $BOB $H $T) --value $AMOUNT --private-key $ALICE_KEY --rpc-url "$RPC" >/dev/null
ID=$($CD lock-id $ALICE $BOB $ZERO $AMOUNT $H $T)
ONCHAIN_ID=$($CAST call "$CONTRACT" "lockId(address,address,address,uint256,bytes32,uint64)(bytes32)" $ALICE $BOB $ZERO $AMOUNT $H $T --rpc-url "$RPC")
[ "$ID" = "$ONCHAIN_ID" ] || { echo "lock id differs: $ID vs $ONCHAIN_ID" >&2; exit 1; }
echo "locked 1 ETH, id $ID (matches the contract's lockId)"
BOB_BEFORE=$(bal $BOB)
# Carol submits the claim: anyone may, only Bob is paid.
RECEIPT=$($CAST send "$CONTRACT" $($CD claim $ID $SECRET) --private-key $CAROL_KEY --rpc-url "$RPC" --json)
BOB_AFTER=$(bal $BOB)
[ "$(echo "$BOB_AFTER - $BOB_BEFORE" | bc)" = "$AMOUNT" ] || { echo "Bob was not paid 1 ETH" >&2; exit 1; }
echo "claimed: Bob +1 ETH"
LOG=$(echo "$RECEIPT" | python3 -I -c "import json,sys; l=json.load(sys.stdin)['logs'][0]; print(l['address'], l['topics'][0], l['topics'][1], l['data'])")
set -- $LOG
LEARNED=$($CD secret-from-log "$1" $ID $H "$2" "$3" "$4")
[ "$LEARNED" = "$SECRET" ] || { echo "the Claimed log did not yield the secret" >&2; exit 1; }
echo "the Claimed log yields the secret, which opens the hash lock"

# --- lock -> refund ---------------------------------------------------------
SECRET2=0x$(openssl rand -hex 32)
H2=$($CD hash-lock $SECRET2)
T2=$(( $(now) + 60 ))
HALF=500000000000000000
$CAST send "$CONTRACT" $($CD lock $BOB $H2 $T2) --value $HALF --private-key $ALICE_KEY --rpc-url "$RPC" >/dev/null
ID2=$($CD lock-id $ALICE $BOB $ZERO $HALF $H2 $T2)
if $CAST send "$CONTRACT" $($CD refund $ID2) --private-key $ALICE_KEY --rpc-url "$RPC" >/dev/null 2>&1; then
  echo "a refund before T went through" >&2; exit 1
fi
echo "refund before T refused"
$CAST rpc evm_increaseTime 61 --rpc-url "$RPC" >/dev/null
$CAST rpc evm_mine --rpc-url "$RPC" >/dev/null
if $CAST send "$CONTRACT" $($CD claim $ID2 $SECRET2) --private-key $BOB_KEY --rpc-url "$RPC" >/dev/null 2>&1; then
  echo "a claim after T went through" >&2; exit 1
fi
echo "claim after T refused"
CONTRACT_BEFORE=$(bal $CONTRACT)
$CAST send "$CONTRACT" $($CD refund $ID2) --private-key $CAROL_KEY --rpc-url "$RPC" >/dev/null
[ "$(echo "$CONTRACT_BEFORE - $(bal $CONTRACT)" | bc)" = "$HALF" ] || { echo "the refund did not leave the contract" >&2; exit 1; }
echo "refunded 0.5 ETH to Alice after T"
echo "ok"
