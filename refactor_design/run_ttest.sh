#!/bin/sh
# Live t-test round trip over the dataset manager: orchestrator + Rust data-runner (CSV lane)
# + runner_jaspbase.R + R frontend. The data lane is AUTO-PROVISIONED: the orchestrator
# spawns jasp-data-runner at boot and keeps it alive. The frontend opens test_data/debug.csv
# by submitting a data_open WORK (→ terminal result with the dataset_id), then a real
# jaspBase::runJaspResults() t-test runs on the CONVERTED Feather via dataset_paths.
#
# Usage: sh refactor_design/run_ttest.sh
set -e
cd "$(dirname "$0")/.."   # project root
ROOT="$(pwd)"

CTRL="tcp://127.0.0.1:9571"
export JASP_ORCH_URL="$CTRL"
export JASP_RUNNER_LIBDIR="${JASP_RUNNER_LIBDIR:-/home/sp42/jaspModuleTools/workdir/jaspTTests}"

ORCH_PID=""; RUNNER_PID=""
cleanup() {
  [ -n "$RUNNER_PID" ] && kill "$RUNNER_PID" 2>/dev/null || true
  [ -n "$ORCH_PID" ]   && kill "$ORCH_PID"   2>/dev/null || true
  wait 2>/dev/null || true
}
trap cleanup EXIT INT TERM

echo "=== building orchestrator + data-runner ==="
cargo build --manifest-path orchestrator/Cargo.toml

echo "=== starting orchestrator on $CTRL (auto-provisions the data lane) ==="
./orchestrator/target/debug/jasp-orchestrator > /tmp/orch_ttest.log 2>&1 &
ORCH_PID=$!
sleep 1

echo "=== starting jaspbase runner (libdir=$JASP_RUNNER_LIBDIR) ==="
Rscript refactor_design/runner_jaspbase.R "$CTRL" > /tmp/runner_ttest.log 2>&1 &
RUNNER_PID=$!

echo "=== running frontend t-test driver ==="
set +e
Rscript refactor_design/frontend_ttest.R "$CTRL"
FE_EXIT=$?
set -e

echo ""
echo "===== runner log =====";     cat /tmp/runner_ttest.log
echo "===== orchestrator log (incl. data lane) ====="; cat /tmp/orch_ttest.log

echo ""
if [ "$FE_EXIT" -eq 0 ]; then
  echo "=== T-TEST ROUND TRIP: PASS ==="
else
  echo "=== T-TEST ROUND TRIP: FAIL (frontend exit $FE_EXIT) ==="
fi
exit $FE_EXIT
