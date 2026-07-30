# HANDOVER — Runner incremental recompute is failing (root cause isolated, not yet fixed)

**Status:** the single-threaded router refactor is **done and green** (8/8 Rust tests + clippy; see
`orchestrator-router-design.md`). The runner's **incremental recompute** — reusing a base
revision's cached plots so a re-run doesn't re-render — is **implemented but failing**: plots
re-render every revision. Root cause is isolated to jaspBase's recompute-*state* persistence; it is
**not yet fixed**. This is the live issue.

## What works

- `runner_jaspbase.R` "jaspBase accommodation" block (~L207): `setWriteSealLocation` +
  `setSaveLocation(outputDir, "jaspResults.json")` called before `runJaspResults`; copy-on-seed
  copies the base revision's dir into the new revision; temp-file counter continues past the base's
  images (`jaspbase_max_temp_index`).
- The **results tree** now persists: `setSaveLocation` fixed the first bug (`saveResults()` was
  no-op'ing with "Did not store jaspResults" because the save location was never set). Rev 1 logs
  `found a plot with name: x` — the old results ARE loaded.
- `JASP_ORCH_KEEP_WORKSPACES=1` preserves workspaces for inspection.

## What's broken

Plots **re-render every revision.** `results_1/` gains a fresh `jasp-3.png`/`jasp-4.json` even
though the base's `jasp-1.png` was copied in byte-identical. The test `frontend_recompute.R` FAILs.

## The diagnosis

jaspResults persists a revision across **three separate things**; only two survive:

1. **`jaspResults.json` / `.rds`** — the results *tree* (what the frontend renders). ✅ works now.
2. **`jasp-N.png` / `.json`** — rendered images, separate files. ✅ copied by copy-on-seed.
3. **`jaspState.RData`** — the **R plot-object storage** that recompute *actually* keys off.
   `fillEnvironmentWithStateObjects` loads it so a re-run finds the existing plot object *carrying
   its image path* (`_filePathPng`) and skips re-rendering (gate in `jaspPlot.cpp`:
   `if (_filePathPng != "" && !_editing) return;`). **This file comes back EMPTY (0 bytes) in rev 1.**

Image reuse depends on #3 carrying the plot object *with `_filePathPng`*. Empty → re-render.

### Smoking gun (workspace preserved at `/home/sp42/neoTest/s-1/w-recompute/`)

```
results_0/jaspState.RData   123466776 bytes   (123 MB — saved OK)
results_1/jaspState.RData           0 bytes   (EMPTY)

results_1/jasp-1.png   byte-identical to results_0's  (the copy, not a re-render)
results_1/jasp-3.png   9658 bytes   (FRESH re-render of the raincloud)
results_1/jasp-4.json  48580 bytes  (its plotly data)
```

Runner log — note `Now rendering a plot with name: x` prints in EVERY run incl. rev 1:
```
[rev 0]       Now rendering a plot with name: x / could not find an old plot
[rev 0 retry] Now rendering a plot with name: x / found a plot with name: x. Resized by user: no
[rev 1]       Now rendering a plot with name: x / found a plot with name: x. Resized by user: no
```

### Red herring — don't be misled

`found a plot with name: x` is **not** the image-reuse path. It's `getOldPlotInfo`
(`jaspPlot.cpp` ~L211) reading the *results tree* to copy *edit/resize options*. The image-reuse
path is the `_filePathPng` gate, fed by `jaspState.RData`.

## Root-cause hypothesis (not yet confirmed)

`.saveState()` (jaspBase `R/common.R` ~L662) saves
`list(figures = getPlotObjectsForState(), other = getOtherObjectsForState())`. In rev 1 it wrote an
**empty** file → `getPlotObjectsForState()` returned nothing. Leading hypothesis:
**`emptyRecomputed()`** (called at the start of each analysis inside `runJaspResults` — "ensure an
analysis always starts with a clean hashtable of computed jasp Objects") clears the computed-objects
storage that `fillEnvironmentWithStateObjects` just loaded from the copied state, so by the time
`.saveState` runs the plot objects are gone. Alternative: the plot objects never land in
`.plotStateStorage` in our runner setup at all.

## How to reproduce / inspect

```sh
sh refactor_design/run_recompute_test.sh
```
Boots orchestrator (`tcp://127.0.0.1:9572`) + `runner_jaspbase.R` (jaspTTests 0.95.5, libdir
`/home/sp42/jaspModuleTools/workdir/jaspTTests`) + `frontend_recompute.R` (submits `w-recompute`
rev 0 = raincloud ON / Welch off, then rev 1 with `base_revision: 0` / Welch ON). PASS =
`results_1/` gains NO new artifact. Workspace preserved at
`/home/sp42/neoTest/s-1/w-recompute/results_{0,1}` (`JASP_ORCH_KEEP_WORKSPACES=1`).

**Inspect the state file directly (the key next move):**
```sh
Rscript -e 'e <- new.env(); load("/home/sp42/neoTest/s-1/w-recompute/results_0/jaspState.RData", envir=e); print(ls(e)); str(e, max.level=2)'
```
Does the loaded plot object carry an image path? Then trace why the *save* produces an empty state.

## What to investigate next (the actual root cause)

In `/home/sp42/jaspBase/`:
- `R/common.R` — `runJaspResults` (~L72), `emptyRecomputed`, `.saveState` (~L662), `.retrieveState`
  (~L679), `getPlotObjectsForState`, `getOtherObjectsForState`.
- `src/jaspResults.cpp` — `saveResults` (~L185, no-ops if `_saveResultsHere==""`), `loadResults`,
  `complete` (calls `saveResults(); send(); finishWriting()`), `fillEnvironmentWithStateObjects`,
  `getPlotObjectsForState` (~L432), `getOtherObjectsForState` (~L465).
- `src/jaspPlot.cpp` — render gate `if (_filePathPng != "" && !_editing) return;` (~L80),
  `getOldPlotInfo` (~L211).

**Goal:** find why `getPlotObjectsForState()` returns empty (or why plot objects don't carry
`_filePathPng`), then fix the runner accommodation so the state round-trips and plots are reused.

**Secondary noise (not the root cause):** `submit_and_wait` retries the send on timeout, so each
revision is processed several times (duplicate `fe->rn work` lines + a `stale result … rev 0 < 1 —
dropping`). Multiplies re-renders but isn't the cause.

## Settled decisions — do NOT relitigate

- **Single-threaded router** + mpsc mailbox; router never does blocking I/O (a janitor thread does
  `rm -r`). See `orchestrator-router-design.md`.
- **Per-revision self-contained dirs** `results_<rev>/`; **copy-on-seed** (copy base dir into the
  new revision); **frontend-declared `base_revision`**; **revision-granular `work_close`**.
- The **jaspBase accommodation block** (`setWriteSealLocation` + `setSaveLocation` before
  `runJaspResults`) is correct and necessary — it fixed results-*tree* persistence. The remaining
  gap is the recompute *state* (`jaspState.RData`), a separate mechanism.
- `found a plot` = edit/resize-option recovery, **not** image reuse.

## Key files

- `refactor_design/runner_jaspbase.R` — runner; accommodation block ~L207–243, seeding ~L303–310.
- `refactor_design/frontend_recompute.R` — two-revision recompute test frontend.
- `refactor_design/run_recompute_test.sh` — boots the stack + runs the test, preserves workspace.
- `orchestrator/src/main.rs` — the router (done, green).
- jaspBase source: `/home/sp42/jaspBase/{R/common.R, src/jaspResults.cpp, src/jaspPlot.cpp}`.

## Deferred follow-ups (after recompute works)

- Migrate `integration_test.R` to the new handshake (still old two-socket protocol).
- `jaspclient` (C++) live validation (compiles, not yet run against the orchestrator).
- `neo-jasp.md` §19.5 spec delta + document `base_revision`/`work_close`.
- Stale-§5 pointer in old `orchestrator-design.md`. `runner_alpha.R` activity-ping parity (optional).
