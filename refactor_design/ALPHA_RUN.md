# NEO Alpha — run it

> **NOTE — describes the ORIGINAL alpha (historical).** The architecture has since moved on:
> the orchestrator is now a single control endpoint + per-peer PAIR channels with a runner
> **provisioner** (on-demand spawn from `JASP_ORCH_LIBSET`) and **module discovery**
> (`list_modules`/`modules`, `base_uri`, pushed to the frontend), and the runner is
> `runner_jaspbase.R` (real jaspBase analyses), not the hello-world `runner_alpha.R` below.
> For the current state and next steps, see `HANDOVER-dataplane.md`, `HANDOVER-client-discovery.md`,
> and `HANDOVER-module-discovery.md`. The e2e scripts `run_{provision,recompute,modules}_test.sh`
> are the current way to run the stack.

First end-to-end slice: **frontend → orchestrator → R runner → result renders**. The
orchestrator is a **broker**: it forwards a submitted `work` to the connected R runner
(injecting `dataset_paths`/`output_dir`), and streams the runner's `result` back to the
frontend. The runner computes n/mean/sd from the injected dataset (default `test_data/debug.arrow`) and returns a hello-world
`htmlNode` (the live web form the results panel renders), so the frontend draws a visible result.
No dataset cache or module registry yet. The jaspBase hookup that runs real analyses is the next
step (see `refactor_design/jaspbase-plugin.md`).

## What was built

| Piece | Where | Notes |
|---|---|---|
| Orchestrator (Rust) | `orchestrator/` | Two PAIR v1 sockets (frontend `tcp://127.0.0.1:9555`, runner `tcp://127.0.0.1:9556`); broker forwards `work`→runner (injecting `dataset_paths`/`output_dir`) and `result`→frontend; intercepts `register` and assigns `runner_id`. |
| Runner (R) | `refactor_design/runner_alpha.R` | Registers, reads `test_data/debug.csv`, computes n/mean/sd, emits a hello-world `htmlNode` (live web form). Needs `nanonext` + `jsonlite`. |
| Client (C++) | `Desktop/jaspclient/` | `JaspClient` facade: explicit `submit(work, handler)` / `abort`; correlation-map routing; dedicated recv thread + one queued hop to the GUI thread. No polling, no signal web. |
| Frontend wiring | `Analysis::run()` → `createWorkJson()` → `submit`; result handler → `setResults` | `run()` is the single trigger funnel (`refresh()` and option-changes all reach it). `MainWindow` owns the client and dials the orchestrator. |

## Prerequisites (the container has these except Rust + nng)

- **nng** C library + headers, findable by CMake (`find_package(nng)`).
  From source: `git clone https://github.com/nanomsg/nng && cmake -B b -S nng -DBUILD_SHARED_LIBS=ON && cmake --build b && cmake --install b`
- **cargo / rustc** for the orchestrator (not in this container; install via rustup).
- The usual JASP Linux deps (already in the devcontainer).

## Build & run

```sh
# 1. Orchestrator + runner (one shot)
sh refactor_design/launch_alpha.sh
#   or by hand, in two terminals:
#     cargo run --manifest-path orchestrator/Cargo.toml --bin jasp-orchestrator   # tcp://127.0.0.1:9555 (workspace: add --bin for a specific binary)
#     Rscript refactor_design/runner_alpha.R              # dials :9556

# 2. JASP (separate terminal) — normal Linux build, e.g. in the devcontainer
CMAKE_PREFIX_PATH=/opt/Qt/6.10.3/gcc_64/lib/cmake cmake -GNinja -S . -B jasp-build
cmake --build jasp-build --target JASP -j6
JASP_CLIENT_LOG=stub ./jasp-build/Desktop/JASP
#   dials tcp://127.0.0.1:9555 by default; override with: JASP_ORCH_URL=tcp://... 
```

## Expected

1. Start the orchestrator + runner first (`launch_alpha.sh`). The client dials non-blocking
   and reconnects, so order is forgiving, but start the stack first to be safe.
2. Start JASP, open/create an analysis and change an option (or otherwise trigger a run).
3. The orchestrator logs `fe->rn work work_id=aN …` then `rn->fe result work_id=aN …`; the
   runner logs `<- work` / `-> result`; `JASP_CLIENT_LOG=stub` logs the client's TX/RX.
4. The analysis renders a **"Hello World" block** listing the computed values (n/mean/sd for
   each numeric column in `test_data/debug.csv`) in the results panel.

## Known alpha limits (by design)

- One hardcoded dataset id (`ds-001` → `test_data/debug.arrow`); the orchestrator injects the
  absolute path (env `JASP_ORCH_TEST_DATASET`), the runner reads the Feather file directly via
  `read_jasp_data()` (no orchestrator-owned Arrow cache yet — that's Phase 3).
- The runner emits a hello-world `htmlNode` for *any* work regardless of analysis/options (it
  advertises one `analysis` capability `base-r@0.1`; the orchestrator broadcasts to the single
  runner). The jaspBase hookup (`jaspbase-plugin.md`) replaces this with real analysis execution.
- No abort handling on the orchestrator side (it just forwards); the client drops a superseded
  work's handler on re-submit.
- Registration is logged, not stored — with one runner, broadcast == unicast; pipe-targeted
  routing is a runner-pool concern (§9.3).
- `find_package(nng REQUIRED)` makes nng a hard dependency of the desktop build.
