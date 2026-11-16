#!/bin/sh
# Runs the isolated development Home (~/elastos-neptune-dev) on the patched
# Runtime (branch neptune-dev, patches/elastos-runtime-v2.0.1-neptune.patch),
# with the neptune provider pointed at the local regtest node.
# Open http://localhost:61180/apps/home/ (exactly localhost, for passkeys).
set -eu
DEV_HOME="${DEV_HOME:-$HOME/elastos-neptune-dev}"
RUNTIME="${ELASTOS_RUNTIME:?set ELASTOS_RUNTIME to the patched elastos binary (see PLAN.md)}"
export HOME="$DEV_HOME"
export ELASTOS_COLLABORATION_STARTUP_MODE=isolated
export NEPTUNE_PROVIDER_CONFIG='{"networks":[{"id":"neptune-regtest","rpc":"http://127.0.0.1:9797"}]}'
export NEPTUNE_PROVIDER_DATA="$DEV_HOME/neptune-provider-data"
cd "$DEV_HOME"
exec "$RUNTIME" gateway --addr localhost:61180
