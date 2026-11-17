#!/bin/sh
# Installs the Neptune Swap capsule into the isolated development Home
# (default ~/elastos-neptune-dev, see PLAN.md). Never touches the real Home:
# its components.json is pinned by the signed release.
#   scripts/install-dev-home.sh            copy capsules/neptune-swap and list it
# Env: DEV_HOME (default $HOME/elastos-neptune-dev).
set -eu
cd "$(dirname "$0")/.."
DEV_HOME="${DEV_HOME:-$HOME/elastos-neptune-dev}"
DATA="$DEV_HOME/Library/Application Support/elastos"
REAL="$HOME/Library/Application Support/elastos"
[ "$(cd "$DATA" && pwd -P)" != "$(cd "$REAL" 2>/dev/null && pwd -P || echo none)" ] || { echo "refusing: $DATA is the real Home" >&2; exit 1; }
test -f "$DATA/components.json" || { echo "no dev Home at $DATA" >&2; exit 1; }
# The Runtime refuses a principal root whose data folder others can read.
chmod 700 "$DATA"
test -f capsules/neptune-swap/browser/pkg/intents-ui_bg.wasm || { echo "build first: scripts/build-intents-capsule.sh" >&2; exit 1; }
# The app was called neptune-intents until 2026-11-16: remove that install.
rm -rf "$DATA/capsules/neptune-intents" "$DATA/capsules/neptune-swap"
cp -R capsules/neptune-swap "$DATA/capsules/neptune-swap"
python3 -I - "$DATA/components.json" <<'PY'
import json, sys
path = sys.argv[1]
manifest = json.load(open(path))
manifest["external"].pop("neptune-intents", None)
manifest["external"]["neptune-swap"] = {
    "install_path": "capsules/neptune-swap",
    "description": "Neptune Swap (development)",
    "platforms": {"*": {"install_path": "capsules/neptune-swap"}},
}
json.dump(manifest, open(path, "w"), indent=2)
PY

# The provider: release build, installed as bin/neptune-provider with its
# checksum, as the Runtime verifies before it starts a native provider.
nice -n 10 cargo build --release -p neptune-provider
install -m 0755 target/release/neptune-provider "$DATA/bin/neptune-provider"
# Real proving (testnet, main net) runs the Triton VM prover out of process; it
# must sit next to the provider. Built from the vendored neptune-core.
PROVER=vendor/neptune-core/target/release/neptune-prover
if [ -x "$PROVER" ]; then
  install -m 0755 "$PROVER" "$DATA/bin/neptune-prover"
else
  echo "warning: $PROVER missing; only mock-proof networks can send" >&2
fi
SUM="sha256:$(shasum -a 256 "$DATA/bin/neptune-provider" | cut -d' ' -f1)"
SIZE="$(stat -f %z "$DATA/bin/neptune-provider")"
python3 -I - "$DATA/components.json" "$SUM" "$SIZE" <<'PY'
import json, sys
path, checksum, size = sys.argv[1], sys.argv[2], int(sys.argv[3])
manifest = json.load(open(path))
manifest["external"]["neptune-provider"] = {
    "install_path": "bin/neptune-provider",
    "description": "Neptune wallet and node access (development)",
    "provider_runtime": {
        "role": "provider", "substrate": "native", "runtime_abi": "elastos.provider-stdio/v1",
        "execution": "native-provider", "provides": "elastos://neptune/*",
    },
    "platforms": {"darwin-arm64": {"checksum": checksum, "size": size, "install_path": "bin/neptune-provider"}},
}
json.dump(manifest, open(path, "w"), indent=2)
PY

# The fork's wallet provider (it lets this app ask Wallet for an account), when built:
#   (cd <runtime fork>/capsules/wallet-provider && cargo build --release)
#   WALLET_PROVIDER=<that target/release/wallet-provider> scripts/install-dev-home.sh
if [ -n "${WALLET_PROVIDER:-}" ]; then
  test -x "$WALLET_PROVIDER" || { echo "no wallet provider at $WALLET_PROVIDER" >&2; exit 1; }
  [ -f "$DATA/bin/wallet-provider.release" ] || cp "$DATA/bin/wallet-provider" "$DATA/bin/wallet-provider.release"
  cp "$WALLET_PROVIDER" "$DATA/bin/wallet-provider.new" && mv "$DATA/bin/wallet-provider.new" "$DATA/bin/wallet-provider"
  WSUM="sha256:$(shasum -a 256 "$DATA/bin/wallet-provider" | cut -d' ' -f1)"
  WSIZE="$(stat -f %z "$DATA/bin/wallet-provider")"
  python3 -I - "$DATA/components.json" "$WSUM" "$WSIZE" <<'PY'
import json, sys
path, checksum, size = sys.argv[1], sys.argv[2], int(sys.argv[3])
manifest = json.load(open(path))
entry = manifest["external"]["wallet-provider"]["platforms"]["darwin-arm64"]
entry["checksum"], entry["size"] = checksum, size
json.dump(manifest, open(path, "w"), indent=2)
PY
  echo "installed the fork's wallet-provider"
fi

# Home's clipboard bridge allows only listed apps; let this one copy its address.
python3 -I - "$DATA/capsules/home/browser/home-clipboard-protocol.js" <<'PY'
import sys
path = sys.argv[1]
source = open(path).read()
anchor = "const HOME_CLIPBOARD_TARGET_PURPOSE_POLICY = Object.freeze({\n"
def entry_for(app):
    return (f'  "{app}": Object.freeze({{\n'
            '    "neptune.address": Object.freeze({\n'
            '      operations: Object.freeze(["write"]),\n'
            '      maxUtf8Bytes: MAX_HOME_CLIPBOARD_TEXT_UTF8_BYTES,\n'
            '    }),\n'
            '  }),\n')
source = source.replace(entry_for("neptune-intents"), "")
if '"neptune-swap"' not in source:
    assert anchor in source, "Home's clipboard policy moved"
    source = source.replace(anchor, anchor + entry_for("neptune-swap"), 1)
open(path, "w").write(source)
PY
echo "installed neptune-swap and neptune-provider into $DATA"
