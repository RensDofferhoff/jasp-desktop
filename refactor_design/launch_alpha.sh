#!/bin/sh
# Launch the NEO JASP alpha stack for local testing.
# Orchestrator: tcp://127.0.0.1:9555 (frontend) + tcp://127.0.0.1:9556 (runner)

set -e

ORCH_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ORCH_PID=""
RUNNER_PID=""

cleanup() {
    echo ""
    echo "=== shutting down ==="
    [ -n "$RUNNER_PID" ] && kill "$RUNNER_PID" 2>/dev/null || true
    [ -n "$ORCH_PID" ] && kill "$ORCH_PID" 2>/dev/null || true
    wait 2>/dev/null || true
    echo "stopped."
}
trap cleanup INT TERM EXIT

mkdir -p /tmp/jasp-runner-output

echo "=== starting orchestrator ==="
cd "$ORCH_DIR"
cargo run --manifest-path orchestrator/Cargo.toml &
ORCH_PID=$!
sleep 1

echo "=== starting runner ==="
Rscript refactor_design/runner_alpha.R &
RUNNER_PID=$!

echo ""
echo "=== stack running ==="
echo "  orchestrator PID: $ORCH_PID  (tcp://127.0.0.1:9555, tcp://127.0.0.1:9556)"
echo "  runner PID:       $RUNNER_PID"
echo ""
echo "Start JASP with:  JASP_CLIENT_LOG=stub JASP_ORCH_URL=tcp://127.0.0.1:9555 /path/to/JASP"
echo "Press Ctrl-C to stop."

wait -n $ORCH_PID $RUNNER_PID 2>/dev/null || true
