#!/bin/sh
# Module discovery end-to-end test: orchestrator started WITH a libset; the frontend queries the
# catalog (libset-derived), a fake runner advertising a NEW module attaches → push; it exits →
# push. The fake runner (probe_fake_runner.R) only registers — no analysis engine — so this
# exercises discovery (list_modules + push-on-actual-change + base_uri on the wire), nothing else.
#
# Usage: sh refactor_design/run_modules_test.sh
set -e
cd "$(dirname "$0")/.."   # project root
ROOT="$(pwd)"

CTRL="tcp://127.0.0.1:9575"
LIBSET="/home/sp42/jaspModuleTools/workdir"
WORKSPACES="${MODULES_WORKSPACES:-/home/sp42/neoTest-modules}"
rm -rf "$WORKSPACES"; mkdir -p "$WORKSPACES"

export JASP_ORCH_URL="$CTRL"
export JASP_ORCH_LIBSET="$LIBSET"
export JASP_ORCH_DIR_ROOT="$WORKSPACES"
export JASP_ORCH_TEST_DATASET="$ROOT/test_data/debug.arrow"
export JASP_ORCH_KEEP_WORKSPACES=1
export MODULES_FAKE_RUNNER="$ROOT/refactor_design/probe_fake_runner.R"

ORCH_PID=""
cleanup() {
  [ -n "$ORCH_PID" ] && kill "$ORCH_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "=== building orchestrator ==="
cargo build --manifest-path orchestrator/Cargo.toml

echo "=== starting orchestrator on $CTRL (libset=$JASP_ORCH_LIBSET) ==="
./orchestrator/target/debug/jasp-orchestrator > /tmp/orch_modules.log 2>&1 &
ORCH_PID=$!
sleep 1

echo "=== running discovery frontend driver (spawns + kills its own fake runner) ==="
set +e
Rscript refactor_design/frontend_modules.R "$CTRL"
FE_EXIT=$?
set -e

echo ""
echo "===== fake runner log =====";    cat /tmp/modules_test_fake_runner.log 2>/dev/null || true
echo "===== orchestrator log =====";   cat /tmp/orch_modules.log
if [ "$FE_EXIT" -eq 0 ]; then
  echo "=== MODULES TEST: PASS ==="
else
  echo "=== MODULES TEST: FAIL (frontend exit $FE_EXIT) ==="
fi
exit $FE_EXIT
