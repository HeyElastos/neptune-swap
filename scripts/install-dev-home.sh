#!/bin/sh
# Installs the Neptune Intents capsule into the isolated development Home
# (default ~/elastos-neptune-dev, see PLAN.md). Never touches the real Home:
# its components.json is pinned by the signed release.
#   scripts/install-dev-home.sh            copy capsules/neptune-intents and list it
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
test -f capsules/neptune-intents/browser/pkg/intents-ui_bg.wasm || { echo "build first: scripts/build-intents-capsule.sh" >&2; exit 1; }
rm -rf "$DATA/capsules/neptune-intents"
cp -R capsules/neptune-intents "$DATA/capsules/neptune-intents"
python3 -I - "$DATA/components.json" <<'PY'
import json, sys
path = sys.argv[1]
manifest = json.load(open(path))
manifest["external"]["neptune-intents"] = {
    "install_path": "capsules/neptune-intents",
    "description": "Neptune Intents (development)",
    "platforms": {"*": {"install_path": "capsules/neptune-intents"}},
}
json.dump(manifest, open(path, "w"), indent=2)
PY

# The provider: release build, installed as bin/neptune-provider with its
# checksum, as the Runtime verifies before it starts a native provider.
nice -n 10 cargo build --release -p neptune-provider
install -m 0755 target/release/neptune-provider "$DATA/bin/neptune-provider"
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

# Home's clipboard bridge allows only listed apps; let this one copy its address.
python3 -I - "$DATA/capsules/home/browser/home-clipboard-protocol.js" <<'PY'
import sys
path = sys.argv[1]
source = open(path).read()
anchor = "const HOME_CLIPBOARD_TARGET_PURPOSE_POLICY = Object.freeze({\n"
entry = ('  "neptune-intents": Object.freeze({\n'
         '    "neptune.address": Object.freeze({\n'
         '      operations: Object.freeze(["write"]),\n'
         '      maxUtf8Bytes: MAX_HOME_CLIPBOARD_TEXT_UTF8_BYTES,\n'
         '    }),\n'
         '  }),\n')
if '"neptune-intents"' not in source:
    assert anchor in source, "Home's clipboard policy moved"
    open(path, "w").write(source.replace(anchor, anchor + entry, 1))
PY
echo "installed neptune-intents and neptune-provider into $DATA"
