# HANDOVER — orchestrator v2: views LANDED (router+worker) → the runner read seam is next

**Date:** 2026-09-02 (late evening, session two) · **Status:** §12 steps 1–5 + runner-side
abort are **implemented, tested, e2e-verified**; the runner-side view READ seam,
coercion parity (gate 2), the differential flip (step 7), and prefetch (step 8) are
not started · **Constitution:** `refactor_design/orchestrator-v2-design.md` (its status
block + §13 addendum say exactly this)

## Where this lives in git

Steps 1–4 + abort landed as the atomic commit `4f7c61dea` on `development`. **This
session's views work is UNCOMMITTED** — review/commit as one slice (the wire additions,
the worker builder, the router book, the tests, the schema regen, the doc updates).
Still deliberately uncommitted from before (decide their fate separately): `AGENTS.md`
local mods, `analysis-views-design.md` local mods, other untracked `HANDOVER-*.md`,
the numbered junk dirs at repo root. `orchestrator/messages.schema.json` is
regenerated (now carries `views`, `cache_fill`, `cache_filled`, `view_build`, the
`ViewSpec` family).

## Read first, in this order

1. `orchestrator-v2-design.md` §8 (views) + §3 (the views wire rows) — THE design.
   §13's post-views addendum maps every file. The rejected tables (§15, §8, AV §14)
   are **law**.
2. This file's "What landed" + "Decisions & deviations" — the implementation made
   calls the design doc doesn't record.
3. `analysis-views-design.md` — the concept, as amended (AV6/7/9/11 superseded by §8;
   the §5 artifact and §7 R-side duties are the NEXT session's brief).

## What landed this session (the views phase, step ⑤)

**Naming hazard (AV §11), still law:** `DataOp::View`/"data_view" = the GRID's chunked
TSV. Analysis views = the stapled-spec typed-Arrow projections (`view_build`, `cache_fill`).

| Area | File(s) | Notes |
|---|---|---|
| Wire | `crates/wire/src/messages.rs` | `ViewSpec{dataset_id, columns:[{name, as}], filter, all}` (structured types, multiplicity structural); `Work.views: Option<Vec<ViewSpec>>`; `Message::CacheFill{view_id, spec, source, target, base_revision}` (router→worker order) + `CacheFilled{view_id, bytes?, error?}` (worker→router confirm, never forwarded); `DataOp::ViewBuild` = the capability key ("view_build"); `VIEW_FORMAT_VERSION="av1"` folded into the hash; `view_id()` = SHA-256 — all additive, `v` stays 1 |
| Worker builder | `crates/data_runner/src/analysisview.rs` (~500 lines + tests) | the §8.3 matrix (see below); AV5 artifact: Feather V2+LZ4 (the caches' writer), `<real>__<type>` fields, `__base_row` (int32, 0-based, last field), `jasp:view` schema metadata {format_version, view_id, base_revision, dataset_id, rows, base_row_column, token_map (name→hex), spec}; tmp+rename atomic publish; one in-memory batch (the spec IS the prune) |
| Worker wiring | `crates/data_runner/src/main.rs` | advertises `Data{op: view_build}`; serves `cache_fill` (not a work!) → builds → replies `cache_filled{bytes}` or `{error}` |
| Router book | `crates/v2/src/router.rs` | `views: HashMap<id, ViewEntry>`; states `Wanted / Ordered(exec) / Ready{bytes} / Failed(reason)`; `staple()` at admission AND at dispatch (the guard); `Awaiting::ViewBuild` parks (id-free); `order_fills()` (free builders only, no credit, source `path_refs` held); `handle_cache_filled`; `try_unpark_view_waiters`; `scan_views()` LRU in the tick (budget `JASP_ORCH_VIEW_BUDGET_MB`, default 2 GiB) |
| Dispatch stamp | `router.rs::dispatch` | injects `view_refs: [{dataset_id, view_id, path}]` per spec as an envelope field (the `dataset_paths` precedent — NOT typed on `Work`); pass-through refs are `view_id: null` + the base path; `dataset_paths` still rides (migration bridge, AV §6) |
| Config | `crates/v2/src/main.rs` | `view_cache_path()` = `<session>/views/<hash>.arrow` (dies with the session workspace); `view_budget_bytes` |
| Tests | wire +1 (`view_specs_and_ids_round_trip`); data_runner +5 (determinism gate, coercion matrix, metadata, refusals, r_character); v2 +7 (`view_miss_parks_until_cache_filled`, `view_hit_dispatches_without_a_build`, `passthrough_hits_the_base_file`, `filter_specs_are_refused`, `build_failure_fails_waiters_and_poisons`, `hold_rule_lru_reclaims_only_unheld_views`, `worker_death_reorders_the_fill`); v2 e2e +1 (`stapled_view_fills_builds_and_dispatches_e2e`) |

**Test state (all green, clippy clean):** classic 41 (+1i) · classic e2e 7 ·
data_runner 93 (+1i) · **v2 19** · **v2 views e2e 1** · wire 5 = **166**.

**The e2e that proves §8 end to end** (`crates/v2/tests/views_e2e.rs`, real binaries):
open real `test_data/debug.csv` → stapled work (group@nominal, x@scale, z@nominal) →
parks → router orders the fill to the provisioner-spawned worker → the real builder
writes the blob → `cache_filled` un-parks → dispatch carries `view_refs` (paths exist
on disk) → the test opens the blob with Arrow and checks: `group__nominal` dict A/B
keys passthrough, `x__scale` verbatim, `z__nominal` levels numerically sorted
("98"…"203"), `__base_row` dense, metadata view_id == file name, hex token map.

## The §8.3 coercion matrix as implemented (the parity target, gate 2)

| Stored | Requested | Built |
|---|---|---|
| dictionary | scale | k values parsed ONCE, rows index by key; unparseable → null (text-as-scale) |
| dictionary | nominal | label-or-value overlay (`jasp:labels`, sparse), dictionary ORDER preserved, ordered=0 |
| dictionary | ordinal | same, ordered=1 (dict order = the ranking) |
| float64 | scale | verbatim copy, NaN kept (`as.numeric` keeps it) |
| float64 | nominal/ordinal | distinct numerically-sorted levels; NaN → NA; level strings = `r_character()` |

`r_character` (isolated in ONE function in `analysisview.rs`) reproduces R's
`as.character` hybrid: 15 significant digits, trailing zeros trimmed, scientific
forced outside [1e-4, 1e15), otherwise shorter-of-fixed/scientific (why `1e14` →
`"1e+14"` but `123456789012345` stays fixed), ties → fixed, exponents `1e+20`/`1e-07`.
**The coercion-parity harness (gate 2, still to build) is the arbiter** — if R
disagrees anywhere, fix `r_character`/the matrix in that one place.

## Decisions & deviations the design doc doesn't record (this session's calls)

1. **The fill order is a new internal message, `cache_fill`** — not a work unit (the
   §8 rejected-table kills that: no result-forwarding, no correlation, no credit) and
   not `prefetch` (that carries no source/target and stays LAST/unbuilt). It mirrors
   `cache_filled`; the design's prose ("order the fill") names the action, this is its
   wire shape. Router mints identity (view_id + target path — the `data_open` split);
   worker reads `source`, writes `target`.
2. **Capability spelling: `DataOp::ViewBuild` = "view_build"**, advertised as
   `Data{op: view_build, formats: none}`. NOT "data_view" (the grid — naming hazard);
   classic tolerates the value (shared enum; classic's routing never matches it).
3. **`Failed` is a terminal poison state in the book.** A deterministic build failure
   (unknown column, filter, bad stored type) fails every waiter with the builder's own
   reason and refuses future staples of the same id forever — retrying an unbuildable
   spec can never succeed, so there is no retry loop. (A TRANSIENT failure — worker
   died — is `Ordered` → `Wanted`, which DOES retry.)
4. **`Awaiting::ViewBuild` carries no view ids.** The missing set is re-derived from
   the work's CURRENT stapled spec at every check — so the parked-supersession path
   (new revision, different spec) can never wait on stale ids, and
   `try_unpark_view_waiters` (run on every `cache_filled` and after in-place
   supersession) picks up works whose NEW spec was already cached.
5. **`view_refs` is an injected envelope field, not a typed `Work` field** — the exact
   precedent of `dataset_paths` (router-added identity at dispatch). The stapled
   `views` specs ride untouched; runners that don't speak views ignore `view_refs`.
6. **Fills prefer FREE builders only** (`select_view_builder`: `outstanding < slots`).
   If none is free the fill waits for the next terminal/register — a busy worker's
   socket holds at most aborts and ignorable hints (the §5 discipline extended to
   fills). Re-ordering triggers: staple miss, register, every terminal, eviction.
7. **Fills hold a `path_refs` reference on their source** (acquired at order, released
   at confirm/reset/death) — an edit reclaiming the base file mid-build can't yank it.
8. **The hold rule reads only live dispatch records** (`DispatchRecord.view_ids`).
   Parked waiters await non-Ready views by construction (Ready views aren't awaited);
   a queued-but-undispatched work whose view gets LRU-evicted is caught by the
   dispatch-time staple guard, which re-parks + re-orders (a cache is always safe to
   drop — P3 backstop).
9. **The e2e spawn gotcha:** `cargo test -p v2 --test views_e2e` does NOT rebuild
   `jasp-data-runner` (the provisioner spawns the sibling binary). Run
   `cargo build -p data_runner` first or a stale worker silently lacks `view_build`.
   (Recorded in design §13 addendum + the test header.)
10. **`close_work` now also drops parked instances of the closed work** (rev-matched
    when a revision is given) — a pre-existing gap that view-parking would have made
    loud. One line, clearly classic-close semantics.
11. **`r_character`'s R-hybrid** (see above) was reverse-engineered from R behavior
    (`as.character(1e14)` is `"1e+14"`; `1e5` is `"1e+05"`; `0.001` is `"0.001"` —
    shorter-of-two-forms, ties→fixed, NOT plain %.15g). Gate 2's harness re-judges it.

## Known debts (old, still open — fix opportunistically)

- **Wedge-recycle hole** (unchanged): provisioner + *attached* wedged runner ⇒ Recycle
  kills nothing ⇒ repeated ticks. Fix idea stands: `recycle_requested_ms` + evict.
- **Data-lane false wedge** (unchanged): no mid-job activity from the worker; a legit
  >30 s open looks hung. NOTE: fills are NOT dispatches (no credit) so a long build
  can't false-wedge — but a long `data_open` still can.
- `hung_snapshot`/`ready_snapshot`/`is_ever_registered` test helpers unused
  (`#[allow(dead_code)]`); `superseded`/`running` synthetics share `empty_payload`.
- Nothing calls `jasp_checkpoint()` yet (see below).
- New, small: stale `.tmp.<pid>` siblings under `views/` after a mid-build worker
  death — die with the session workspace wipe; harmless, noted.

## What's next (in order)

### 1. The runner-side READ seam — slice A DONE+GREEN; next: slice B (the seam)

Plan: `refactor_design/runner-views-read-design.md` (four slices). **Slice A (the
coercion-parity gate) is DONE and GREEN** — `tests/view_parity.R`, 144/144 over the real
binaries, and it REVISED the contract en route (design doc D9/D10): coercion semantics are
SYSTEM law, not R's — nominal is always unordered, non-finite is null (never a "NaN"
level), and f64→nominal level strings use the system `level_string` = plain **%.15g**
(15 significant digits, %g range rule; values agreeing at 15 digits are ONE category,
dedupe-on-string in the cast; `analysisview.rs`) mirrored in R by `jasp_level_string`
(`jaspRunner/R/data.R`, one sprintf) for the migration-era fallback — the mirror dies at
slice D with the fallback. Locale is display-time only (D10): canonical ASCII in data,
localize at render for the viewer (grid already does via ViewRender; results-label
localization, if ever, = canonical-string check or jaspResults format hints — noted, not
built). Legacy-JASP context: classic cast in C++ anyway, so R-exactness was never the real
contract (that's WHY D9).

**Slice B is DONE and GREEN too** (2026-09-03): `runner_jaspbase.R` reads `view_refs`
(`view_frame_from_ref` — codec rename, loud token_map cross-check, `__base_row`
dropped; pass-through refs stay lazy), preload = the frame, natives frame-first with
the miss ladder (frame hit → sibling coerce → migration lazy rung), and
`coerce_col`'s numeric→categorical branch flows through `factor_from_numeric`
(jasp_level_string — D9). Gate: `test_v2_views_ttest_e2e.R` — a dual-role t-test on
encoding_torture.csv run FOUR ways (fallback/views x preloadData true/false) over the
real stack, all byte-identical; the runner log pins the seam served (view read +
preload frame (view), 0.003 s vs 0.123 s). The libset convention:
JASP_ORCH_LIBSET=/home/sp42/jaspModuleTools/workdir/lib (its jaspAnova dir is a
complete library — one runner, every module). Supersede e2e + parity gate + walk
fixtures + all cargo suites re-run green. NOTE for slice C: a full jaspAnova e2e
needs the GUI's complete default options object (jaspBase fills no defaults for
absent keys) — the real frontend's options arrive complete, so it rides with slice C.

**Next: slice C's C++ half + D12 (the D11 flip's data plane is DONE + GREEN, 2026-09-04;
D12 DECIDED 2026-09-07, not yet implemented).** The storage vocabulary flip landed as
converged (D11, runner-views-read-design.md): the base cache's field names ARE the tokens
(`jasp_enc_hex_<hex(display)>`, no type suffix), the wire `ColumnInfo.name` IS the token
(display_name IS the decode), blob fields are `<token>_<type>` (= the R alias — the
runner's slice-B rename became an identity), the worker/edit-lane/`read_jasp_data` all
resolve either vocabulary (token first), and the walk is dual-vocabulary for the
migration window (the t-test e2e's bridge pair pins classic display-named options
byte-equivalent). All gates re-green: parity 145/145 (over the token vocabulary), walk_test
(+ §13 token fixtures), the t-test e2e (6 runs), the full cargo suite (classic 41+1i + 7 e2e ·
data_runner 94+1i · v2 19 · views e2e 1 · wire 5), clippy clean, supersede e2e green,
abort plane 6/6. **D12 (the owner's call, recorded in the design doc's decisions table):
the FRONTEND composes the suffix** — bound controls emit `token_type` values at the
emission point (where the `types` entry already sits), because slice C's staple must
implement `(token, type)` pairing in C++ anyway and the suffix is the general dual-role
convention, not a jaspBase quirk. **The slice C work list for the next agent:**
1. **C++ emission (D12):** compose `token_type` at the boundValues emission base — one
   site, value[i] × types[i] adjacent; bare/runtime-cast strings stay BARE;
   modelOriginal/model text untouched.
2. **C++ staple:** `createWorkJson` collects+dedupes the SAME `(token, type)` pairs into
   `work["views"]` (superset-tolerant); the AV3 `fullDataset` flag for the 5 offender
   sites + registry lint.
3. **R runner (small):** `rewrite_name` gains a pre-composed passthrough rung (an
   arriving `token_type` that decodes to a schema column passes through untouched; its
   `(display, type)` joins the pairs — ~5 lines). The classic encode path stays until
   slice D.
4. **Fixtures:** walk_test gains pre-composed passthrough fixtures; the t-test e2e gains
   a pre-suffixed-options run pinning C++-composed ≡ R-composed (same byte-identical
   harness as the existing bridge pair).
5. **Then the REAL FRONTEND TEST** (the user's explicit next milestone): build
   (`cmake --build build --target JASP`), run the GUI against v2, and check: analyses
   produce results; the terror_tall §8.6 memory recipes; **plot labels show display
   names** (the `decodeplot` → `decodeColNames` → runner-natives chain is verified
   in-code — jaspBase/R/writeImage.R:180-236 — but has ZERO e2e coverage: our e2e runs
   all plots OFF); **the rlang/jags model-editor extraction matches `displayName`
   post-D11** (pre-D11 name==display made this invisible — check
   `boundcontrolrlangtextarea`/`boundcontroljagstextarea`); saved .jasp loading (old
   options carry display names — the load path must bind them against the token schema).
After that: (D) retirement after the classic freeze (~670 → ~500 data-pipeline lines; the
coercion semantics then live in ONE place — the worker — AV4's whole point). Module-visible
behavior is unchanged throughout. jaspBase itself is untouched — the bridge contract
(`jaspbase-plugin.md` §4.1) is the stable seam. **NOTE: the D11 slice is UNCOMMITTED in
the working tree — commit it as one atomic slice BEFORE starting the C++ half.**

### 2. jasp_checkpoint() wiring (carried over, anytime)

jaspBase's check code must call it (wrap pattern above). Serves both orchestrators.

### 3. §12 step 7: differential + flip

Both orchestrators over the shared `frontend_*.R` scenarios; flip the desktop default
to v2; freeze classic for real.

### 4. Step 8: prefetch — only if dispatch-time fills show in the numbers.

Backlog finding (from slice A): the edit lane's scale→nominal RETYPE renders 'g'-10
canonical strings (`csv2arrow` P1) — lossy: distinct values beyond 10 significant
digits merge into one level. Pre-existing, unrelated to views; worth a ticket.

## Validate everything (from repo root)

```sh
cd orchestrator && cargo build -p data_runner      # BEFORE the views e2e (gotcha #9)
cd orchestrator && cargo test                      # 166 green (needs UNSANDBOXED — ipc)
cargo clippy --all-targets                         # clean — keep it that way
cargo run --quiet --bin jasp-orchestrator -- --schema | diff - messages.schema.json
Rscript -e 'parse(file="refactor_design/runner_jaspbase.R")'
Rscript refactor_design/test_abort_plane.R         # 6/6
Rscript refactor_design/test_v2_supersede_e2e.R    # §6 story; runner boot 60–180 s
```

### Environment gotchas (unchanged + two new from the D11 session)

- **NEW: build the worker before the views e2e** (gotcha #9 above).
- **NEW (2026-09-07): run EVERYTHING that spawns sockets with unsandboxed terminal
  permissions — not just ipc.** Inside the sandbox, AF_UNIX is blocked outright AND
  nanonext loopback TCP silently misbehaves (send() "succeeds", nothing arrives —
  the R gates hang at the hello handshake with a confusing `invalid connection`).
  Symptom guide: cargo e2e `Permission denied` on `ipc://`, or an R gate dying at
  handshake → you forgot `unsandboxed`.
- **NEW (2026-09-07): R `identical()` is encoding-sensitive** — a UTF-8-marked string
  (from `rawToChar`, the codec, `fromJSON`) is NOT `identical()` to a native-marked
  equal string (from Arrow/names()). Compare by `==` when the two sides have different
  provenance. Bit both `view_frame_from_ref`'s tripwire and the parity gate this session.
- The Zed sandbox blocks AF_UNIX binds — run cargo tests unsandboxed or
  `-- --skip routing_over_ipc`.
- `JASP_RUNNER_LIBDIR` is the MODULE dir (self-contained libpath), not the workdir root.
- Runner boot is slow (60–180 s); e2e polls "waiting for work" with a 180 s deadline.
- C++ desktop build/test rules: root `AGENTS.md`; `build/` is pre-configured.

## Rules of engagement (unchanged, still law)

- **P1** single-writer router (plain maps, no locks, never blocks, never FS/process
  I/O). **P2** dumb executors. **P3** correctness never depends on hints — determinism
  + content addressing backstop everything.
- Wire additive, `v` stays 1. Classic frozen (bugfix-only); its suite is the spec of
  record for everything unchanged.
- *One writer, many caches; a cache is always safe to drop.*
- Every step leaves the tree green and classic runnable.

*The caches are built; the runners can now drink from them. Make the reader as thin
as the writer was honest.* 📐
