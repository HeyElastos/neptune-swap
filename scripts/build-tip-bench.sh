#!/bin/sh
# Builds capsule/bench: light-wasm's tip verifier plus the main-net fixtures, for
# timing succinct validation in a browser. Serve the folder over http and open
# index.html; it prints the time per verification.
set -eu
cd "$(dirname "$0")/.."
nice -n 10 cargo build --release --target wasm32-unknown-unknown -p light-wasm -j "${CARGO_BUILD_JOBS:-4}"
rm -rf capsule/bench/pkg
wasm-bindgen --target web --no-typescript --out-dir capsule/bench/pkg \
  "${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown/release/light_wasm.wasm"
cp crates/light-core/fixtures/main-tip.json.gz crates/light-core/fixtures/main-parent.json.gz capsule/bench/
ls -l capsule/bench/pkg
