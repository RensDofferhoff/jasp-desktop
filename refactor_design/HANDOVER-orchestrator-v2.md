# HANDOVER — orchestrator v2: scheduler LANDED → the views phase is next

**Date:** 2026-09-02 (evening) · **Status:** §12 steps 1–4 + runner-side abort are
**implemented, tested, e2e-verified**; views phase (steps 5–8) not started ·
**Constitution:** `refactor_design/orchestrator-v2-design.md` (its status block is
updated to say exactly this)

## Where this lives in git

Landed as ONE atomic commit on `development` ("orchestrator v2 phase 1: the
scheduler + workspace + runner abort plane" — `git log --oneline -3` finds it):
the split, the v2 crate, and the runner plane cross-reference, and every
intermediate state was only ever validated as a whole. Deliberately NOT in that
commit (pre-existing, still uncommitted — decide their fate separately):
`AGENTS.md` local mods, `analysis-views-design.md` local mods, the other untracked
`HANDOVER-*.md` files, and the numbered junk dirs at repo root (crash dumps).
`orchestrator/messages.schema.json` is regenerated and committed (now contains
`slots` + `aborted`).

## Read first, in this order

1. `orchestrator-v2-design.md` — THE design (§0 TL;DR → all of it). The
   rejected-ideas tables (§15 + §8's view graveyard) and `analysis-views-design.md`
   §14 are **law** — doubted into the ground; don't relitigate without new evidence.
2. This file's "What landed" + "Decisions & deviations" sections — the implementation
   made calls the design doc doesn't record.
3. `analysis-views-design.md` — the views concept, **amended** (status block):
   AV6/7/9/11 superseded by v2 §8; AV1–AV5, AV8(+`FORMAT_VERSION`), AV10, AV12 stand.
4. `orchestrator-design.md` — classic, the as-built spec (pre-extraction paths).

## What landed (map)

`orchestrator/` is now the §11 Cargo workspace — `crates/{wire, classic, v2,
data_runner}`. Binaries still land in `orchestrator/target/{debug,release}/`
(`jasp-orchestrator`, `jasp-data-runner`, `jasp-orchestrator-v2`), so every script
path keeps working. Virtual workspace root ⇒ `cargo run` from `orchestrator/` needs
`--bin <name>` (only `launch_alpha.sh` needed the one-line fix).

| Area | File(s) | Notes |
|---|---|---|
| Wire crate | `crates/wire/src/{lib,messages,framing,transport,provisioner,janitor}.rs` | messages moved verbatim; framing deduplicated from 3 copies; transport + provisioner + janitor live here too (see deviations #1) |
| Classic (FROZEN, bugfix-only) | `crates/classic/src/main.rs` (+`tests/dataset_e2e.rs`, `examples/`) | compiled against `wire`; `use wire as messages;` keeps internals readable |
| Data worker | `crates/data_runner/src/{main,csv2arrow,arrowview,dataedit}.rs` | own crate; advertises `slots: 1`; NO view-build capability yet (deliberate) |
| v2 scheduler | `crates/v2/src/{main,router}.rs` | books/queues/machinery + all 12 tests in `router.rs`'s tests mod |
| Runner abort plane | `refactor_design/runner_jaspbase.R` | §7 deltas: arm/drain, `jasp_checkpoint()`, `jaspAbort`, loop-top no-op, `status="aborted"` |
| R tests | `refactor_design/{test_abort_plane,test_v2_supersede_e2e}.R` | 6/6 unit; e2e over the real jaspBase runner |
| Docs | v2 design (status + §13 addendum), `ALPHA_RUN.md` | §13 notes the pre/post-extraction path mapping |

**Test state (all green, clippy clean):** classic 41 (+1 ignored R cross-lang) ·
classic e2e 7 · data_runner 88 (+1 ignored) · **v2 12** · wire 4.

v2's must-pass list, by test name: `never_dispatch_to_a_busy_executor`,
`slots_window_dispatches_two_then_waits`, `churn_sends_exactly_one_abort`,
`abort_is_credit_neutral`, `raced_completion_is_a_no_op_router_side`,
`queued_supersession_replaces_in_place`, `wedge_is_recycled_and_work_fail_fasts`,
`op_aware_eviction_open_dies_view_survives`,
`lane_death_fails_the_view_but_keeps_the_dataset`,
`stale_edit_is_rejected_at_dispatch`, + 2 parity tests.

**The e2e that proves the whole §6 story** (`test_v2_supersede_e2e.R`, passing):
open → dispatch rev1 → supersede rev2 → ONE abort push → runner drains at run
boundary → `status="aborted"` terminal → router discards it by revision → rev2
dispatches on the freed credit → frontend sees ONLY `rev=2 complete`.

## Decisions & deviations the design doc doesn't record

1. **`wire` is wider than "messages + framing"** — it also carries `transport`
   (NNG plumbing), `provisioner`, `janitor`. Rationale: §11 rule 1 "extract, don't
   fork" needs a shared home for the NNG traps; classic and v2 both consume them;
   the doc's 4-crate diagram is preserved. `wire` therefore depends on `nng`.
2. **Transport ordering fix (real bug found by v2's tests):** the Aio callback now
   forwards to the mailbox BEFORE re-arming (unbounded mpsc — never blocks);
   re-arm-first let completions on different NNG pool threads invert wire order.
   Classic had the same latent race; its suite stayed green after the fix.
3. **Wire additions landed (all additive, `v`=1):** `Register.slots` (default 1,
   classic ignores), `Status::Aborted` (terminal; classic treats it terminal too —
   its ONE bugfix), `ProvReq::Recycle {modules,lanes}` (wedge-kill; classic never
   sends), `Status` gained `PartialEq/Eq/Copy`, `WorkKind`+`WorkPayload::kind()`
   moved into wire (orphan rule forced it).
4. **v2 book shapes** (in `router.rs`): `works` = *desired state* per work id
   (`frontend, revision, kind, runner: Option, abort_sent`) — the RUNNING dispatch
   lives in `Executor.inflight: (session,work_id) → DispatchRecord{revision, kind,
   dataset_paths, data}`. The record (not `works`) drives credit return, ref release,
   and op-aware side effects — so stale terminals complete their dataset lifecycle
   without forwarding, and duplicate terminals no-op. This replaces classic's
   `data_works` book entirely.
5. **Supersede replies:** rev < desired → synthetic **Complete** + `message:
   "superseded…"` (the client drops it by revision, §23 — a terminal shape is safe);
   rev == desired → `running` re-ack (duplicate submit; no re-dispatch under pull).
6. **`rescue_orphaned_ready_work`:** on eviction, ready work whose capability lost
   its last provider re-routes through the miss path (park/fail) — no silent
   stranded queue.
7. **v2 `Config` is a deliberate ~150-line duplicate of classic's** (policy, not
   protocol). Its path methods MUST keep producing identical paths (same runner /
   frontend on-disk contract) — noted in `v2/src/main.rs`.
8. **`managed` heuristic:** `Executor.managed = provisioner.is_some()` — there is no
   runner_id↔pid mapping; `Recycle` kills by advertised modules/lanes, so attached
   runners simply match no child.

## Known debts (small, documented — fix opportunistically)

- **Wedge-recycle hole:** provisioner configured + an *attached* wedged runner ⇒
  `Recycle` kills nothing, the pipe never closes, book-eviction never fires ⇒
  repeated Recycle ticks, work stuck. Fix idea: `recycle_requested_ms` on
  `Executor`; still hung one window later ⇒ direct `evict_executor`.
- **Data-lane false wedge:** the data worker sends no mid-job activity, so a legit
  >30 s open looks hung (§14 Q5 parked this). Views builds are cheap projections,
  but opens precede them — consider worker-side activity or an exemption before
  shipping v2 against big CSVs.
- `hung_snapshot`/`ready_snapshot`/`is_ever_registered` test helpers unused
  (`#[allow(dead_code)]`), v2's `superseded`/`running` synthetics share
  `empty_payload`.
- Nothing calls `jasp_checkpoint()` yet (see "Runner-side abort, remaining half").

## The views phase (§8 + §12 steps 5–8) — how to slice it

**Naming hazard first (`analysis-views` §11):** `DataOp::View`/"data_view" = the
GRID's chunked TSV reads (exists, done). "Analysis views" = the new stapled-spec
typed-Arrow projections. Same word, different artifacts — never conflate them in
code or conversation.

**Wire (all additive; `v` stays 1; add to `crates/wire/src/messages.rs`):**
- `Work` gains `views: [spec…]` stapled per dataset input — spec shape = v2 §3 /
  AV §4: `{dataset_id, columns: [{name, as: scale|nominal|ordinal}], filter, all}`
  (multiplicity structural; types structured, never string conventions).
- `cache_filled {view_id}` — data worker → router, **internal, never forwarded**.
- `prefetch {specs}` — frontend → router → worker hint; ignorable, no credit. LAST.
- `view_id = hash(FORMAT_VERSION, spec, base_revision)` — computed by the ROUTER at
  staple time (µs of CPU — honors P1). `FORMAT_VERSION` constant folded into the
  hash input so a writer/codec change can never false-hit an old blob (AV8 amended).

**v2 router (`crates/v2/src/router.rs`):** add the view book (§4:
`hash → {state: building|ready|evictable, waiters, spec, touch, bytes}`); staple
check at admission (books-only lookup — the router never stats the FS): hit ⇒
ready-queue, miss ⇒ park with a new `Awaiting::ViewBuild` + order the fill to a
free capable worker (no credit — it's not frontend work; waiters were never
dispatched so worker death just re-orders); `RouterMsg::CacheFilled` un-parks;
**the one hold rule** in the tick's LRU sweep — *a view referenced by a parked or
dispatched work is never reclaimed; everything else is LRU food; no pins, no
releases*; reclamation executes on the janitor. Pass-through = hash-hit-on-base
(`columns: all` + unfiltered resolves to a reference to the base file — zero copy).

**data worker (`crates/data_runner`):** the build function, seeded from the
`csv2arrow` typing machinery (AV4: worker owns all casts/relabeling —
dictionary→scale = values parsed once; nominal/ordinal = label-or-value overlay +
ordered flag; text-as-scale → null), emitting the AV §5 artifact (Feather V2+LZ4,
`<real name>__<type>` fields, base-row index column, real↔token map + base revision
+ spec digest in schema metadata). Advertise it as an added capability
(spelling open — §11.3; classic ignores unknown capabilities either way).

**Gates before anything user-visible flips:**
1. **Determinism differential (AV8, the gate):** same `(spec, base_revision)` ⇒
   byte-identical rebuild.
2. **Coercion parity:** same spec → old R read (`read_jasp_data`, §8.3) vs worker
   view ⇒ identical frames — the harness is the arbiter, then the R-side
   read-coercion engine retires.
3. **Filters:** a view filter must BE data (derived boolean column) — that design
   isn't landed; until it is, `filter:` application is blocked. Check its status
   first; don't invent an R-expression evaluator in Rust (rejected, AV §14).

**Then §12 step 7 (differential + e2e):** run BOTH orchestrators over shared
scenarios (`frontend_*.R` pattern; the e2e scripts from this session are the seed),
flip the desktop default to v2, freeze classic for real. **Step 8 (prefetch):**
only if dispatch-time fills show in the numbers.

## Runner-side abort — remaining half (anytime; serves both orchestrators)

The runner-side machinery is DONE and tested: abort plane (arm/drain), the
`jasp_checkpoint()` hook, `jaspAbort` cooperative unwind, run-boundary drain,
loop-top raced-completion no-op, `status="aborted"` terminals (v2 e2e exercises the
boundary path). What's missing: **jaspBase's check code must call
`jasp_checkpoint()`** so long analyses unwind mid-run instead of at the boundary.
jaspBase lives OUTSIDE this repo (`JASP_RUNNER_LIBDIR` =
`/home/sp42/jaspModuleTools/workdir/jaspTTests`). Two options: patch jaspBase
properly, or wrap it from the runner at boot — `install_state_timers()` in
`runner_jaspbase.R` is the working `assignInNamespace` wrap pattern to copy. Find
the check function via `refactor_design/jaspbase-plugin.md` (the bridge contract).

## Validate everything (from repo root)

```sh
cd orchestrator && cargo test          # 152 green across 6 targets (needs UNSANDBOXED — see gotchas)
cargo clippy --all-targets             # clean — keep it that way
cargo run --quiet --bin jasp-orchestrator -- --schema | diff - messages.schema.json
Rscript -e 'parse(file="refactor_design/runner_jaspbase.R")'   # after R edits
Rscript refactor_design/test_abort_plane.R                     # 6/6, seconds
Rscript refactor_design/test_v2_supersede_e2e.R                # full §6 story; runner boot is 60–180 s
```

### Environment gotchas (cost me time — don't repeat)

- **The Zed sandbox blocks AF_UNIX filesystem socket binds** (plain Python
  `bind()` gets EPERM). `routing_over_ipc` and the e2e (ipc endpoints) FAIL
  in-sandbox and PASS unsandboxed — run cargo tests unsandboxed, or
  `-- --skip routing_over_ipc`.
- `JASP_RUNNER_LIBDIR` is `/home/sp42/jaspModuleTools/workdir/jaspTTests` — the
  MODULE dir (self-contained libpath), NOT the workdir root (that dir has no
  top-level jaspBase).
- Runner boot is slow (jaspBase + renv): the e2e polls the runner log for
  "waiting for work" with a 180 s deadline. Logs: `/tmp/v2-e2e-{orch,runner}.log`;
  workspaces `/tmp/jasp-v2-e2e-*` (cleaned on exit).
- C++ desktop build/test rules: root `AGENTS.md`; `build/` is pre-configured.

## Rules of engagement (unchanged, still law)

- **P1** single-writer router (plain maps, no locks, never blocks, never FS/process
  I/O). **P2** dumb executors (register slots, signal, run, report — no queues, no
  revision logic, no opinions). **P3** correctness never depends on hints —
  determinism + content addressing backstop everything.
- Wire additive, `v` stays 1. Classic frozen (bugfix-only); its suite is the spec
  of record for everything unchanged.
- *One writer, many caches; a cache is always safe to drop.* A second party holding
  decision-influencing state is a bug — unless bounded by a credit window and
  reconciled by revisions.
- Every step leaves the tree green and classic runnable.

*The scheduler holds; the caches are next. Build the views smaller every time you
doubt them.* 📐
