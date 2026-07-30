#!/bin/sh
# Provisioner end-to-end test: orchestrator with provisioner enabled, NO manual runner.
# The orchestrator must spawn a real Rscript runner when work is submitted.
#
# Usage: sh refactor_design/run_provision_test.sh
set -e
cd "$(dirname "$0")/.."   # project root
ROOT="$(pwd)"

CTRL="tcp://127.0.0.1:9573"
WORKSPACES="/home/sp42/neoTest-provision"
rm -rf "$WORKSPACES"; mkdir -p "$WORKSPACES"
export JASP_ORCH_URL="$CTRL"
export JASP_ORCH_LIBSET="/home/sp42/jaspModuleTools/workdir"
export JASP_ORCH_DIR_ROOT="$WORKSPACES"
export JASP_ORCH_TEST_DATASET="$ROOT/test_data/debug.arrow"
export JASP_ORCH_RUNNER_SCRIPT="$ROOT/refactor_design/runner_jaspbase.R"
export JASP_ORCH_KEEP_WORKSPACES=1

ORCH_PID=""
cleanup() {
  [ -n "$ORCH_PID" ] && kill "$ORCH_PID" 2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "=== building orchestrator ==="
cargo build --manifest-path orchestrator/Cargo.toml

echo "=== starting orchestrator on $CTRL (libset=$JASP_ORCH_LIBSET, dir_root=$WORKSPACES) ==="
./orchestrator/target/debug/jasp-orchestrator > /tmp/orch_provision.log 2>&1 &
ORCH_PID=$!
sleep 1

echo "=== running provision frontend driver (NO manual runner — provisioner must spawn one) ==="
set +e
Rscript refactor_design/frontend_provision.R "$CTRL"
FE_EXIT=$?
set -e

echo ""
echo "===== orchestrator log ====="; cat /tmp/orch_provision.log
echo ""
echo "workspaces preserved for inspection: $WORKSPACES/<session>/w-provision/results_0"
if [ "$FE_EXIT" -eq 0 ]; then
  echo "=== PROVISION TEST: PASS ==="
else
  echo "=== PROVISION TEST: FAIL (frontend exit $FE_EXIT) ==="
fi
exit $FE_EXIT
