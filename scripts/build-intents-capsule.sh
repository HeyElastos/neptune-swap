#!/bin/sh
# Builds the Neptune Swap capsule's page: the Slint UI (crates/intents-ui) for wasm32, then
# wasm-bindgen into capsules/neptune-intents/browser/pkg/ beside the hand-written index.html.
#   scripts/build-intents-capsule.sh          release build
#   scripts/build-intents-capsule.sh --opt    also wasm-opt -Oz (cargo install wasm-opt)
# Env: CARGO_TARGET_DIR, CARGO_BUILD_JOBS (default 4).
set -eu
cd "$(dirname "$0")/.."
OPT=0
for a in "$@"; do
  case "$a" in
    --opt) OPT=1 ;;
    *) echo "unknown option $a" >&2; exit 2 ;;
  esac
done
nice -n 10 cargo build --release --target wasm32-unknown-unknown -p intents-ui -j "${CARGO_BUILD_JOBS:-4}"
out=capsules/neptune-intents/browser/pkg
rm -rf "$out"
wasm-bindgen --target web --no-typescript --out-dir "$out" \
  "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/intents-ui.wasm"
if [ "$OPT" = 1 ]; then
  nice wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int --enable-sign-ext \
    --enable-mutable-globals --enable-reference-types --enable-multivalue \
    -o "$out/opt.wasm" "$out/intents-ui_bg.wasm"
  mv "$out/opt.wasm" "$out/intents-ui_bg.wasm"
fi
ls -l "$out"
printf 'wasm gzip -9: %s bytes\n' "$(gzip -9 -c "$out/intents-ui_bg.wasm" | wc -c | tr -d ' ')"
