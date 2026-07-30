#!/bin/sh
# Live incremental-recompute test: orchestrator + runner_jaspbase.R + two-run frontend harness.
#
# Verifies the runner's copy-on-seed recompute (the "jaspBase accommodation" block): an unchanged
# plot is REUSED across revisions while a changed statistic recomputes. Uses the Bayesian One-Sample
# T-Test prior-and-posterior plot (the one jaspTTests plot that recomputes cleanly unmodified);
# rev 1 toggles the `descriptives` TABLE off, leaving the plot unchanged. The discriminator (in
# frontend_recompute.R): results_1/ must gain no new plot artifact vs results_0/.
#
# Usage: sh refactor_design/run_recompute_test.sh
set -e
cd "$(dirname "$0")/.."   # project root
ROOT="$(pwd)"

CTRL="tcp://127.0.0.1:9572"
SCRATCH="/home/sp42/neoTest"
rm -rf "$SCRATCH"; mkdir -p "$SCRATCH"   # fresh each run, preserved afterwards for inspection
export JASP_ORCH_URL="$CTRL"
export JASP_ORCH_DIR_ROOT="$SCRATCH"
export JASP_ORCH_TEST_DATASET="$ROOT/test_data/debug.arrow"
export JASP_RUNNER_LIBDIR="${JASP_RUNNER_LIBDIR:-/home/sp42/jaspModuleTools/workdir/jaspTTests}"
export JASP_RUNNER_MODULE="${JASP_RUNNER_MODULE:-jaspTTests}"
export JASP_RUNNER_MODULE_VER="${JASP_RUNNER_MODULE_VER:-0.95.5}"
export JASP_ORCH_KEEP_WORKSPACES=1   # preserve results_0/results_1 for inspection after the run

ORCH_PID=""; RUNNER_PID=""
cleanup() {
  [ -n "$RUNNER_PID" ] && kill "$RUNNER_PID" 2>/dev/null || true
  [ -n "$ORCH_PID" ]   && kill "$ORCH_PID" 2>/dev/null || true
  wait 2>/dev/null || true
  # NOTE: $SCRATCH is deliberately NOT removed — inspect results_0/results_1 after the run.
}
trap cleanup EXIT INT TERM

echo "=== building orchestrator ==="
cargo build --manifest-path orchestrator/Cargo.toml

echo "=== starting orchestrator on $CTRL (dir_root=$SCRATCH) ==="
./orchestrator/target/debug/jasp-orchestrator > /tmp/orch_recompute.log 2>&1 &
ORCH_PID=$!
sleep 1

echo "=== starting jaspbase runner (libdir=$JASP_RUNNER_LIBDIR, module=$JASP_RUNNER_MODULE $JASP_RUNNER_MODULE_VER) ==="
Rscript refactor_design/runner_jaspbase.R "$CTRL" > /tmp/runner_recompute.log 2>&1 &
RUNNER_PID=$!

echo "=== running recompute frontend driver ==="
set +e
Rscript refactor_design/frontend_recompute.R "$CTRL"
FE_EXIT=$?
set -e

echo ""
echo "===== runner log =====";       cat /tmp/runner_recompute.log
echo "===== orchestrator log ====="; cat /tmp/orch_recompute.log

echo ""
echo "workspaces preserved for inspection: $SCRATCH/<session>/w-recompute/results_0 and results_1"
if [ "$FE_EXIT" -eq 0 ]; then
  echo "=== RECOMPUTE TEST: PASS ==="
else
  echo "=== RECOMPUTE TEST: FAIL (frontend exit $FE_EXIT) ==="
fi
exit $FE_EXIT
