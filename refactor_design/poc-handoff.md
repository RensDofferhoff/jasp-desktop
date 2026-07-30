# PoC Handoff — Gut jasp-desktop, Wire Up Frontend → Rust Orchestrator → R Runner

> **Goal:** Strip the legacy IPC/engine/R-embedding path out of jasp-desktop and replace
> it with a minimal NNG-based pipeline: Qt frontend → Rust orchestrator → R runner.
> Hardcode everything that isn't on the critical path. One dataset, one analysis.
>
> **Normative specs:** `neo-jasp.md` (architecture), `HANDOVER.md` (decisions + rationale).
> This document is the *implementation plan*. When in doubt, the specs win.

---

## 0. Guiding Principle

**Gut first, build second.** The codebase compiles as a monolith with R embedded via
RInside, IPC via boost shared memory, and engine lifecycle management baked into the
frontend. We remove those three subsystems surgically, stub the seams so it still
compiles, then wire in the new components one at a time.

Do NOT try to build the orchestrator/runner/client in isolation and "connect later."
The value is in proving the frontend can drive the new pipeline end-to-end.

---

## 1. Current Architecture (What Exists)

### 1.1 The Three Layers Being Removed

```
┌─────────────────────────────────────────────────────────┐
│  Desktop (Qt/QML frontend)                              │
│  ┌──────────┐  ┌───────────┐  ┌──────────────────────┐ │
│  │ Analysis  │  │ Analyses  │  │ EngineSync           │ │
│  │ (model)   │→│ (list     │→│ (scheduler, singleton)│ │
│  │           │  │  model)   │  │  polls shouldRun()   │ │
│  └──────────┘  └───────────┘  └──────┬───────────────┘ │
│                                      │                  │
│                              ┌───────▼────────┐         │
│                              │EngineRepresent- │         │
│                              │ation (per-proc) │         │
│                              └───────┬────────┘         │
│                                      │                  │
│  CommonData:                 ┌───────▼────────┐         │
│  ┌──────────────┐            │ IPCChannel     │         │
│  │DataSetPackage│◄──────────►│ (boost shm)    │         │
│  │DatabaseInterf│            └───────┬────────┘         │
│  │Column,DataSet│                    │                  │
│  └──────────────┘                    │                  │
└──────────────────────────────────────┼──────────────────┘
                                       │  shared memory
┌──────────────────────────────────────▼──────────────────┐
│  Engine process (JASPEngine binary)                      │
│  ┌──────────┐  ┌──────────┐  ┌────────────────────────┐ │
│  │ Engine    │→│ rbridge  │→│ RInside (embedded R)    │ │
│  │ (recv/   │  │ (C++↔R)  │  │ jaspBase, jaspModules  │ │
│  │  dispatch)│  │          │  │                        │ │
│  └──────────┘  └──────────┘  └────────────────────────┘ │
└─────────────────────────────────────────────────────────┘
```

### 1.2 Key Files and Their Roles

| File | Role | PoC Fate |
|------|------|----------|
| `Desktop/analysis/analysis.h/cpp` | Single analysis instance. Status machine. `createAnalysisRequestJson()` builds the wire message. `setResults()` receives results. `shouldRun()` is the trigger EngineSync polls. | **KEEP + MODIFY** |
| `Desktop/analysis/analyses.h/cpp` | Singleton list model of all analyses. `friend class EngineSync`. RPC handlers (~550 lines). `applyToAll()` iteration. | **KEEP + MODIFY** (remove EngineSync friendship) |
| `Desktop/engine/enginesync.h/cpp` | Singleton scheduler. Timer-driven `process()` loop. Polls `shouldRun()`, dispatches to engines. ~20 signal/slot connections in mainwindow. | **REMOVE** |
| `Desktop/engine/enginerepresentation.h/cpp` | Per-engine wrapper. Sends via `channel()->send()`, receives via `processReplies()`. Calls `analysis->setResults()`. | **REMOVE** |
| `Desktop/engine/rscriptstore.h` | Structs for R requests (filter, rcode, computed column). | **REMOVE** |
| `CommonData/ipcchannel.h/cpp` | Boost shared memory IPC. Two channels per engine. Heartbeat. | **REMOVE** |
| `CommonData/databridge.h/cpp` | DataBridge base class (Engine inherits from it). | **KEEP** (used by CommonData internals) |
| `CommonData/databaseinterface.*` | SQLite DB interface for dataset storage. | **KEEP** (frontend data layer) |
| `CommonData/column.*`, `dataset.*` | Column/dataset abstractions. | **KEEP** (frontend data layer) |
| `CommonData/rbridge.*` | R bridge functions (rbridge_readDataSet etc). | **REMOVE** (only used by Engine process) |
| `Common/enginedefinitions.h` | Enums: `engineState`, `performType`, `analysisResultStatus`. | **KEEP** (Analysis uses these) |
| `Engine/engine.h/cpp` | Engine process: receives JSON, dispatches to R. | **REMOVE** |
| `Engine/main.cpp` | Engine process entry point. | **REMOVE** |
| `Engine/jaspBase/` | R package (analysis infrastructure). | **KEEP** (runner uses it) |
| `R-Interface/` | RInside/Rcpp C++ library. | **REMOVE** |
| `Desktop/data/datasetpackage.h/cpp` | Dataset lifecycle. Has `EngineSync* _engineSync` member. | **KEEP + MODIFY** |
| `Desktop/mainwindow.h/cpp` | App wiring. Creates Analyses, EngineSync. ~20 EngineSync connections. | **KEEP + MODIFY** |
| `Desktop/main.cpp` | Entry point. | **KEEP** |

### 1.3 Critical Integration Points (Read These Carefully)

These are the exact seams where the old path connects. Understanding them is essential
for the gutting.

**A. Analysis → EngineSync (the trigger)**

`Analysis::run()` (analysis.cpp L263-268) just sets status to `Empty`:
```cpp
void Analysis::run() {
    if (_isReport) return;
    setStatus(Empty);  // EngineSync polls shouldRun() and picks this up
}
```

`Analysis::shouldRun()` (analysis.h L121):
```cpp
bool shouldRun() {
    return !isWaitingForModule() && (isSaveImg() || isEditImg() || isRewriteImgs() || isEmpty())
           && form() && !_isReport;
}
```

`EngineSync::processAnalysisRequests()` (enginesync.cpp L819-884) polls all analyses:
```cpp
Analyses::analyses()->applyToAll([&](Analysis * analysis) {
    if(analysis && analysis->shouldRun()) {
        // find or create engine for this module
        engine->runAnalysisOnProcess(analysis);
    }
});
```

**B. EngineSync → Engine (the send)**

`EngineRepresentation::runAnalysisOnProcess()` (enginerepresentation.cpp L506-522):
```cpp
void EngineRepresentation::runAnalysisOnProcess(Analysis *analysis) {
    setAnalysisInProgress(analysis);
    Json::Value json(analysis->createAnalysisRequestJson());
    channel()->send(json.toStyledString());  // boost IPC
}
```

`Analysis::createAnalysisRequestJson()` (analysis.cpp L659-696) builds:
```json
{
  "typeRequest": "analysis",
  "id": 1,
  "perform": "run",
  "preloadData": true,
  "revision": 0,
  "rfile": "",
  "dynamicModuleCall": "jaspDescriptives::Descriptives",
  "resultFont": "...",
  "name": "Descriptives",
  "title": "Descriptives",
  "options": { "variables": ["col1", "col2"] }
}
```

**C. Engine → Frontend (the receive)**

`EngineRepresentation::processAnalysisReply()` (enginerepresentation.cpp L540-665):
```cpp
void EngineRepresentation::processAnalysisReply(Json::Value & json) {
    int id       = json.get("id", -1).asInt();
    int revision = json.get("revision", -1).asInt();
    Json::Value progress = json.get("progress", Json::nullValue);
    Json::Value results  = json.get("results", Json::nullValue);
    analysisResultStatus status = analysisResultStatusFromString(json.get("status", "???").asString());

    // Stale-result check:
    if(analysis->revision() > revision && status != analysisResultStatus::imagesRewritten)
        return;  // ignore old revision

    switch(status) {
    case analysisResultStatus::complete:
    case analysisResultStatus::fatalError:
    case analysisResultStatus::validationError:
        analysis->setResults(results, status);
        clearAnalysisInProgress();
        break;
    case analysisResultStatus::running:
        analysis->setResults(results, status, progress);
        break;
    // ... imageSaved, imageEdited, imagesRewritten
    }
}
```

**D. MainWindow wiring** (mainwindow.cpp):
```cpp
// Constructor:
_analyses   = new Analyses();
_engineSync = new EngineSync(this);

// ~20 connections including:
connect(_engineSync, &EngineSync::computeColumnSucceeded, ...);
connect(_engineSync, &EngineSync::engineTerminated, this, &MainWindow::fatalError);
connect(_engineSync, &EngineSync::refreshAllPlotsExcept, _analyses, &Analyses::refreshAllPlots);
connect(_engineSync, &EngineSync::processNewFilterResult, _filterModel, ...);
connect(_filterModel, &FilterModel::sendFilter, _engineSync, &EngineSync::sendFilter);
connect(_computedColumnsModel, &ComputedColumnModel::sendComputeCode, _engineSync, &EngineSync::computeColumn);

// QML context:
_qml->rootContext()->setContextProperty("engineSync", _engineSync);

// DataSetPackage:
DataSetPackage::pkg()->setEngineSync(this);  // in EngineSync constructor
```

**E. DataSetPackage → EngineSync** (datasetpackage.h/cpp):
```cpp
class EngineSync;  // forward decl
EngineSync * _engineSync = nullptr;
void setEngineSync(EngineSync * engineSync);
bool isThisTheSameThreadAsEngineSync();
void enginesPrepareForData();   // calls _engineSync->enginesPrepareForData()
void enginesReceiveNewData();   // calls _engineSync->enginesReceiveNewData()
void stopEngines();             // calls EngineSync::singleton()->stopEngines()
void restartEngines();          // calls EngineSync::singleton()->restartEngines()
```

---

## 2. The Gut Plan

### Phase 0: Remove the Three Subsystems

Do this FIRST. Get it compiling with stubs before building anything new.

**Step 0.1: Remove Engine process and R-Interface from build**

Edit `CMakeLists.txt` (top-level):
- Comment out `add_subdirectory(Engine)` (L226)
- Comment out `add_subdirectory(R-Interface)` (L197 or L202 depending on platform)
- Keep `add_subdirectory(Common)`, `CommonData`, `QMLComponents`, `SyntaxInterface`, `Desktop`

**Step 0.2: Remove ipcchannel from CommonData**

- Delete `CommonData/ipcchannel.h` and `CommonData/ipcchannel.cpp`
- Delete `CommonData/rbridge.h` and `CommonData/rbridge.cpp` (only used by Engine process)
- Edit `CommonData/CMakeLists.txt`:
  - Remove `make_includable(../Engine/jaspBase/R/...)` lines (L14-20) — these embed R source
    into C++ headers for the Engine. Not needed when R runs standalone.
  - Remove R-Interface from `target_include_directories` and `target_link_libraries`
  - Remove `${R_INCLUDE_PATH}`, `${R_HOME_PATH}/include`, `${RCPP_PATH}/include`
  - Remove `BUILDING_JASP_ENGINE` compile definition
  - Remove boost interprocess definitions (`BOOST_INTERPROCESS_SHARED_DIR_FUNC`,
    `BOOST_INTERPROCESS_BOOTSTAMP_IS_SESSION_MANAGER_BASED`)
  - **Keep**: SQLite, LibArchive, jsoncpp, boost (non-interprocess parts if any)

**Step 0.3: Remove Desktop/engine/ directory**

Delete:
- `Desktop/engine/enginesync.h`
- `Desktop/engine/enginesync.cpp`
- `Desktop/engine/enginerepresentation.h`
- `Desktop/engine/enginerepresentation.cpp`
- `Desktop/engine/rscriptstore.h`

The Desktop CMakeLists.txt uses `file(GLOB_RECURSE ...)` so removing the files is
sufficient — no CMake edit needed for the Desktop target.

**Step 0.4: Stub the seams in MainWindow**

Edit `Desktop/mainwindow.h`:
- Remove `#include "engine/enginesync.h"`
- Remove `EngineSync * _engineSync` member (or replace with `void * _engineSync = nullptr` temporarily)

Edit `Desktop/mainwindow.cpp`:
- Remove `_engineSync = new EngineSync(this)` from constructor
- Remove `_engineSync->start()` call
- Remove ALL `connect(_engineSync, ...)` lines (~20 of them)
- Remove `connect(..., _engineSync, ...)` lines (filterModel, computedColumnsModel, preferences)
- Remove `_qml->rootContext()->setContextProperty("engineSync", _engineSync)`
- Remove `_engineSync->killProcessTimer()` from destructor
- Remove `_engineSync->currentStateForDebug()` from openGitHubBugReport
- Remove `delete _engineSync` from clearModulesFoldersUser
- Remove `_engineSync->allEnginesInitializing()` from enginesInitializing()
- For connections that other components depend on (e.g., `computeColumnSucceeded`),
  leave the signal in the sender but don't connect it yet. The PoC won't use filters
  or computed columns.

**Step 0.5: Stub DataSetPackage**

Edit `Desktop/data/datasetpackage.h`:
- Remove `class EngineSync;` forward declaration
- Remove `EngineSync * _engineSync` member
- Remove `setEngineSync()` method
- Remove `isThisTheSameThreadAsEngineSync()` method

Edit `Desktop/data/datasetpackage.cpp`:
- Remove `#include "engine/enginesync.h"`
- Remove `setEngineSync()` implementation
- Remove `isThisTheSameThreadAsEngineSync()` implementation
- Stub `enginesPrepareForData()` and `enginesReceiveNewData()` as no-ops
- Stub `stopEngines()` and `restartEngines()` as no-ops
- Remove the `EngineSync::singleton()` calls

**Step 0.6: Clean up Analysis/Analyses**

Edit `Desktop/analysis/analyses.h`:
- Remove `friend class EngineSync;` (L49)

Edit `Desktop/analysis/analyses.cpp`:
- Remove any `#include "engine/enginesync.h"` if present
- The comment at L1309-1313 references EngineSync — update or remove

Edit `Desktop/analysis/analysis.h`:
- The comment at L40-41 references EngineSync — update it
- Keep `shouldRun()`, `createAnalysisRequestJson()`, `setResults()` — these are reused

**Step 0.7: Verify it compiles**

Run cmake + build. Fix any remaining references. The app should launch, show the UI,
load data, but analyses will never run (no engine). That's expected.

---

## 3. What to Build

### 3.1 Rust Orchestrator (`orchestrator/`)

**Role:** NNG message broker between frontend and R runner. For the PoC, it's a dumb
pipe that forwards `work` messages and `result` messages.

```
orchestrator/
├── Cargo.toml
└── src/
    └── main.rs
```

**Cargo.toml dependencies:**
```toml
[dependencies]
nng = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

**Behavior:**
1. Create NNG PAIR socket, listen on `ipc:///tmp/jasp-orch.ipc`
2. Create second NNG PAIR socket, listen on `ipc:///tmp/jasp-runner.ipc`
3. Loop:
   - Receive JSON from frontend socket
   - Log it
   - Forward to runner socket
   - Receive JSON from runner socket
   - Log it
   - Forward to frontend socket

**No intelligence.** No routing, no validation, no queuing. Just forward.

### 3.2 R Runner (`runner/`)

**Role:** Standalone R process. Receives `work`, runs analysis, sends `result`.

```
runner/
└── runner.R
```

**R dependencies:** `nanonext`, `arrow`, `jsonlite`, `jaspBase`, `jaspDescriptives`

**Behavior:**
1. Connect NNG PAIR socket to `ipc:///tmp/jasp-runner.ipc`
2. Loop:
   - Receive JSON work message
   - Parse it
   - Load hardcoded Arrow dataset: `arrow::read_feather("test_data/poc_data.arrow")`
   - Run hardcoded analysis: `jaspDescriptives::Descriptives(dataset, options)`
   - Serialize results to JSON
   - Send result message back

**Hardcoded values:**
- Dataset path: `test_data/poc_data.arrow`
- Analysis: `jaspDescriptives::Descriptives`
- Options: `list(variables = c("contNormal", "contGamma"))` (or whatever columns exist)

### 3.3 Frontend Client (`Desktop/jaspclient/`)

**Role:** Thin NNG client replacing EngineSync/EngineRepresentation.

```
Desktop/jaspclient/
├── jaspclient.h
└── jaspclient.cpp
```

**C++ dependency:** NNG library (link via CMake `find_package(nng)` or pkg-config)

**Interface:**
```cpp
class JaspClient : public QObject {
    Q_OBJECT
public:
    JaspClient(QObject *parent = nullptr);
    ~JaspClient();

    void connectToOrchestrator(const std::string &url);  // "ipc:///tmp/jasp-orch.ipc"
    void sendWork(const Json::Value &work);
    void pollResults(int timeoutMs = 100);  // call from QTimer

signals:
    void resultReceived(const Json::Value &result);

private:
    nng_socket _socket;
    nng_dialer _dialer;
    bool _connected = false;
};
```

**Integration with Analysis:**

The key change is in how analyses get triggered and how results come back.

**Old flow:**
```
Analysis::run() → setStatus(Empty)
    → EngineSync::process() polls shouldRun()
    → EngineRepresentation::runAnalysisOnProcess()
    → IPCChannel::send()
    → Engine process
    → IPCChannel::receive()
    → EngineRepresentation::processAnalysisReply()
    → Analysis::setResults()
```

**New flow:**
```
Analysis::run() → setStatus(Empty)
    → Analyses detects shouldRun() (via QTimer or signal)
    → JaspClient::sendWork(createWorkJson())
    → NNG → Orchestrator → Runner
    → NNG → Orchestrator → JaspClient
    → JaspClient::resultReceived signal
    → Analyses routes to correct Analysis
    → Analysis::setResults()
```

**Step 3.3.1: Create JaspClient**

Implement the NNG PAIR client. Use a QTimer (100ms) to poll for incoming messages
and emit `resultReceived`.

**Step 3.3.2: Wire JaspClient into MainWindow**

In `mainwindow.cpp`:
```cpp
_jaspClient = new JaspClient(this);
_jaspClient->connectToOrchestrator("ipc:///tmp/jasp-orch.ipc");

// Replace EngineSync connections:
connect(_jaspClient, &JaspClient::resultReceived, this, &MainWindow::handleResult);
```

**Step 3.3.3: Replace the analysis trigger**

In `Analyses` or `MainWindow`, add a QTimer that polls analyses (replacing
`EngineSync::processAnalysisRequests()`):

```cpp
// In MainWindow constructor or Analyses:
QTimer *analysisPoller = new QTimer(this);
connect(analysisPoller, &QTimer::timeout, this, [this]() {
    Analyses::analyses()->applyToAll([](Analysis *a) {
        if (a->shouldRun()) {
            Json::Value work = a->createWorkJson();  // new method
            MainWindow::instance()->jaspClient()->sendWork(work);
        }
    });
});
analysisPoller->start(200);
```

**Step 3.3.4: Route results back**

```cpp
void MainWindow::handleResult(const Json::Value &result) {
    int id = result.get("id", -1).asInt();
    Analysis *a = Analyses::analyses()->get(id);
    if (!a) return;

    int revision = result.get("revision", -1).asInt();
    if (a->revision() > revision) return;  // stale

    Json::Value results  = result.get("results", Json::nullValue);
    Json::Value progress = result.get("progress", Json::nullValue);
    std::string statusStr = result.get("status", "complete").asString();

    analysisResultStatus status = analysisResultStatusFromString(statusStr);
    a->setResults(results, status, progress);
}
```

**Step 3.3.5: Add `createWorkJson()` to Analysis**

New method on `Analysis` that builds the neo-jasp wire format (replacing
`createAnalysisRequestJson()`):

```cpp
Json::Value Analysis::createWorkJson() {
    setStatus(Running);

    Json::Value work;
    work["work_id"]     = "w-" + std::to_string(id()) + "-" + std::to_string(revision());
    work["kind"]        = "analysis";
    work["revision"]    = revision();
    work["dataset_ids"] = Json::Value(Json::arrayValue);
    work["dataset_ids"].append("ds-001");  // hardcoded for PoC
    work["output_dir"]  = "/tmp/jasp-output";

    Json::Value payload;
    payload["module"]         = module();
    payload["module_version"] = moduleVersion().asString();
    payload["analysis"]       = name();
    payload["options"]        = boundValues();

    Json::Value settings;
    settings["ppi"] = 96;
    payload["settings"] = settings;

    work["payload"] = payload;
    return work;
}
```

---

## 4. Wire Format (PoC Subset)

### 4.1 `work` message (Frontend → Orchestrator → Runner)

```json
{
  "work_id": "w-1-0",
  "kind": "analysis",
  "revision": 0,
  "dataset_ids": ["ds-001"],
  "output_dir": "/tmp/jasp-output",
  "payload": {
    "module": "jaspDescriptives",
    "module_version": "0.19.0",
    "analysis": "Descriptives",
    "options": {
      "variables": ["contNormal", "contGamma"]
    },
    "settings": {
      "ppi": 96
    }
  }
}
```

### 4.2 `result` message (Runner → Orchestrator → Frontend)

```json
{
  "work_id": "w-1-0",
  "id": 1,
  "revision": 0,
  "status": "complete",
  "results": [ ... ],
  "progress": null
}
```

The `id` field is added by the orchestrator (or hardcoded by the runner) so the
frontend can route results to the correct `Analysis` instance. In the full design,
the orchestrator maps `work_id` → `analysis_id`. For the PoC, the runner echoes
back whatever `id` it needs.

**Status values:** `"running"`, `"complete"`, `"validationError"`, `"fatalError"`
(matches `analysisResultStatus` enum in `enginedefinitions.h`).

---

## 5. Implementation Phases

### Phase 1: Gut (Day 1)

Execute Section 2 (The Gut Plan) steps 0.1–0.7. **Success criterion:** jasp-desktop
compiles and launches without Engine, R-Interface, or IPCChannel. UI loads, data can
be opened, but analyses don't run.

### Phase 2: Rust Orchestrator Skeleton (Day 1-2)

1. Create `orchestrator/` with Cargo project
2. Implement NNG PAIR listener on `ipc:///tmp/jasp-orch.ipc` (frontend-facing)
3. Implement NNG PAIR listener on `ipc:///tmp/jasp-runner.ipc` (runner-facing)
4. Forward messages between them
5. Test with `nngcat` or a simple script

**Success criterion:** Send JSON into one socket, see it come out the other.

### Phase 3: R Runner Skeleton (Day 2)

1. Create `runner/runner.R`
2. Connect to orchestrator via `nanonext`
3. Receive work message, log it
4. Send back dummy result
5. Test with orchestrator running

**Success criterion:** Orchestrator logs show work in, result out.

### Phase 4: Hardcoded Analysis (Day 2-3)

1. Prepare test dataset: create `test_data/poc_data.arrow` with known columns
   - Either convert an existing CSV from `test_data/` using `arrow::write_feather()`
   - Or use `smoke_test_data_R.R` from `refactor_design/poc/` as a starting point
2. R runner: load Arrow dataset, run `jaspDescriptives::Descriptives()`
3. R runner: serialize results JSON, send back
4. Verify results JSON has the structure the frontend expects

**Success criterion:** Runner produces valid Descriptives results from Arrow data.

### Phase 5: Frontend Integration (Day 3-4)

1. Create `Desktop/jaspclient/jaspclient.h/cpp`
2. Add NNG to Desktop CMake (find_package or pkg-config)
3. Wire JaspClient into MainWindow (replace EngineSync connections)
4. Add `createWorkJson()` to Analysis
5. Add analysis poller (QTimer)
6. Add result routing (`handleResult`)
7. Update `Desktop/CMakeLists.txt` to include jaspclient sources and link NNG

**Success criterion:** Frontend sends work when analysis is created, receives results.

### Phase 6: End-to-End Test (Day 4-5)

1. Start orchestrator: `cargo run` in `orchestrator/`
2. Start R runner: `Rscript runner/runner.R`
3. Start JASP desktop
4. Open test dataset (or hardcode it to auto-load)
5. Click Descriptives in ribbon, select variables
6. Verify results appear in the results panel

**Success criterion:** Full pipeline works. Results render in the UI.

---

## 6. Hardcoded Values for PoC

| What | Value |
|------|-------|
| NNG endpoint (frontend↔orchestrator) | `ipc:///tmp/jasp-orch.ipc` |
| NNG endpoint (orchestrator↔runner) | `ipc:///tmp/jasp-runner.ipc` |
| Dataset path | `test_data/poc_data.arrow` |
| Dataset ID | `"ds-001"` |
| Module | `jaspDescriptives` |
| Analysis | `Descriptives` |
| Output dir | `/tmp/jasp-output` |
| PPI | 96 |
| Result font | default |

---

## 7. File Structure After PoC

```
jasp-desktop/
├── orchestrator/              # NEW: Rust orchestrator
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
├── runner/                    # NEW: R runner
│   └── runner.R
├── Desktop/
│   ├── jaspclient/            # NEW: NNG client
│   │   ├── jaspclient.h
│   │   └── jaspclient.cpp
│   ├── analysis/
│   │   ├── analysis.h/cpp     # MODIFIED: +createWorkJson(), updated comments
│   │   └── analyses.h/cpp     # MODIFIED: -friend EngineSync
│   ├── data/
│   │   └── datasetpackage.*   # MODIFIED: -EngineSync references
│   ├── mainwindow.h/cpp       # MODIFIED: -EngineSync, +JaspClient
│   ├── engine/                # DELETED
│   └── ...
├── CommonData/
│   ├── ipcchannel.*           # DELETED
│   ├── rbridge.*              # DELETED
│   ├── databaseinterface.*    # KEPT
│   ├── column.*, dataset.*    # KEPT
│   └── CMakeLists.txt         # MODIFIED: -R-Interface, -boost IPC
├── Engine/
│   ├── engine.*, main.cpp     # DELETED
│   └── jaspBase/              # KEPT (R package, used by runner)
├── R-Interface/               # DELETED (or excluded from build)
├── CMakeLists.txt             # MODIFIED: -Engine, -R-Interface subdirs
└── test_data/
    └── poc_data.arrow         # NEW: test dataset
```

---

## 8. Dependencies

| Component | Dependencies |
|-----------|-------------|
| Rust orchestrator | `nng` crate, `serde`, `serde_json` |
| R runner | `nanonext`, `arrow`, `jsonlite`, `jaspBase`, `jaspDescriptives` |
| C++ jaspClient | NNG C library (`libnng-dev` or build from source) |
| Desktop (existing) | Qt 6, jsoncpp, boost (non-IPC parts), SQLite, LibArchive |

**Installing NNG for C++:**
```bash
# Ubuntu/Debian:
sudo apt install libnng-dev

# Or build from source:
git clone https://github.com/nanomsg/nng.git
cd nng && mkdir build && cd build
cmake -DBUILD_SHARED_LIBS=ON .. && make && sudo make install
```

**Installing R packages:**
```r
install.packages(c("nanonext", "arrow", "jsonlite"))
# jaspBase and jaspDescriptives: install from JASP's R library or source
```

---

## 9. Known Limitations (PoC)

- **One hardcoded dataset** — no `dataset_open`, `data_edit`, `dataset_ready`
- **One hardcoded analysis** — no dynamic module loading or discovery
- **No filters, no computed columns** — the EngineSync connections for these are stubbed
- **No error handling** — basic logging only; if the runner crashes, nothing recovers
- **No runner pool** — single runner, single analysis at a time
- **No heartbeat/liveness** — if a component dies, the others hang
- **No result streaming** — runner sends complete results only (no `running` status)
- **No abort** — once work is sent, it can't be cancelled
- **No settings sync** — PPI/font/etc are hardcoded
- **No .jasp file save/load** — analyses aren't persisted
- **No R syntax mode** — the `sendRScript` path is stubbed
- **No module installation** — dynamic modules are loaded at R startup

---

## 10. Success Criteria

1. ✅ jasp-desktop compiles without Engine, R-Interface, IPCChannel
2. ✅ Orchestrator receives and forwards JSON messages between two NNG sockets
3. ✅ R runner receives work, loads Arrow data, runs Descriptives, sends results
4. ✅ Frontend sends `work` when user creates a Descriptives analysis
5. ✅ Frontend receives `result` and renders it in the results panel
6. ✅ Full pipeline: click in UI → results appear, no R embedded in C++

---

## 11. Risks and Mitigations

| Risk | Mitigation |
|------|-----------|
| NNG C++ bindings are immature | Use the C API directly (`nng.h`), it's stable. Wrap in a thin C++ class. |
| jaspBase/jaspDescriptives don't work standalone | Test with `Rscript -e "jaspDescriptives::Descriptives(...)"` first. May need to set `.libPaths()`. |
| Results JSON format mismatch | Compare runner output against what `processAnalysisReply()` expects. The `results` array structure is defined by jaspBase. |
| Qt event loop vs NNG blocking | Use `nng_recv` with `NNG_FLAG_NONBLOCK` in a QTimer poll loop (100ms). Don't block the Qt thread. |
| Arrow file format mismatch | Ensure R `arrow::write_feather()` version matches what the runner reads. Use Arrow IPC format (not legacy). |
| GLOB_RECURSE picks up deleted files | After deleting engine/ files, re-run cmake to refresh the glob. |

---

## 12. Quick-Start Commands

```bash
# 1. Gut and verify build
cd jasp-desktop/build
cmake .. && make -j$(nproc) 2>&1 | tail -20

# 2. Start orchestrator
cd jasp-desktop/orchestrator
cargo run

# 3. Start R runner (separate terminal)
cd jasp-desktop
Rscript runner/runner.R

# 4. Start JASP (separate terminal)
cd jasp-desktop/build
./Desktop/JASP

# 5. Create test data (one-time)
Rscript -e "
  df <- data.frame(contNormal=rnorm(100), contGamma=rgamma(100,2,1))
  arrow::write_feather(df, 'test_data/poc_data.arrow')
"
```
