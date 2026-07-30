# GUT_TODO — Wreckage from the NEO Gut (Phase 1)

> Status tracker for the "burn it down" pass that removed the in-process R engine,
> the boost shared-memory IPC, and the `EngineSync` scheduler.
> Recoverable: all deletions are in git history (branch `development`).
>
> **COMPILED & RUNS.** On Linux this builds against system packages via PkgConfig — **not**
> conan (conan is only used on the other platforms). The gutted desktop has been built and
> validated: configure passes, `JASPDesktopLib` compiles (verified in the podman devcontainer),
> a full local build succeeds, and the app **starts and runs**. The engine never initializes —
> by design: it was deleted. Replacing it with the orchestrator → runner is Phase 2 (§8).

---

## 1. What was DELETED (the subsystem)

| Path | Was |
|------|-----|
| `Desktop/engine/` (enginesync.*, enginerepresentation.*, rscriptstore.h) | Desktop-side scheduler + per-engine IPC wrapper |
| `CommonData/ipcchannel.{h,cpp}` | boost::interprocess shared-memory channel (sin #2) |
| `CommonData/rbridge.{h,cpp}` | C++↔R bridge (sin #1); coupled to R-Interface; unused by Desktop |
| `CommonData/databridge.{h,cpp}` | `DataBridge` base class (Engine-only); coupled to rbridge |
| `Engine/engine.{h,cpp}`, `Engine/main.cpp`, `Engine/CMakeLists.txt`, `Engine/JASPEngine.exe.manifest` | The `JASPEngine` child process |
| `Tests/testengine.{h,cpp}` | Test that drove EngineSync/EngineRepresentation |

**Kept:** `Engine/jaspBase/` (R package — the future runner needs it) and
`Engine/jaspModuleBundleManager/`. The `make_includable(../Engine/jaspBase/R/*.R …)`
lines in `CommonData/CMakeLists.txt` were left in place and still resolve.

## 2. What was de-R'd / de-IPC'd in the build

- `CMakeLists.txt` (top): removed `add_subdirectory(R-Interface)` (+ the Windows
  MinGW custom target) and `add_subdirectory(Engine)`. **Left in place (dead):** the
  `_LIB_R_INTERFACE_*` cache vars (~L165–185) — cosmetic cleanup later.
- `CommonData/CMakeLists.txt`: dropped R-Interface include/link, `${R_*}`/`${RCPP_*}`
  include paths, `BUILDING_JASP_ENGINE` + `R_HOME` defines, and the
  `BOOST_INTERPROCESS_*` defines. Still links boost (non-IPC), SQLite, LibArchive.
- `SyntaxInterface/CMakeLists.txt`: dropped R-Interface include/link and the
  `JASP_R_INTERFACE_LIBRARY` define.
- `Tests/CMakeLists.txt`: removed the `JASPTestEngine` target + its `add_test`.

### Found during the first compile (build-validation fixes)

Compiling flushed out the remaining references the grep couldn't see — all fixed:

- `Tools/CMake/Install.cmake`: removed `JASPEngine` from the three `install(TARGETS …)` rules
  (APPLE / LINUX / WIN32 branches).
- `Tools/CMake/Pack.cmake`: `CPACK_PACKAGE_EXECUTABLES` is now just `"JASP"` (dropped `JASPEngine`).
- `Desktop/CMakeLists.txt`: `add_dependencies(JASP JASPDesktopLib)` — dropped the `JASPEngine` dependency.
- `Desktop/data/datasetpackage.cpp`: added an explicit `#include "analysis/analysis.h"`. It used to
  get the full `Analysis` type *transitively* via the deleted `engine/enginesync.h`; without it the
  file fails with "invalid use of incomplete type 'class Analysis'".
- `Tools/CMake/R.cmake`: relaxed the `Rcpp`/`RInside` presence `FATAL_ERROR`s and the `REQUIRED`
  `libRInside` find. The desktop no longer embeds R (R-Interface is gone) and its C++ only touches
  Rcpp behind `#ifndef BUILDING_JASP`, so configure must not hard-fail when they're absent. They are
  still required for **R module builds** — which is exactly what the R-version issue below blocks.

### R toolchain — why the container can't build the full exe yet

- The devcontainer installs R **4.6.1** from the CRAN apt repo. R 4.6 removed `R_NamespaceRegistry`,
  which the repo's **pinned Rcpp 1.1.1** still references, so Rcpp/RInside fail to compile.
- Consequence: `renv` installs nothing → `jaspModuleBundleManager` is never built → `--target JASP`
  (which depends on the `Modules` target) fails with `missing and no known rule to make it`. This is
  the ONLY thing standing between the container and a full exe; `JASPDesktopLib` itself compiles fine.
- **Fix (matches the build guide's own advice):** compile R **from source** in the Dockerfile and set
  `CUSTOM_R_PATH`. Pin the version to whatever is known-good locally (the local build works), i.e. an
  R < 4.6 that Rcpp 1.1.1 compiles against — likely the current local R.
- On the user's local machine the full build succeeds and the app starts/runs (engine inert by design).

## 3. Central seams CUT (clean, grep-verified)

These no longer reference the deleted types at all:
- `Desktop/mainwindow.{h,cpp}` — include, member, ~14 connects, context property,
  and all method-body calls removed/stubbed. `enginesInitializing()` returns `false`.
- `Desktop/data/datasetpackage.{h,cpp}` — `setEngineSync`, `_engineSync`,
  `isThisTheSameThreadAsEngineSync` gone; `enginesPrepareForData/enginesReceiveNewData/
  stopEngines/restartEngines` are now no-ops.
  - **Preserved on purpose:** `enginesReceiveNewData()` still calls
    `ColumnEncoder::setCurrentColumnNames(getColumnTypesMap())` — that's column-name
    bookkeeping (sin #3), not engine comms. Don't remove it.
- `Desktop/analysis/analyses.h` — `friend class EngineSync;` removed.
- `Desktop/analysis/analysis.h` — header comment updated to point at the orchestrator.

**Clean check (passes):** grep for `enginesync|setEngineSync|isThisTheSameThreadAsEngineSync`
and `EngineSync::` over those files → zero matches.

---

## 4. Peripheral files — STUBBED to compile (functionally inert)

These referenced the deleted types and would have broken the build. They have been stubbed so
the desktop compiles, but the functionality is inert until reconnected to the orchestrator.
Grep over all compiled code (Desktop/CommonData/Common/QMLComponents) for the deleted symbols
now returns only two harmless *text* mentions (see §7).

1. **`Desktop/modules/dynamicmodules.cpp`** — STUBBED. Removed the `engine/enginesync.h`
   include and the `EngineSync::singleton()->killModuleEngine(...)` call in
   `refreshDeveloperModule`. *Reconnect later:* developer-module refresh should tell the
   orchestrator to recycle that module's runner.
2. **`Desktop/modules/modulelibrary.cpp`** — STUBBED. Removed the include and the constructor
   block connecting to the engine scheduler's module-install/uninstall signals. *Reconnect
   later:* module install/uninstall status comes from the orchestrator's module registry.
3. **`Desktop/qquick/rcommander.{h,cpp}`** — STUBBED (the R prompt is inert). Removed the
   `EngineRepresentation` forward-decl + `_engine` member and the `engine/enginesync.h` include;
   `runCode`/`addAnalysis`/`loadModule`/`processEngineChanges` now log-and-no-op (the class shape,
   Q_PROPERTYs and signals are kept so QML still binds). *Reconnect later:* reroute the R prompt
   to an `rcode` work-unit over the orchestrator.
4. **`SyntaxInterface/syntaxbridge.cpp`** — EXCLUDED FROM BUILD (not stubbed). The whole
   `SyntaxInterface` library is the headless jaspSyntax R-replay interface, built on
   `DataBridge`/`rbridge_*`. `add_subdirectory(SyntaxInterface)` is commented out in the
   top-level CMake; the file still contains references to the deleted headers, so do NOT
   re-enable it until it has a non-R-embedded path. Nothing in the desktop links it.

## 5. BROKEN — QML runtime (won't fail compile, will fail at runtime)

These reference the removed `engineSync` QML context property:

5. **`Desktop/components/JASP/Widgets/EnginesWindow.qml`** — `model: engineSync`,
   `engineSync.stopOrKillEngine(model.channel)`. It's a debug window opened via
   `MainWindow::showEnginesWindow()`. *Fix:* delete the window or point it at orchestrator/runner status.
6. **`Desktop/components/JASP/Widgets/ModulesMenu.qml`** — `engineSync.activateUtilEngine = visible`.
   *Fix:* drop the line (util-engine concept is gone).

## 6. Deferred functionality (no home yet in the new pipeline)

The PoC runs ONE hardcoded analysis on ONE hardcoded dataset. Everything below had its
EngineSync wiring removed and is not reconnected:

- Filters (`sendFilter`, filter-by-name) and computed columns (`computeColumn`).
- Module install / uninstall / dynamic-module engines.
- Image save / edit / rewrite (`SaveImg`, `EditImg`, `RewriteImgs` statuses still exist on
  `Analysis` but nothing services them).
- Analysis abort, result streaming (`running` status / progress), runner pool, heartbeat/liveness.
- Settings sync (PPI/font/language → engine) and "reload data" / "clean restart".
- R commander / R prompt.
- **Analysis error propagation** — *resolved, not deferred*: catchable R errors carry a full
  collapsible stack trace end-to-end (verified — jaspBase `.addStackTrace` → `errorMessage` →
  runner → results panel, unchanged by the rebuild). A hard runner crash (C++ `abort`/segfault)
  kills the runner; the orchestrator now fails the outstanding work with a friendly "A fatal
  crash occurred while running the analysis" message (verified). **Deferred**: per-analysis
  *crash isolation* (run each analysis in a child process so one crash can't kill the whole
  runner) — deliberately skipped for now; the friendly error is the interim behavior.

## 7. Cosmetic leftovers (harmless, don't break compile)

- `Desktop/analysis/analyses.cpp` ~L1311: comment mentions "EngineSync".
- `Desktop/analysis/analysis.cpp` ~L430: `throw std::logic_error(...)` string references
  `EngineRepresentation::analysisResultStatusToAnalysStatus` (developer-facing text only).
- `MainWindow::pauseEngines()` / `resumeEngines()`: declared in `mainwindow.h`, never defined,
  never called (only the deleted connects invoked them). Dead declarations — remove when convenient.
- `datasetpackage.cpp`: now-unused `#include <QThread>` (was for the deleted thread check).
- Top-level `CMakeLists.txt`: dead `_LIB_R_INTERFACE_*` cache vars.

## 8. Next phase

See `refactor_design/poc-handoff.md` §3–§5: build the Rust orchestrator + R runner, then a
thin `JaspClient` (NNG) that slots into `MainWindow` exactly where `_engineSync` used to live
trigger via `Analysis::shouldRun()`, results via `Analysis::setResults()`). Toolchain needed:
system packages via PkgConfig on Linux (NOT conan — boost/sqlite/libarchive etc. come from apt),
`cargo`/`rustc` (orchestrator), and `nng` (C++ client; the R runner's `nanonext` bundles its own).
To run analyses the container also needs a from-source R (see §2 "R toolchain").
