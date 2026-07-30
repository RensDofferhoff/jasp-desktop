# Runner ↔ jaspBase plugin contract (adapt to jaspBase as-is; do NOT refactor it yet)

> Status: design doc, no code. The runner (the future `jaspRunner`, spec §12 Phase 2) plugs into
> **jaspBase as it exists today** (`Engine/jaspBase/`, already compiled in the runner's libpath).
> jaspBase may later be refactored ("jaspSlop") behind this same seam, but for now we adapt to it.
> This doc pins the seam exactly so that refactor is a swap, not a rewrite.
>
> **Concrete libpath (alpha):** `/home/sp42/jaspModuleTools/workdir/jaspTTests` is a fully
> installed package library (built by jaspModuleTools/renv) containing `jaspBase` **0.20.4**
> (with compiled `libs/jaspBase.so` = the Rcpp `jaspResults` module), `jaspTTests` **0.95.5**,
> `jaspGraphs`, and all heavy deps (Rcpp 1.1.2, BayesFactor, car, ggplot2, plotly, …). The
> runner's startup sets `.libPaths()` to this dir (§1a step 1).

## 1. What the runner reuses vs. what it supplies

The runner already owns transport, registration, framing, and the work loop
(`refactor_design/runner_alpha.R`, spec §17–§20). Per `work` unit it delegates the *computation*
to jaspBase and lets jaspBase serialize the results. jaspBase is reused **unmodified**.

What the runner supplies is small: an **entry-point call** (§3) plus a handful of **bridge
functions/values** (§4) that stand in for the C++ engine that used to back them.

## 1a. Runner lifecycle — when it starts, what it loads, what it advertises

A runner is **module-typed and warm at startup** (spec §6.1): it loads a known module set *once,
at process start*, and its `register` advertises **exactly what it loaded**. Capability = what is
warm, by construction — the work message never triggers a load; it only selects *which* of the
already-loaded analyses to run.

**Who starts the process — two models:**

| | Alpha (now) | Production (§6, deferred) |
|---|---|---|
| Starter | started **by hand** (`runner_alpha.R`) | **orchestrator spawns** a runner for a needed capability it doesn't have warm |
| Module set | fixed (e.g. jaspTTests) | passed as config/argv to the spawned process |
| Pool/rotation | none (one runner) | LRU pool, make-before-break rotation (§6.2/§6.4) |

The *internal* startup order is identical in both; only who launches the process differs.
"Orchestrator spawns typed runners" is the later pool work — kept separate from the jaspBase
hookup so each is a small step.

**Startup order (once, before the work loop):**

```
1. .libPaths(c(LIBDIR, .libPaths()))        # point R at the compiled module library
2. library(jaspBase)                        # loads pkg + Rcpp::loadModule("jaspResults")
                                            # (R/zzaLoadModule.R) -> create_cpp_jaspResults etc.
3. define bridge natives in globalenv       # §4 — engine stand-in (see §2)
4. jaspBase::initEnvironment()              # registerFonts + preload BayesFactor;
                                            # setwd(.requestTempRootNameNative()$root)
5. library(<module set>)                    # EAGER — e.g. library(jaspTTests); brings
                                            # jaspGraphs/BayesFactor/car... via NAMESPACE Imports
6. register(capabilities = [ analysis{name:<module>, version:<ver>}, ... ])
                                            # advertise EXACTLY the modules step 5 loaded
7. enter work loop — serve only work matching the registered capabilities
```

Order matters: the bridge natives (step 3) must be installed **before** `initEnvironment()`
(step 4), because `initEnvironment` calls `.requestTempRootNameNative` to `setwd`. Module load
(step 5) is eager and **before** registration (step 6) so the advertisement is truthful.

> **Alpha note:** the current `runner_alpha.R` advertises a placeholder `base-r@0.1` and computes
> a hello-world by hand. The first real hookup replaces that with step 5 (`library(jaspTTests)`) +
> a truthful `analysis{name:"jaspTTests", version:"0.95.5"}` capability, and the per-work body
> becomes `runJaspResults` (§3).

## 2. Why no engine is needed — the standalone path

jaspBase distinguishes "inside JASP" from standalone via `jaspResultsCalledFromJasp()` →
`isInsideJASP()` (`R/zzzWrappers.R:110`, `src/jaspModuleRegistration.h`). `_insideJASP` defaults
to `false`; the runner never calls the `setInsideJasp` setter, so it is **always in standalone
mode**. In standalone mode:

- `runJaspResults` takes the `!jaspResultsCalledFromJasp()` branch (`R/common.R:99-144`): it sets
  its own display defaults and returns the `jaspResults` R6 object (not engine IPC JSON).
- The data natives (`.readDatasetToEndNative`, …) are resolved by `.fromRCPP` via plain
  `exists(x)` / `utils::getAnywhere(x)` (`R/common.R:643-651`). In the engine these are C++
  functions; **the runner defines R functions of the same names**, and jaspBase calls them.
  This is the whole trick — no IPC, no edit to jaspBase.

> **Where the bridge functions live — decided empirically.** `.fromRCPP` resolves a native by
> `exists(x)` then `utils::getAnywhere(x)` (`R/common.R:643-651`). Tested against installed
> jaspBase 0.20.4 (`refactor_design/_loadprobe.R`):
> - **Define them in `globalenv()` → WORKS.** `.fromRCPP`'s `exists()`/`getAnywhere()` *does* reach
>   globalenv even though the call originates in the jaspBase namespace. (An earlier draft of this
>   doc claimed the imports-parent chain skipped globalenv — that was wrong; the probe disproved it.)
> - **Injecting into the namespace → FAILS.** `assign(..., envir = asNamespace("jaspBase"))` errors
>   with "cannot add bindings to a locked environment" — namespaces are locked after load.
>
> So the runner just **defines the natives at top level (globalenv)** — e.g. `.readDatasetToEndNative
> <- function(...) ...` in the runner script. No namespace hacking. (Re-confirmed for
> `initEnvironment`'s `base::exists(".requestTempRootNameNative")` at `R/common.R:214`, which also
> reaches globalenv — so the temp-root `setwd` fires.)

## 3. The entry point

`jaspBase::runJaspResults(name, title, dataKey, options, stateKey, functionCall = name,
preloadData = FALSE)` (`R/common.R:72`, exported). It:

1. builds the C++ `jaspResults` tree (`loadJaspResults` → `create_cpp_jaspResults(name, .retrieveState())`);
2. parses `dataKey`/`options`/`stateKey` as JSON;
3. resolves the analysis fn: `analysis <- eval(parse(text = functionCall))` (`R/common.R:106`);
4. calls `analysis(jaspResults = jaspResults, dataset = dataset, options = options)` (note: **no
   `ready`** arg — analyses reach "ready" through their own `dependOn`/re-run logic);
5. on error, records it on the tree; on success, `finishJaspResults()` and returns the R6 object.

**The runner calls it per work unit:**

```r
jr <- jaspBase::runJaspResults(
  name         = work$payload$analysis,
  title        = work$payload$analysis,
  dataKey      = "{}",
  options      = toJSON(work$payload$options),   # JSON string; runJaspResults fromJSONs it
  stateKey     = "{}",
  functionCall = paste0(work$payload$module, "::", work$payload$analysis),
  preloadData  = FALSE)                          # analyses pull data on demand via the bridge
```

**`functionCall` mapping.** Analysis functions are *exported* (verified in
`jaspTTests/NAMESPACE`: `export(TTestIndependentSamples)`, …). By JASP convention the
analysis-entry name *is* the exported R function name, so `module::analysis` resolves. The
`…Internal` variants (e.g. `TTestIndependentSamplesInternal`) are the RSyntax wrappers used by
`runWrappedAnalysis` (`R/common.R:1198`) — not the runner's path. **Caveat:** `Analysis::name()`
(frontend `payload.analysis`) and the R function name are matched case-sensitively by `eval`;
verify they agree per module (a mismatch fails loudly, never silently).

**Getting the result JSON.** The C++ object exposes `getResults()` → `constructResultJson()` →
`{ "typeRequest": "analysis", "results": <dataEntry()> }` (`src/jaspModuleRegistration.h:230`,
`src/jaspResults.h:56`). `dataEntry()` is the **live web form** (`.meta` + same-named top-level
nodes) that `Desktop/html/js/analysis.js` renders — the same shape `runner_alpha.R`'s hello-world
proved renders. The R6 wrapper keeps `getResults` private, so reach the C++ object:
`jr$.__enclos_env__$private$jaspObject$getResults()`. The runner sends `payload$results =
<that>$results`. jaspBase owns serialization end-to-end; the runner hand-builds nothing.

## 4. The bridge — what the runner defines

`.fromRCPP(x, ...)` (`R/common.R:622`) whitelists exactly these names and resolves each by
`exists(x)` then `getAnywhere(x)`. Functions are `do.call`ed with the args; non-functions are
returned as values. The runner supplies all of them.

### 4.1 Data natives (functions, defined once; read the per-work dataset)

| Name | Call site / args | Returns |
|---|---|---|
| `.readDatasetToEndNative(columns, columns.as.numeric, columns.as.ordinal, columns.as.factor, all.columns)` | `R/common.R:385` via `.readDataSetToEnd` | a `data.frame` of the requested columns, coerced to the requested types |
| `.readDataSetHeaderNative(columns, columns.as.numeric, columns.as.ordinal, columns.as.factor, all.columns)` | `R/common.R:413` via `.readDataSetHeader` | a `data.frame` of column names/types (header) |
| `.readDataSetRequestedNative()` | `R/common.R:110` (only if `preloadData=TRUE`) | the full requested `data.frame` (unused while `preloadData=FALSE`) |

All three bottom out in `.fromRCPP` from the exported `readDataSetToEnd`/`readDataSetHeader`
(`R/exposeUs.R`). Column args are character vectors of (encoded) column names; `all.columns=TRUE`
means every column. The runner coerces scale→numeric, ordinal→ordered factor, nominal→factor.

### 4.2 Temp / state natives (functions, defined once; resolve inside the per-work scratchpad)

| Name | Call site | Returns |
|---|---|---|
| `.requestTempRootNameNative()` | `R/common.R:215` (`initEnvironment`, does `setwd(paths$root)`) | `list(root = <output_dir>)` |
| `.requestTempFileNameNative(ext)` | `R/common.R:825`, `R/writeImage.R:32`; `ext` ∈ `"png"`,`"json"` | `list(root = <output_dir>, relativePath = <unique file.ext>)` — full path is `paste(root, relativePath, sep="/")` |
| `.requestStateFileNameNative()` | `R/common.R:93,663,685` | `list(root = <output_dir>, relativePath = <state file>)` |

`.saveState` calls `.requestStateFileNameNative` **unguarded** (`R/common.R:663`), so it must exist
even though the alpha ignores state — point it at a throwaway file in the scratchpad.

### 4.3 Per-work values (set from the work unit, before each run)

| Name | Read via | Set from |
|---|---|---|
| `.ppi` | `.fromRCPP(".ppi")` (`R/common.R:841`, `R/writeImage.R:34`) | `work$payload$settings$ppi` |
| `.imageBackground` | `.fromRCPP(".imageBackground")` | `work$payload$settings` (or session default) |
| `.baseCitation` | `.fromRCPP(".baseCitation")` | base citation (session default) |

These are **per-work**, not startup config — the work unit is invariant (§4 of `neo-jasp.md`) and
carries its own `ppi`/`numDecimals` in `settings`; re-running a work must reproduce its own
rendering. (`.numDecimals`/`.fixedDecimals`/`.normalizedNotation`/`.exactPValues` are set by
`runJaspResults` itself in standalone mode, `R/common.R:100-103` — the runner need not.)

## 5. Per-work sequence

Module loading is **not** here — it happened at startup (§1a, step 5). Per work the runner only
selects an already-loaded analysis and refills the per-work context:

```
on work:   # work.payload.module is guaranteed loaded + advertised (§1a)
  1. context  <- establish per-work context:
       dataset    <- read dataset for work$dataset_ids  (CSV now; Arrow/Feather later, §8)
       output_dir <- work$output_dir                    (orchestrator-injected scratchpad, §19.3)
       .ppi / .imageBackground / .baseCitation <- from work$payload$settings
                    (re-assigned into globalenv — per-work, invariant §4; see §2: globalenv works,
                     the namespace is locked)
  2. jr <- jaspBase::runJaspResults(                      (§3)
            functionCall = paste0(work$payload$module, "::", work$payload$analysis), ...)
  3. results <- jr$.__enclos_env__$private$jaspObject$getResults()$results
  4. send `result` { work_id, revision, status:"complete", payload:{ results } }
```

The once-installed natives (§4.1, §4.2) read whatever context step 1 just set. Errors surface
inside `runJaspResults` and land on the tree as `fatalError`/`validationError` — the runner
forwards that status verbatim.

## 6. Known alpha limits / future seams

- **Runner lifecycle is manual + single-module for now.** The alpha is started by hand and typed
  to one module (jaspTTests), advertising it truthfully. **Orchestrator-spawned typed runners +
  the LRU pool/rotation (§6) are deferred** — that is separate work from the jaspBase hookup.
- **Dataset bridge is CSV → data.frame for now.** The real path is the Arrow cache (§8): the
  orchestrator owns `dataset_id → cache_path`, the runner mmaps Feather. Only §4.1 changes; the
  entry point and serialization do not.
- **No streaming partial results.** jaspBase can `send()` partial results mid-run (the engine
  relayed them); the alpha runs to completion and sends one final `result`. The client already
  tolerates a stream (same `work_id`/`revision`), so streaming is additive later.
- **No abort mid-run.** R interruption (§7) is a runner concern; jaspBase has a
  `jaspAnalysisAbort` condition the runner can raise to stop a run.
- **State/`output_dir` cleanup** is the orchestrator's job (§19.3); the runner writes inside
  `output_dir` and references artifacts by relative path.
- **jaspBase refactor ("jaspSlop") later** happens behind this contract: §3 (entry) + §4 (bridge)
  are the stable seam; only their internals change.
