#!/bin/sh
# Fetch neptune-core v0.20.1 into vendor/ and apply the wasm32 patch.
set -eu
cd "$(dirname "$0")/.."
REV=404f7379
DEST=vendor/neptune-core
if [ ! -d "$DEST/.git" ]; then
  mkdir -p vendor
  git clone --quiet https://github.com/Neptune-Crypto/neptune-core.git "$DEST"
fi
git -C "$DEST" fetch --quiet --tags origin
git -C "$DEST" checkout --quiet --force v0.20.1
test "$(git -C "$DEST" rev-parse --short=8 HEAD)" = "$REV"
git -C "$DEST" apply ../../patches/neptune-core-wasm32.patch
echo "vendor/neptune-core at v0.20.1 ($REV) with patches/neptune-core-wasm32.patch"
