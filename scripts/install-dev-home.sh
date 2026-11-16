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
echo "installed neptune-intents into $DATA"
