# HANDOVER — NEO PoC, current state (pick up here)

> You are taking over the NEO JASP refactor PoC mid-flight. Read this, then
> `refactor_design/neo-jasp.md` (the **normative spec** — PART II is the wire protocol),
> then `refactor_design/GUT_TODO.md` (deferred gut items) and `refactor_design/poc-handoff.md`
> (the original PoC plan). This file is the *status*; the spec wins on any conflict.
> (The registration model is now synced code↔spec — §9.3/§19.5 — so that earlier exception is
> closed.)

## 0. Working with the user

Terse; dislikes long chat summaries — put detail in docs. Wants decisive opinions, not
"it depends"; give a recommendation, then the caveat. Building toward a clean design and
happy to burn legacy code. Pattern that works: **opinion → fold into the spec/docs → offer the
next step.** "continue"/"go" = do the thing. They engage hard on design decisions (the
`kind`-vs-`services`-vs-`capabilities` discussion went several rounds) — expect to reason, not
just execute. **They are wary of mess**: verify before stacking new work, and keep code and
spec in sync.

## 1. Where we are (one paragraph)

The legacy monolith path is **gutted**. Correlation (`work_id`/`revision`) is settled. The
orchestrator is a **broker**: two NNG **PAIR v1** (polyamorous) sockets, two blocking threads —
frontend→runner (injecting `dataset_paths`/`output_dir`) and runner→frontend. Runners
**register** before the work loop; the orchestrator **assigns** the canonical `runner_id`.
The registration model (unified `capabilities` tagged by `kind` + `environment`) is
**live-verified** (16/16 integration test) and **synced to the spec** (`neo-jasp.md` §9.3/§19.5).
**The jaspBase hookup now works end-to-end**: a real analysis submitted from JASP flows
frontend → orchestrator → the R runner (`runner_jaspbase.R`), which runs it through
`jaspBase::runJaspResults()` (the *engine* entry point — not the syntax/wrapper path, not
jaspTools), and jaspBase's **native** serialization renders a real results table in JASP —
verified with an independent-samples t-test from `jaspTTests`. The runner defines bridge natives
in `globalenv()` to stand in for the gutted C++ engine (data / temp / value natives) and unwraps
the frontend's `{value, types}` option form. What remains is *generalization*: the runner is
still alpha-shaped (single module via env, identity column-encoding), though Phase 1 tidying has
landed and the **data plane's thin slice is in** — it now reads **absolute Arrow/Feather** datasets
(via `jaspRunner/R/data.R::read_jasp_data()`), uses a **per-work** scratchpad with an honest cwd,
and gates its verbose logging. The orchestrator's registry + disconnect detection are done;
routing is still broadcast (single runner). See
§4 for the phased roadmap (tidy runner → registration & disconnect → data plane → control plane).

## 2. What's built & verified

**The gut (done, grep-clean):**
- Deleted `Desktop/engine/`, `CommonData/ipcchannel.*`/`rbridge.*`/`databridge.*`, the
  `Engine/` process files, `Tests/testengine.*`. **Kept** `Engine/jaspBase/`.
- De-R'd / de-IPC'd the build (removed `R-Interface` + `Engine` subdirs, excluded `SyntaxInterface`).
- **Display-only `Analysis` status**: `{ Empty, Running, Complete, Aborted, ValidationError,
  FatalError }`. Work triggered by explicit `submit`, never by setting a status.

**Orchestrator (Rust, `orchestrator/`) — BROKER + runner registry, compiles + 6 tests (5 pass, 1 cross-lang ignored):**
- `cargo clippy --all-targets` clean; `cargo test` 5/5 pass (inproc + ipc round-trips, register →
  evict-on-disconnect, work-errors-when-no-live-runner) + 1 `#[ignore]`d cross-language Rust↔R test
  (`cargo test -- --ignored r_runner_disconnect`).
- Two NNG PAIR v1 sockets: frontend `tcp://127.0.0.1:9555`, runner `tcp://127.0.0.1:9556`.
  Both polyamorous. `JASP_ORCH_URL` / `JASP_RUNNER_URL` env vars override.
- `serve_broker`: two blocking threads. Thread 1: `fe.recv()` → `inject_dataset_paths` (injects an
  **absolute** dataset path — env `JASP_ORCH_TEST_DATASET`, default `test_data/debug.arrow`
  resolved against the orch cwd — and a **per-work** `output_dir` `/tmp/jasp-runner-output/<work_id>`) →
  `rn.send()`. Main thread: `rn.recv()` → intercept `Register` (assign `runner_id`, ack, log,
  **store in registry**) → forward `Result`/other to `fe.send()`. No polling, no `RecvTimeout`, no
  busy spins.
- **Disconnect detection (Phase 2 core, DONE):** `Socket::pipe_notify` callback maintains a
  `Pipe → {runner_id, capabilities}` registry (keyed by `Pipe`, which is `Eq + Hash`); `RemovePost`
  evicts the runner. When a `work` arrives and no live runner can serve it (but one had registered
  before), the broker returns a `fatalError` result instead of broadcasting into the void (NNG
  discards sends to a gone peer with no error). Routing is still broadcast (correct for one runner);
  pipe-targeted routing via `set_pipe` is the remaining Phase 2 item for multi-runner.
- **Registration model (NEW this session, in code):** `messages.rs` `Register` =
  `{ runner_id?: hint, capabilities: Vec<Capability>, priority, environment }`.
  - `Capability` is a serde internally-tagged enum (`tag = "kind"`):
    - `Analysis { name, version }`
    - `Rcode {}` (empty — no routing key yet; future: r_version constraints)
    - `Data { op: DataOp, formats?: Vec<String> }` — `formats` meaningful **only** for `data_open`
  - `DataOp` = `data_open` / `data_edit` / `data_close` / `data_update` (string-renamed unit enum).
  - `environment: Value` = hardware/runtime (`r_version`, `gpu`, `high_memory`) — **renamed
    from the old `capabilities` field** to free `capabilities` as the unified umbrella.
  - Removed: the old `modules: Vec<ModuleInfo>`, `services: Vec<Service>`, `ModuleInfo`, `Service`.
  - `RegisterAck` = `{ ok, runner_id?, reason? }` — the orchestrator mints `runner_id` (`r-N`).
- `cargo run -- --schema` prints the JSON Schema generated from the Rust types.

**Runner (R) — jaspBase-backed, real analyses render:**
- Two runners exist:
  - `runner_alpha.R` — the stepping stone: pure base R, emits a hello-world `htmlNode`. Proved
    the transport/registration/render pipe. Kept as a no-dependency fallback.
  - `runner_jaspbase.R` — **the real runner.** Loads a compiled module library, runs analyses
    through jaspBase, sends jaspBase's native serialization. **Verified: an independent-samples
    t-test from `jaspTTests` renders a real table in JASP.**
- `runner_jaspbase.R` startup (§1a of `jaspbase-plugin.md`, module-typed + warm at startup):
  `.libPaths(libdir)` → `library(jaspBase)` (loads the Rcpp `jaspResults` module) → define the
  **bridge natives in `globalenv()`** → `jaspBase::initEnvironment()` → `library(<module>)`
  (eager, before registration) → register advertising `analysis{<module>@<ver>}` truthfully.
- **The bridge** stands in for the gutted C++ engine. `.fromRCPP` (jaspBase `R/common.R:622`)
  resolves the natives via `exists()`/`getAnywhere()`, which **reaches `globalenv()`** (verified
  empirically). The jaspBase namespace is **LOCKED** — you cannot `assign()` into it. Natives:
  `.readDataSetRequestedNative` (full typed dataset, for `preloadData=TRUE`),
  `.readDatasetToEndNative`/`.readDataSetHeaderNative` (column-wise),
  `.requestTempRootNameNative`/`.requestTempFileNameNative`/`.requestStateFileNameNative`
  (per-work scratchpad), and the per-work *value* natives `.ppi`/`.imageBackground`/`.baseCitation`.
- **Per work:** set per-work context (dataset = the injected **Arrow/Feather** file read via
  `jaspRunner/R/data.R::read_jasp_data()` with a schema-derived spec `.spec_from_schema` — Arrow
  dictionaries→factors, float64→numeric, so the old `.typeDataset` character→factor guess is gone;
  scratchpad from `output_dir`, `.ppi`/etc. from `settings`) → **unwrap the frontend's options** (`.processOptions`: variable
  options arrive as `{value, types}` + a `.meta`; the old C++ `ColumnEncoder` unwrapped + encoded
  them — the runner unwraps; encoding is identity for simple names, real datasets need
  `encodeColNames`, tied to the data plane) → `jaspBase::runJaspResults(name, title, dataKey="{}",
  options, stateKey="{}", functionCall="<module>::<analysis>Internal", preloadData=<from work,
  default TRUE>)` → `getResults()` (live `.meta` form) → send `payload$results`.
- Event loop: `recv_aio` + `cv` + `until()` + `call_aio(ra)$data` — async, zero-CPU idle.
  **The accessor is `$data`, not `$raw`** (`$raw` is NULL in nanonext and caused the earlier
  `readBin "invalid connection"` crash). No `pipe_notify` yet (see §7).
- Contract + citations: `refactor_design/jaspbase-plugin.md`.

**Integration test (`refactor_design/integration_test.R`) — 16/16 PASS:**
- Register message uses `capabilities`/`environment`. **16/16 PASS** against the new register
  shape (T0 register incl. "register_ack carries assigned runner_id", T1 work forwarding,
  T2 result forwarding). Run with orchestrator up: `Rscript refactor_design/integration_test.R`.

**`JaspClient` (C++, `Desktop/jaspclient/`) + frontend wiring — code-complete, grep-clean:**
- `JaspClient` (`QObject` facade): `submit(work, handler) → work_id`, `abort(work_id)`.
  Correlation slot `work_id → {revision, handler}`; stale results (lower revision) dropped.
- `Analysis::run()` → `createWorkJson()` → `submit`; `workId() = "a" + instance id`.
- Confirmed uses `nng_pair1_open` and §18.1 framing — PAIR v1 matches the orchestrator.
- **Real JASP → orch → runner round trip: TESTED and WORKING — with real analyses.** A t-test
  submitted from real JASP renders a real jaspBase results table (not just the hello-world).
  `JASP_CLIENT_LOG=stub` confirms `JaspClient RX: result work_id=a0 status=complete`; status
  transitions empty→complete.

## 3. Key decisions (pointers into the spec)

- **Orchestrator = Rust** (§15, §5.2). Single static binary.
- **NNG PAIR v1 (polyamorous)**, §18.1 framing. nanonext `"poly"`, Rust `Protocol::Pair1`.
  **Do NOT mix v0 and v1 — wire-incompatible.**
- **Unified `capabilities` registration model** (DECIDED, **in code AND spec**): one `capabilities` list, each entry tagged by `kind` (`analysis`/`rcode`/`data`),
  mirroring the work `kind` enum one-to-one. A capability is the **routing-key projection** of
  a work kind (work = full request; capability = minimal key to match). `environment` (was
  `capabilities`) holds hardware. This replaces the old `modules` + `services` split.
  - **Why unified:** closes the rcode gap (rcode now advertises as a capability), treats
    "run a module" as a capability like any other, and is extensible (new kind → new variant).
- **`data` is ONE capability variant with an `op`** (mirrors work `kind:"data"`); `formats`
  only for `data_open`.
- **Two data lanes** (the data-plane routing design, in spec §5.4/§8.1): **R utility runner**
  opens reader-heavy formats (`csv`, `spss`, `jasp-legacy`, + `excel`/`stata`/`sas`); **Rust
  data-runner** (`arrow-rs`) opens Arrow-native formats (`arrow`/`parquet`) and owns **ALL**
  writes (`data_edit`/`data_close`/`data_update`). Opens route by **format**; edits route to
  Rust. Both lanes are **pinned, not rotated** (§6.1). Data-runner is a *routed role*, not a
  special component.
- **`filter` / `computed_column` → their OWN work kinds, NOT `rcode`** (DECIDED, future/not
  built). They're constrained, validated, routine expressions with structured data-plane
  output (a row mask / a new column via `data_update`), unlike arbitrary `rcode`. Route to
  R-capable runners (jaspBase). The unified model accommodates them cheaply (add `kind` +
  capability variants). Whether one `compute` kind or two (`filter`+`computed_column`) is an
  open detail; leaning two (distinct payloads/outputs) sharing an R-compute capability.
- **`work_id` = stable analysis instance id** (`"a17"`); `revision` frontend-owned (§19.1, §23).
- **Client correlation = id-keyed evicting slot** (§5.1, §23). **Analysis status display-only**.
- **Server-assigned `runner_id`** (DONE): orchestrator mints `r-N` in `register_ack`; runner's
  own id is only a hint.
- **jaspBase adapted-to as-is, not refactored (yet).** The runner plugs into jaspBase as it
  exists today (`jaspbase-plugin.md`); a later "jaspSlop" refactor happens *behind the same
  seam* (entry point + bridge are stable). No jaspBase edits in the PoC.
- **The runner replicates the ENGINE path — and only that path.** Three callers exist and must
  not be conflated: (1) **JASP desktop/engine** → `runJaspResults(<module>::<Analysis>Internal,
  preloadData, options)` directly — the runner replicates this **entry point and args**, but in
  **standalone mode** (`isInsideJASP()` left false): `runJaspResults` returns the `jaspResults`
  object and the runner extracts the JSON via `getResults()`, instead of the engine's send-func
  callback; (2) **syntax/RStudio** → wrapper → `runWrappedAnalysis` →
  `jaspSyntax::loadQmlAndParseOptions` → `runJaspResults` (needs `jaspSyntax`, **not in the
  libpath** — off-limits); (3) **jaspTools** → testing harness (**never used**, by explicit
  decision). Note `runWrappedAnalysis`'s `calledFromJasp` branch only *echoes options* (it's an
  options-parser for the desktop's R-syntax feature) and runs nothing — so the engine never used
  it to run analyses.
- **`preloadData` lives in `Description.qml`** (module-level property; default `true`; jaspAnova
  sets `false`). The generator copies it into the wrapper, but the *source of truth* is the
  description. The runner takes it from the **frontend** (`work.payload.preloadData %||% TRUE`) —
  forward-compatible, and `TRUE` is the safe default (preloading is a superset).
- **`functionCall` = `<module>::<Analysis>Internal`** — the `…Internal` function (exported), as
  the engine called it. The bare `<Analysis>` is the user-facing wrapper.
- **`dataKey`/`stateKey` are dead parameters** of `runJaspResults` (parsed via `fromJSON`, never
  referenced). Pass `"{}"`.
- **Bridge natives live in `globalenv()`**, not the jaspBase namespace (which is locked).
  `.fromRCPP`'s `exists()`/`getAnywhere()` reaches globalenv — verified empirically.
- **Frontend variable options are `{value, types}` + `.meta`**; the runner unwraps them
  (`.processOptions`) — the old C++ `ColumnEncoder`'s job. Column-name *encoding* is deferred to
  the data plane (identity for simple names).

## 4. NEXT STEPS — phased roadmap

**Done:**
1. ✅ Live integration test: 16/16 against the new register shape.
2. ✅ Spec synced to code: `neo-jasp.md` §9.3/§19.5 rewritten to the unified `capabilities` +
   `environment` model; prose in §5.4, §6.1, §9.4, §19.3, §17.2, §21.4, §25.6 updated.
3. ✅ Visual round trip: real JASP → orchestrator → R runner → result renders (hello-world
   `htmlNode`). Async cv loop works over real TCP (`$data` accessor).
4. ✅ **jaspBase hookup**: `runner_jaspbase.R` runs real analyses through `runJaspResults`
   (engine path) with a globalenv bridge; jaspBase serializes natively; a real t-test table
   renders in JASP. (`jaspbase-plugin.md`.)

**The remaining work is generalization, in four phases. Sequencing opinion: Phase 2 before
Phase 3 — the data plane adds a *second* runner (the Rust data-runner), and with >1 runner the
the orchestrator's current broadcast routing is wrong, so a real registry + pipe-targeted routing +
disconnect detection must exist first.**

> **✅ DONE — disconnection detection (`pipe_notify` under PAIR v1).** Re-validated under PAIR v1 in
> both directions: Rust↔Rust unit tests (register → evict on disconnect; work errors when no live
> runner) and a cross-language Rust↔R test (a real `Rscript`/nanonext runner dials the Rust
> orchestrator, registers, disconnects, and is evicted via `pipe_notify` `RemovePost`). The
> orchestrator keeps a `Pipe → {runner_id, capabilities}` registry, evicts on `RemovePost`, and
> returns a `fatalError` result instead of broadcasting work into the void when no live runner can
> serve it. **No spurious signals observed under v1** (the v0 quirk did not recur). What remains
> in Phase 2: pipe-*targeted* routing (currently broadcast — correct for one runner, breaks with
> >1) and liveness (opportunistic activity reporting + `busy_hang_timeout` on silence — **no fixed
> heartbeat**; a TCP beat is a deferred remote/TCP extension, not a cornerstone). See §25.4.

**Phase 1 — Tidy + generalize the runner** *(quick, low-risk)*
- ✅ Scratchpad / cwd / absolute paths: the orchestrator injects an **absolute** dataset path
  (env `JASP_ORCH_TEST_CSV`, default `test_data/debug.csv` resolved against its cwd) and a
  **per-work** `output_dir` (`/tmp/jasp-runner-output/<work_id>`). The runner reads paths directly
  — `.runnerRoot` / `resolvePath` / the hardcoded-CSV fallback are deleted — and does `setwd` into
  the work scratchpad each work, reclaiming cwd from `initEnvironment()`'s startup tempdir.
  Verified live (the integration test prints the absolute path + per-work dir).
- ✅ Logging gate: the verbose `optionsJson` dump is behind `JASP_RUNNER_VERBOSE` (mirrors
  `JASP_CLIENT_LOG`), off by default.
- ⏳ Config-driven module set: load + advertise *N* modules from config, not a hardcoded `jaspTTests`.
- Write-seal: **deferred** — only needed for the on-disk `.jasp` save path; the streaming runner
  doesn't persist `jaspResults.json`, so `Created Write Seal at: ''` is benign noise (§13/§14).
- (Column-name **encoding** deferred to Phase 3 — tied to the data bridge.)

**Phase 2 — Registration & disconnection (orchestrator)** *(foundational — core DONE)*
- ✅ Re-validate `pipe_notify` under PAIR v1 — done, no spurious signals (Rust↔Rust + Rust↔R tests).
- ✅ Store a real registry on register: `Pipe → {runner_id, capabilities}` (keyed by `Pipe`, which is
  `Eq + Hash`; populated when the `Register` message arrives, captured via `msg.pipe()`).
- ✅ Disconnection detection: `pipe_notify` `RemovePost` evicts the runner from the registry;
  no-silent-loss routing returns a `fatalError` result when no live runner can serve a `work`.
- ⏳ Pipe-targeted routing: route a `work` to a runner advertising the matching capability via
  `set_pipe`, not broadcast (broadcast is correct for one runner; breaks with >1). Deferred to
  multi-runner (the Phase 3 data plane adds a second runner).
- ⏳ Liveness: opportunistic activity reporting — the runner reports `last_activity` on each
  queue-processing tick (via the `activity` message, §19.4, when it has nothing else to send,
  rate-limited by `activity_min_interval_ms`); the orchestrator applies `busy_hang_timeout` on
  *silence* (no `result` *or* `activity`). **No fixed heartbeat** — a side-thread beat proves only
  socket liveness, not actual work; a TCP beat is a deferred remote/TCP extension (§25.4).

**Phase 3 — Data plane (§8)** *(the big user-visible one)*
- ✅ **Thin slice done:** the runner reads a real **Arrow/Feather** dataset (`test_data/debug.arrow`,
  generated from `debug.csv` by `make_debug_arrow.R`) through `read_jasp_data()` with a
  schema-derived spec — **validated by a live t-test round trip** (jaspBase computed a real table
  from the Arrow-typed frame; `.typeDataset` deleted). The orchestrator still injects a hardcoded
  absolute path (env `JASP_ORCH_TEST_DATASET`); the real data plane below is what remains.
- `dataset_open` routed to a lane: R utility runner for reader-heavy formats (csv/spss/jasp-legacy/
  excel/stata/sas); Rust `arrow-rs` data-runner for arrow/parquet **and all writes**
  (`data_edit`/`data_close`/`data_update`).
- Orchestrator owns the Arrow cache (`dataset_id → Feather path`, LZ4); runner mmaps Feather
  instead of reading hardcoded CSV. Column-name encoding (`encodeColNames`) lands here. The runner
  then gets the schema from `dataset_ready` (§19.2) instead of reading it off the file (alpha
  shortcut).

**Phase 4 — Control plane**
- `hello`/`welcome` + `session_id`, module discovery (`list_modules`/`modules`), `error` handling,
  reconnect.

## 5. Environment and how to run

- **Orchestrator:** `cargo run --manifest-path orchestrator/Cargo.toml`
  (frontend `tcp://127.0.0.1:9555`, runner `tcp://127.0.0.1:9556`). `JASP_ORCH_URL` /
  `JASP_RUNNER_URL` override. `cargo run -- --schema` prints the JSON Schema.
  `cargo test --manifest-path orchestrator/Cargo.toml` runs the 3 tests.
- **Runner (real, jaspBase):** `Rscript refactor_design/runner_jaspbase.R [orch_url]`.
  Env: `JASP_RUNNER_LIBDIR` (default `/home/sp42/jaspModuleTools/workdir/jaspTTests`),
  `JASP_RUNNER_MODULE` (default `jaspTTests`), `JASP_RUNNER_MODULE_VER` (default `0.95.5`).
- **Runner (hello-world fallback):** `Rscript refactor_design/runner_alpha.R [orch_url]`
  (pure base R; no module library needed).
- **Integration test:** `Rscript refactor_design/integration_test.R` (orchestrator must be up).
- **Launch script:** `sh refactor_design/launch_alpha.sh` (starts orchestrator + runner_alpha —
  point it at runner_jaspbase.R for real analyses).
- **JASP:** built locally. Run with `JASP_CLIENT_LOG=stub JASP_ORCH_URL=tcp://127.0.0.1:9555`.
- **To see a real t-test:** open `test_data/debug.csv` in JASP (cols `group`,`x`,`y`,`z`), add an
  Independent Samples T-Test, `x`→Dependent, `group`→Grouping. The orchestrator's
  `inject_dataset_paths` still hardcodes `ds-001 → test_data/debug.csv`, so the UI variables must
  match that file's columns (the data plane, Phase 3, removes this coupling).
- **R here:** 4.6.1; `nanonext`/`arrow`/`jsonlite` installed. `jaspBase`/modules NOT installed
  globally; the **compiled libpath** `/home/sp42/jaspModuleTools/workdir/jaspTTests` has
  `jaspBase` **0.20.4** (compiled `libs/jaspBase.so` = the Rcpp `jaspResults` module),
  `jaspTTests` **0.95.5**, `jaspGraphs`, and all heavy deps (Rcpp **1.1.2**, BayesFactor, car,
  ggplot2, plotly, …). Built by jaspModuleTools/renv. **`jaspSyntax` is NOT in the libpath**
  (which is why the `runWrappedAnalysis`/syntax path is off-limits to the runner).
- Module **source** checkouts: `/home/sp42/modules-registry/Official/<module>` (e.g. jaspTTests,
  jaspAnova) — useful for reading analysis code / QML / `Description.qml`.
- **cargo:** available at `~/.cargo/bin/cargo` in this container. JASP builds on the user's
  machine (Qt/conan).

## 6. File map

```
orchestrator/
├── Cargo.toml                    # deps: nng, serde(derive), serde_json, schemars
├── hello_result.json             # canned jaspResults fixture (kept for reference)
└── src/
    ├── main.rs                   # BROKER: two PAIR v1 sockets, two blocking threads, register intercept
    └── messages.rs               # wire types: Register{capabilities,environment}, Capability{Analysis,Rcode,Data}, DataOp, RegisterAck
Desktop/
├── jaspclient/{jaspclient.h,cpp} # NNG client (PAIR v1): submit/abort, evicting correlation slot, recv thread
└── analysis/{analysis.h,cpp}     # run()->submit, workId()="a"+id, display-only Status
refactor_design/
├── neo-jasp.md                   # NORMATIVE SPEC — §9.3/§19.5 synced to unified capabilities model
├── HANDOVER-next.md              # THIS FILE
├── jaspbase-plugin.md            # runner ↔ jaspBase bridge contract (implemented by runner_jaspbase.R)
├── runner_jaspbase.R             # REAL runner: jaspBase runJaspResults + globalenv bridge; real tables render
├── runner_alpha.R                # stepping-stone runner: pure base R, hello-world htmlNode (fallback)
├── integration_test.R            # T0 register, T1 work, T2 result — 16/16 PASS
├── make_debug_arrow.R            # regenerates test_data/debug.arrow from debug.csv (§8.3 schema)
├── launch_alpha.sh               # convenience: starts orchestrator + runner
├── GUT_TODO.md                   # gut status + deferred items
├── poc-handoff.md                # original PoC plan
└── poc/*.R                       # validated NNG/Arrow PoC scripts
jaspRunner/
└── R/data.R                      # unified Arrow/Feather read path (read_jasp_data); §8.3, benchmarked + smoke-tested
test_data/
├── debug.csv                     # source-of-truth values (6 rows, 1 cat + 3 num columns)
└── debug.arrow                   # Feather+LZ4 fixture the orch injects (regen: make_debug_arrow.R)
```

## 7. Gotchas

- **SPEC = CODE on registration** (synced this session). `neo-jasp.md` §9.3/§19.5 now match the
  unified `capabilities` + `environment` model in the code.
- **Integration test: 16/16 PASS** against the new register shape. Rust unit tests (5 pass, 1
  ignored) + R integration test + cross-language Rust↔R disconnect test all green.
- **PAIR v0 ≠ v1** — wire-incompatible. We use v1 everywhere (`"poly"` / `Protocol::Pair1`).
  C++ JaspClient confirmed on `nng_pair1_open`.
- **Async cv accessor is `$data`, not `$raw`.** nanonext's `call_aio()` exposes `value`/`data`/`aio`;
  `$raw` is NULL and caused `readBin "invalid connection"`. The runner and any future async R code
  must use `call_aio(ra)$data`.
- **NNG is NOT fork-safe.** `parallel::mcparallel` (fork) + NNG socket creation in the child
  crashes with "illegal operation." Use separate processes, not forked ones, for multi-peer tests.
- **Disconnect detection — DONE (Phase 2 core).** `pipe_notify` re-validated under PAIR **v1**
  (Rust↔Rust unit tests + a cross-language Rust↔R test: `refactor_design/probe_register_disconnect.R`,
  run via `cargo test -- --ignored r_runner_disconnect`). The orchestrator keeps a `Pipe →
  {runner_id, capabilities}` registry, evicts on `RemovePost`, and returns a `fatalError` result
  when no live runner can serve a `work` (no silent loss). The nanonext `cv`/`pipe_notify` spurious
  signal quirk under **v0** did **not** recur under v1.
- **Routing is still broadcast, not pipe-targeted.** The registry exists and eviction works, but
  `work` is still broadcast to all runners (correct for one runner; breaks with >1). Pipe-targeted
  routing via `set_pipe` is the remaining Phase 2 item, needed once the data plane adds a second
  runner (§9.3).
- Both peers must be connected before work flows; NNG PAIR doesn't buffer for absent peers.
- **Terminal tool quirk:** backgrounded-shell one-liners with `&` + `$?` get rejected as
  "forbidden substitutions." Run the orchestrator and the test in **separate terminals**, or
  avoid `$?`/`$VAR` in the command.
- `performType` enum still declared (unused) in `Common/enginedefinitions.h` — optional cleanup.
- **Frontend options are `{value, types}` + `.meta`, not plain values.** boundValues() sends
  variable options as `{value, types}` objects with a top-level `.meta` (shouldEncode flags).
  The old C++ `ColumnEncoder` unwrapped + encoded them before R saw them. The runner must
  **unwrap** (`.processOptions`) or the analysis gets a length-2 list and errors (`options$group
  != ""` → "'length = 2' in coercion to 'logical(1)'"). Encoding is identity for simple names.
- **`initEnvironment()` does `setwd(.requestTempRootNameNative()$root)`** — at startup that's
  `tempdir()`. **Resolved (Phase 1):** the orchestrator injects **absolute** dataset paths and the
  runner does `setwd(.state$outputDir)` per work, so cwd is always the current work's scratchpad and
  the old `.runnerRoot` cwd-snapshot workaround is gone. `initEnvironment()`'s startup `setwd` is now
  harmless (overwritten on the first work).
- **jaspBase namespace is LOCKED** — bridge natives must go in `globalenv()`, not
  `asNamespace("jaspBase")` (`assign()` there errors "cannot add bindings to a locked
  environment"). `.fromRCPP`'s `exists()`/`getAnywhere()` reaches globalenv fine.
- **`runJaspResults` `dataKey`/`stateKey` are dead** — parsed via `fromJSON`, never referenced.
  Pass `"{}"`.
- **`preloadData` from the frontend, default `TRUE`.** Lives in `Description.qml` (module-level;
  jaspAnova sets `false`). `TRUE` is the safe default (preloading is a superset — the analysis
  either uses the preloaded frame or reads on demand via the bridge).
- **`getResults()` is reached via R6 private**:
  `jr$.__enclos_env__$private$jaspObject$getResults()`. Fragile; a cleaner accessor is a Phase 1
  tidy candidate.
- **Benign standalone-mode messages:** "Did not store jaspResults", "Created Write Seal for
  jaspResults at: ''" (no write-seal location set by the runner), and "registerFonts ...
  resultFont does not exist" at startup — none affect `getResults()` / rendering.
- **`runner_jaspbase.R` is alpha-shaped:** single module via env, identity column-encoding.
  (Dataset path is now absolute-from-orchestrator and `optionsJson` logging is gated — both done in
  Phase 1.) Config-driven multi-module is the remaining Phase 1 item.
