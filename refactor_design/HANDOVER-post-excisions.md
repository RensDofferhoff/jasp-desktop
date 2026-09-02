# HANDOVER — Post-Excisions: the world after Cut 1–7, the dependency diet, and the first smoke tests

**Status:** 2026-09-02. The Great Excision is **complete** (Cuts 1–7, see `HANDOVER-excision.md`
for the cut-by-cut log — this document is the *current state*, not the history). The dependency
diet landed (boost, brotli, gmp, mpfr, sqlite3 gone). First user smoke tests shook out two
ancient icon-URL bugs (both fixed). This is the map for anyone picking the codebase up cold.

**The religion, in one sentence:** the backend owns every value; the frontend owns only the
*description* of the data (the wire schema) plus windows that look at it — one schema, one
signal (`schemaChanged`), values only in the warehouse.

## Commits (chronological, all on `development`)

| Commit | What |
|---|---|
| `4076fc37a` | Cut 1 — DataSetSyncer deleted |
| `f1020f297` | Cut 2 — importers/exporters + legacy open path (−8.5k) |
| `f37f35606` | Cut 3 — DatabaseInterface deleted (sqlite route gone) |
| `636a90549` | Cut 4 — legacy undo command family deleted |
| `7a5e428ce` | Cut 5 — Column + Label + mirror + DataSetTableModel deleted |
| `f004212f7` | Cut 6 — Filter internals + provider cutover |
| `333c824e9` | Cut 7 — the sweep (dead QML, ColumnModel/GridModel orphans, filter UI gated) |
| `d7e275337` | Diet 1 — boost (+sqlite3/gmp/mpfr from conanfile) gone |
| `bdca1a2e7` | Diet 2 — brotli gone |
| `3e7816fb1` | Icon fix 1 — ColumnTypesModel::menuImageSource → full theme URL |
| `e1f0bc3ec` | Icon fix 2 — ColumnTypesModel::iconList → full theme URL |

## The two planes

- **Metadata plane** (frontend): the wire *schema* — column names, types, level dictionaries,
  distinct counts, row count. Landed from the backend on open and on every revision bump.
  Stored in exactly one place per dataset: `DataSet`. Everything (grid headers, forms,
  variables window, filters-as-metadata) reads from it.
- **Value plane** (backend): every cell byte lives in the Rust lane's Arrow cache. The grid
  fetches *chunks* via `data_view` at view time. The frontend never stores rows and never
  re-parses files.

**The edit loop:** QML gesture → `DataEditCommand` (undoable op + stored inverse, revision-
keyed) → JaspClient/nng → orchestrator (per-dataset FIFO) → new revision → `data_changed`
push → `DataSet::applyRevision → landWireSchema` → `schemaChanged` → providers reset + fire
their signallers → every form/list/grid re-queries the schema. That loop IS the app.

## The cast (major classes, by job)

### Backend (outside the app)
- **Orchestrator / data lane (Rust)** — the warehouse: Arrow cache, revisions, edit FIFO.
  The single source of truth. Rust suite lives there (~148 tests, untouched by all cuts).
- **R runner** — engine-side analyses + filter expression evaluation.

### CommonData core (no QML)
- **`DataSet`** — *the schema made into an object.* Wire schema lands here
  (`applySchema`/`applyRevision`); it is a table of contents, not the data. Emits
  `schemaChanged` — the heartbeat. Model API (`data`/`headerData`) serves metadata roles only;
  value roles serve honest empties.
- **`Workspace`** — the session registry: owns the DataSets (tabs), tracks `shownDataSet`,
  holds the **`formProvider()` slot** and its own `varInfo` (exposed to QML as `dataSetInfo`).
  Routes; never serves.
- **`DataSetPackage` / `AsyncLoader`** — the door: lane CSV-family opens only; every other
  route completes with `FileEvent::notSupportedInNeoMsg(...)`. Autosave is a quiet no-op.
- **`UndoStack` + `DataEditCommand`** — the memory of gestures: every edit is an op sent to
  the backend plus a locally stored inverse. Undo works *because* we store "how to un-do",
  not data. UndoStack keeps macros + the ~250 MB byte cap.
- **`Filter`** — an expression in a box: rFilter / constructorJson / constructorR / errorMsg /
  invalidated. NO per-row mask, NO provider role. Renames/retypes are rewritten into the
  expression by `datasetChanged` maintenance; `setInvalidated` still triggers
  `sendFilterByName` so the engine re-evaluates (metadata flows; results land nowhere until
  the derived-columns era).
- **`VariableInfoProvider` / `VariableInfo` / `VarInfoSignaller`** (`variableinfo.h`) — the
  provider contract: `provideInfo(question, name)` + a signaller that yells when answers
  change. Consumers (`VariableInfoConsumer`) just call `requestInfo`.

### Desktop models (exposed to QML as context properties)
- **`GridModel`** (`dataSetModel`) — the spreadsheet's face: shape/headers from the schema,
  cell values fetched as lane chunks. Writes come back through the proxy.
- **`ExpandDataProxyModel`** — the grid's write path: turns QML cell edits into
  `DataEditCommand`s (insert_block etc.). `shownToRaw` is the identity now.
- **`ColumnModel`** (`columnModel`) — the variables-window editor adapter: "the chosen
  column's" name/type from the schema; rename/retype submit `schema_change` ops. Post-Cut-7
  it serves only the live "Column definition" tab.
- **`ColumnsModel`** (`columnsModel`) — the columns-and-types list for form source lists —
  and **THE form provider** (see below). `bindLane(shownDataSet)` + `schemaChanged` → reset +
  signaller fire = "schemaChanged → provider refresh".
- **`FilterModel`** (`filterModel`) — thin wrapper over Filter metadata; its UI is gated
  unreachable (see "Deliberately kept").

### QMLComponents (shared form library — also used by test/engine worlds!)
- **`AnalysisForm`** — each analysis UI; owns a `VariableInfo`; `setAnalysisUp()` wires it to
  the Workspace's form provider. Its ListModels (variables lists etc.) are
  `VariableInfoConsumer`s.
- **`DataSetProvider`** — the schema-correct stand-in provider for worlds without a
  MainWindow (JASPQuickTest, engine). Also plays lane fixtures for tests.
- **`RSyntaxHighlighter`** — falls back to `Workspace::varInfo()` when handed none.

## The form provider (the one sneaky pattern)

Problem: `AnalysisForm` (QMLComponents) cannot even name `ColumnsModel` (Desktop) — the link
DAG is `Desktop → QMLComponents → CommonData`. And test/engine worlds have no ColumnsModel.

Solution: `Workspace` (CommonData) keeps one pointer-shaped slot, `formProvider()`.
- **ColumnsModel registers itself in its ctor** (`workspace->setFormProvider(this)`) — desktop app.
- **DataSetProvider registers itself in its ctor** — QuickTest/engine worlds.
- **`AnalysisForm::setAnalysisUp`** asks the slot: `varInfo()->setProvider(formProvider())`.
- Refresh rides the provider's **VarInfoSignaller**: schema change → ColumnsModel resets →
  signaller → the form's VariableInfo relays → all ListModels re-ask.

One slot, two worlds, zero Filter guts. This structurally killed the "no variables / no
types / form disagrees with grid" bug family.

## Where QML calls in (context properties)

| QML name | C++ | Serves |
|---|---|---|
| `dataSetModel` | GridModel | the grid (values via lane chunks) |
| `columnModel` | ColumnModel | variables-window editor |
| `columnsModel` | ColumnsModel | columns/types lists + is the form provider |
| `workspace` | Workspace | tabs, shownDataSet, varInfo |
| `dataSetInfo` | Workspace's varInfo | provider-backed info (set in `qmlutils.cpp`) |
| `filterModel` | FilterModel | filter UI (gated) |
| `columnTypesModel` | ColumnTypesModel | type menus; roles return **full theme URLs** (fixed 3e7816fb1/e1f0bc3ec) |
| `workspaceModel`, `analysesModel`, `preferencesModel`, `mainWindow`, … | themselves | non-data chrome |

## Deliberately kept (do not "clean" these without a plan)

- **The excision comments** (`// The excision, Cut N: …`) — breadcrumbs tying code to the
  handover; sweep them in a later consolidation era. **User decision 2026-09-02: they stay.**
- **Filter GUI files** (`FilterWindow.qml`, `FilterConstructor/`) — instantiated but
  unreachable (toggle + add-computed-column buttons `visible: false`;
  `columnUsedInEasyFilter → false` seals the header path). They return, reworked, when
  filters return as **derived boolean columns**.
- **`Filter` metadata + `sendFilterByName` chain** — expressions still flow to the engine.
- **Workspace-level empty values** — a legacy loading concept; redesign pending.
- **Analysis-filter dropdown** (AnalysisFormExpander) — per-analysis filterId metadata.
- **`syntaxbridge.cpp`** — references DatabaseInterface but SyntaxInterface is excluded from
  the build (`# add_subdirectory(SyntaxInterface)`); GUT_TODO territory.
- **Deps kept on purpose:** libarchive (ExtractArchive: module .zip installs + autosave
  metadata), libsodium (secret store/jaspencrypt), nng (the NEO transport), and
  zlib/zstd/openssl/libiconv in conan — dead on Linux but plausibly load-bearing for
  Windows/macOS packaging (no system iconv there; Qt HTTPS needs OpenSSL DLLs shipped;
  possibly-static conan libarchive graph). **Do not trim without a Win/mac check.**
- **R at configure time** — `include(R)` still locates R and boots renv/Rcpp at configure;
  R includes in Common/CommonData are dead for the desktop (BUILDING_JASP guards Rcpp away)
  but untouched, pending the "make R_HOME conditional on INSTALL_R_MODULES" job.

## The NEO-era return designs (the roadmap — from HANDOVER-excision.md)

All three are **derivations**; the rail reserved `ChangeKind { edit, derived, external }`.
1. **Filters** = a derived BOOLEAN column. Frontend keeps the expression; backend evaluates;
   the VIEW LANE applies the mask when serving chunks. No per-row frontend vector, ever.
2. **Computed columns** = a derivation one column wide; backend mints a revision,
   broadcasts `data_changed (cause: derived)`.
3. **Computed datasets** = a dataset whose cache is generated from others.
Common principle: frontend holds expressions and metadata; every row lives in a backend
cache; every change is a revision bump with a broadcast.

## Pin board (next steps, in rough priority)

1. **R-at-configure** — gate the R bootstrap on `INSTALL_R_MODULES`; make `R_HOME` unneeded
   for plain desktop builds. The real quality-of-life win for Linux configure+build.
2. **User smoke testing continues** — two ancient icon bugs already fell
   (`menuImageSource`, `iconList`); expect more dust. Cut 6/7 UI changes to verify: no filter
   button, no add-computed-column button, variables window = one "Column definition" tab.
3. **Windows/macOS dependency pass** — only with CI/a box: the kept-deps list above.
4. **Excision-comment sweep** — someday, when the breadcrumbs stop being load-bearing.
5. **Feature crawl-back** — derived boolean columns (filters) first, then the B2 labels
   overlay (`jasp:labels`), data-entry tables with `data_view`, computed columns/datasets.
6. **Analysis views** — **designed 2026-09-02, not built** (`analysis-views-design.md`):
   materialized per-dataset projections (casts+relabels in the worker, `name__type` fields,
   two-step pins, fetch refs stapled to work). Depends on filters-as-derived-columns and
   lane coercion parity; the module audit (§12 there) already ran.

## Environment & ritual (this machine)

- Real build dir: **`build/Desktop_Qt_6_11_0-Debug`** (Qt Creator shadow build; AGENTS.md's
  `build/` is stale and user-modified — left alone).
- Qt at `/home/sp42/Qt/6.11.0/gcc_64`; R via env `R_HOME=/home/sp42/customR/R-4.5.2`;
  builds need `CCACHE_DIR=/tmp/ccache`; tests run with `QT_QPA_PLATFORM=offscreen`.
- CMake GLOBs sources/QML → **re-run `cmake .` after any file deletion**.
- Git writes (commit/add) need **unsandboxed** terminal (`.git` is read-only in sandbox).
- **Validation ritual:** build all targets, then
  `JASPTest` (13) · `JASPQuickTest` (63 — the provider gate) · `JASPTestColumnEncoderContext`
  (5) · `JASPTestCsvPrev` (8). All green = done. Rust suite untouched (≈148, lives in the
  orchestrator repo).
- Known noise: `QObject::connect(PreferencesModel, …) invalid nullptr parameter` warnings in
  tests (no PreferencesModel there) and a first-`setShownDataSet` disconnect warning — both
  pre-existing and benign.
