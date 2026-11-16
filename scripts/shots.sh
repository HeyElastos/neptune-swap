#!/bin/sh
# Development screenshots of every capsule screen, both looks, from the native
# build (same Slint UI as the web capsule): screenshots/<look>-<state>.png.
#   scripts/shots.sh [state ...]     default: all states
#   SIZE=360x760 scripts/shots.sh    at a phone size (files end in -360x760)
set -eu
cd "$(dirname "$0")/.."
cargo build -p intents-ui >/dev/null 2>&1 || cargo build -p intents-ui
mkdir -p screenshots
STATES="${*:-form asset node review wallet-none wallet-phrase wallet-import wallet-ready wallet-send wallet-confirm}"
for look in dark light; do
  for state in $STATES; do
    out="screenshots/$look-$state${SIZE:+-$SIZE}.png"
    rm -f "$out"
    demo="$state"; [ "$state" = form ] && demo=""
    NEPTUNE_SWAP_LOOK=$look NEPTUNE_SWAP_DEMO=$demo NEPTUNE_SWAP_SHOT=$out NEPTUNE_SWAP_SIZE=${SIZE:-} \
      target/debug/intents-ui >/dev/null 2>&1 &
    pid=$!
    i=0; while [ ! -f "$out" ] && [ $i -lt 40 ]; do sleep 0.25; i=$((i+1)); done
    sleep 0.3; kill $pid 2>/dev/null || true
  done
done
ls screenshots
