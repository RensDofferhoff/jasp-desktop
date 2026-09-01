# HANDOVER — The Great Excision: removing the entire legacy data route

**Status:** 2026-09-01. Direction set by the user: **neo-jasp is a major refactor — remove
everything legacy, replace it with sanity, and crawl functionality back. We are not
shipping tomorrow.** The strategic question "what legitimately loads via legacy?" is
ANSWERED: **nothing**. Lane datasets (CSV via the orchestrator, today) are the only data
route; every other format/feature returns later as a NEO-era reimplementation.

## What dies (the scope)

| Subsystem | Files (approx.) | Why it can go |
|---|---|---|
| **DatabaseInterface** (sqlite, .jasp load/save) | `CommonData/databaseinterface.{h,cpp}` + users | .jasp persistence is a later NEO era |
| **Column** + the mirror paths | `CommonData/column.{h,cpp}`, `_columns` everywhere | the mirror is already gone for lane; Column owns nothing lane needs |
| **Importers** | `Desktop/data/importers/*` (csv, ods, jasp, excel, rdata, readstat) | lane opens CSV; other formats return as lane conversions |
| **Exporters** | `Desktop/data/exporters/*` | same |
| **DataSetSyncer** | `CommonData/datasetsyncer.{h,cpp}` | R1 becomes DELETION; sync returns as backend sync (P14 pinned in DataSetSyncer's header — preserve that comment into the handover!) |
| **Legacy undo commands** | undostack.cpp: SetColumnProperty/SetColumnType/InsertColumn/RemoveColumns/FilterLabel/MoveLabel/ReverseLabel/ColumnReverseValues/ColumnToggleAutoSortByValues/… | all Column-serializing; lane edits are DataEditCommand |
| **Filter internals** | Filter's sqlite/boolvec/LabelFilterGenerator/FilteredData/VarInfoModelProxy paths | filters return as DERIVED BOOLEAN COLUMNS (below) |
| **DataSetTableModel + proxy legacy branch** | `CommonData/datasettablemodel.*`, ExpandDataProxyModel's DataSetTableModel arm | GridModel is the only grid model |
| **Legacy tests** | testall's savLabels/syncer/import/dropLevels/… blocks | code they test is gone |

**NOT Filter-as-Column** (it isn't — Filter is DataSetBaseNode+VariableInfoProvider, no
inheritance tangle): Filter's *guts* go, the provider interface stays (forms need it).

## The NEO-era designs these return as (pinned direction)

All three are **derivations** — the rail reserved the vocabulary: `ChangeKind { edit,
derived, external }`; `derived` exists, unused, for exactly this.

1. **Filters** = a derived BOOLEAN column. Frontend keeps only the expression (constructor
   JSON / code — metadata, zero rows); the backend evaluates it; the VIEW LANE applies the
   mask at view time (skips excluded rows when serving chunks). No per-row vector ever
   exists in the frontend — the terror_tall bug class stays extinct.
2. **Computed columns** = a derivation one column wide: backend evaluates, mints a
   revision, broadcasts `data_changed (cause: derived)`. Undo composes with the inverse
   machinery, or derived columns recompute on demand.
3. **Computed datasets** = a dataset whose cache is generated from other datasets; the
   orchestrator's dataset manager mints the identity, the pipeline re-runs when a source
   broadcasts data_changed.

Common principle: **the frontend holds expressions and metadata; every row lives in a
backend cache; every change is a revision bump with a broadcast.** No new machinery —
new producers on the existing rail.

## The cuts (each compiles + lane tests green, each its own commit)

- [ ] **Cut 1 — DataSetSyncer**: delete the class; DataSet drops the member; FileMenu /
      MainWindow / DatabaseFileMenu call sites deleted (R1 gates become deletions);
      testall's 7 syncer tests go. Preserve the P14 policy comment here (it moves to §the
      main handover / this file).
- [x] **Cut 2 — Importers/Exporters + legacy open path** (2026-09-01): `Desktop/data/
      importers/` and `Desktop/data/exporters/` deleted (csvparser moved to `Desktop/utilities/` —
      CsvPreviewModel + its test keep it); `datasetloader.{h,cpp}` deleted; AsyncLoader keeps
      ONLY the lane CSV-family open — every other route (other formats, OSF nodes, database
      sources, .jasp, FileSyncData, FileSave/autosave, FileExportResults/Data/GenerateData)
      completes with `FileEvent::notSupportedInNeoMsg(...)` and MainWindow surfaces it (never a
      silent nothing); FileEvent lost the Exporter machinery; autosave is a quiet no-op (no
      modal nag); DesktopCommunicator lost the delimiter ask-API (the lane sniffs itself);
      `DataSet::writeToOStream` gone. **Bug fixed en route:** lane datasets never registered
      schema names in the ColumnEncoder (`setupEncoderPrefix` ran at dbCreate, before the
      schema) — `getColumnTypesMap()` serves the schema when lane-bound and `landWireSchema`
      refreshes the encoder, else `encode("V1")` threw "not a columnName". Deps dropped:
      ReadStat/librdata/freexl everywhere (Desktop links, Libraries finds, APPLE readstat dep,
      Windows rtools headers/DLLs, Install bundle list, Conan provisioning + conanfile) and
      `Tools/CMake/Dependencies.cmake` + root `include(Dependencies)` deleted. Test fallout:
      testall's import/export blocks died; the CSVImporter *fixture* users were reworked onto
      `applySchema` lane fixtures (cycle/close/encoder tests); **JASPTestDebugData deleted
      whole** (its fixture loader was CSVImporter; every test drove legacy Column/Label APIs —
      Cut-5 death row anyway); testUndoColumnDropLevels died early (same reason). Known
      pre-existing (NOT Cut 2): JASPTestDbMigration crashes in `DatabaseInterface::load()` —
      an in-memory `:memory:` load can never pass `filesystem::exists` (Cut-3 territory).
- [x] **Cut 3 — DatabaseInterface** (2026-09-01): `databaseinterface.{h,cpp}` + the sql
      fixture files deleted; DataSet/Column/Filter/Workspace/DataSetPackage/DataSetProvider
      db* methods stripped. **ID minting** is now process-global `std::atomic<int>` counters
      (`g_nextDataSetId/g_nextColumnId/g_nextFilterId` in their own TUs) replacing db row ids;
      `dbUpdate()` sites became `incRevision()`; `checkForUpdates()` is `return false`;
      `dbDelete()` methods survive as purely in-memory teardowns; `DataSet::name()` generates
      `"Dataset " + id`. **LibArchive has a live user beyond CommonData**:
      `QMLComponents/utilities/extractarchive.cpp` (module .zip install via DynamicModules,
      autosave metadata) — link moved CommonData→QMLComponents, `find_package(LibArchive)`
      stays. SQLite::SQLite3 fully dropped (CommonData linked it twice). Filter::
      `filterNameIsFree` arg-order mismatch (header vs cpp) found by compiler and unified to
      `(filterName, dataSet)`. SyntaxInterface is excluded from the build, so
      syntaxbridge.cpp's DatabaseInterface/ArchiveReader references don't block (GUT_TODO
      territory). Test fallout: `testFilterRevisionInvalidatedRoundTrip` (pinned a sqlite
      round-trip) and **JASPTestDbMigration deleted whole** — its `:memory:` load could
      never pass `filesystem::exists` (the pre-existing crash died with it). 14 JASPTest /
      5 ColumnEncoder / 8 CsvPrev / 63 QuickTest all green; from-scratch configure verified.
      **Bug fixed at the wire**: `DataSet::removeFilter` double-deleted — the Cut-3 edit had
      replaced the old `f->dbDelete()` (db-row removal only, object survives) with `delete f`
      while keeping the trailing `delete f`; SIGSEGV in `testFilterRemoveFilter`
      (instruction at `dataset.cpp`'s second delete, deref of garbage 0xc41b). Filter has no
      dtor and nothing but the newList loop removes from `_filters`, so the early delete was
      simply dropped.
- [x] **Cut 4 — legacy undo commands** (2026-09-01): undostack.{h,cpp} rewritten — the
      entire Column-serializing command family deleted (SetColumnProperty/SetColumnType/
      SetData/Insert·Remove Rows·Columns/PasteSpreadsheet/CopyColumns/the label CRUD family
      (Add·Set·Delete·Move·Reverse·FilterLabel)/Set·Custom·UseCustomEmptyValues/
      SetWorkspaceProperty·EmptyValues/SetJson·RFilter/CreateComputedColumn·SetComputedColumnCode
      (zero push sites, already orphans)/ChangeSelectionCommand (zero push sites)/the
      UndoModelCommandMultiple·SingleColumn bases). **UndoStack keeps macros + the ~250 MB
      byte cap + the UndoModelCommand base** (DataEditCommand derives from it and the cap
      sums its undoBytes()) — the base is slimmed to ctor/dataSet()/dataSetStillExists()
      (columnName/rowName/column(index) helpers died with their users). Push sites stripped:
      columnmodel (lane paths already early-returned; legacy fall-throughs deleted, pure-legacy
      label-editor fns are logged no-ops — labels return in B2), filtermodel (editor inert;
      filters return as derived boolean columns), workspacemodel (description has no wire
      support — returns as jasp:description; empty values are a legacy loading concept),
      expanddataproxymodel (all legacy arms inert: insert/remove/resize/copyColumns/
      columnReverseValues/columnautoSortByValues no-op — the lane rail has no structural
      delete op yet, growth is remote; lane insert_block/schema_change paths untouched;
      columnIndexesToNames orphaned and deleted). datasetview.cpp:201 stale comment removed.
      QML-facing signatures all kept (Cut 7 sweeps the callers). Tests: 14 JASPTest /
      5 ColumnEncoder / 8 CsvPrev / 63 QuickTest green; orphan grep clean.
- [x] **Cut 5 — Column + mirror paths + DataSetTableModel** (2026-09-01): `column.{h,cpp}`
      (3.4k lines), `label.{h,cpp}`, `datasettablemodel.{h,cpp}`, `labelfiltergenerator.{h,cpp}`
      (pulled forward from Cut 6 — its Label/Column deps died here) + stale checked-in
      `internalDbDefinition.h`/`createIndexes.h` all DELETED. **DataSet**: the wire schema is
      THE implementation — `_columns`/`_shownColumn`/`_changedDuringBatch` gone; every
      Column* accessor + the legacy write API (insert/remove/create/computed/paste/·
      reverse/autoSort/columnsApply/reorder/resetVariableTypes/setColumnTypes/insertRows·
      Columns·removeRows·Columns as real ops...) deleted; model write overrides are inert
      (`setData`→false etc., vtable-honest); `getColumnIndex` serves the schema; model-API
      (data/headerData/flags/columnCount) schema-only; inert-but-kept:
      `columnsLabelFilteredCount()`→0 (QML binds it), `resetAllFilters` signals-only.
      **Signals died**: labelChanged(Column*)/shownColumnChanged/labelFilterChanged/
      columnTypeChanged kept (schema type changes re-emit it). **ID minting note**: the
      default filter's per-row mask is LOAD-BEARING metadata — `setRowCountMetadata` now
      resizes it all-true (boolvec value-initializes false!) AND emits a model reset, or the
      Filter/FilteredData/VarInfoModelProxy chain serves unknown types + 0 rows (the
      min/max-levels QML test caught this). **Workspace**: shownColumn/createComputedColumn/
      initializeComputedColumns/computedColumnSucceeded/updateComputedColumnDependenciesFor
      Analysis/checkForDependentAnalyses(Column*) chain (Column → Workspace → DataSetPackage
      → Analyses) deleted. **Analysis/Analyses**: computed-column machinery inert (handlers
      log + no-op, isColumnFreeOrMine→true, isOwnComputedColumn→false, createdVariables→{},
      asJSON columns list empty); checkForDependentAnalyses deleted. **MainWindow**:
      _datasetTableModel + its 4 connects gone; ColumnsModel ctor takes no arg; addNewDataSet/
      generateEmptyData play a 1×1 scale lane fixture via applySchema. **ColumnsModel**:
      schema-only (no table fallback). **ExpandDataProxyModel**: legacy arm GONE —
      dataSetSourceModel/rawRunsFromShown/serializedColumn deleted, shownToRaw = identity,
      insert/remove/resize/copyColumns inert (lane rail has no structural delete op yet;
      growth is remote). **ColumnModel**: adapter = only path — column() accessor,
      17 legacy label-editor fns + the computed-column editor deleted (QML guarded), lane
      schema paths untouched. **DataSetProvider**: plays a lane fixture — loadDataSet infers
      wire types/levels/distinct counts from the string data and applySchema's them;
      provideInfo serves from the schema (no row values). **Filter::provideInfo** answers
      VariableType/TotalLevels/TotalNumericValues/Labels/ColumnDescription from the SCHEMA
      directly for lane-bound datasets (the legacy FilteredData path returned 0 for scale
      levels — broke the levels checks); this is the minimal preview of Cut 6's cutover.
      **VariableInfo**: labelChanged(const Column*) removed. **QML guarded** (Cut 7 sweeps):
      VariablesWindow (ComputeColumnWindow instantiation removed, label editor disabled,
      missing-values panel hidden), ColumnBasicInfo (show-parent-analysis hidden),
      LabelEditorWindow derefs neutralized. **DatasetProvider note**: JASPQuickTest's 4-column
      fixture keeps working through the schema (TestLetters=5 levels, TestDoubles=5 numerics).
      Tests green: JASPTest 14 / QuickTest 63 / ColumnEncoder 5 / CsvPrev 8.
- [x] **Cut 6 — Filter internals + provider cutover** (2026-09-02): `filtereddata.{h,cpp}` and
      `varinfomodelproxy.{h,cpp}` DELETED; Filter is no longer a VariableInfoProvider and carries NO
      per-row mask (`_filtered`/setFilterVector/setFilterValueNoDB/setRowCount/calculateFilteredRowCount/
      filtered()/filteredRowCount/checkForUpdates/checkFilterResults/rowFiltered*/varInfo/provideInfo/
      absorbInfo/providerModel/model-API overrides + the varInfo/filteredRowCount Q_PROPERTYs all gone) —
      Filter keeps ONLY expression metadata (name/rFilter/generatedFilter/constructorJson/constructorR/
      errorMsg/statusBarText/invalidated) + the datasetChanged rename-rewrite maintenance + the
      setInvalidated→sendFilterByName engine trigger. **Provider cutover**: Workspace gained an injected
      `formProvider()` slot (VariableInfoProvider*) — QMLComponents cannot see Desktop's ColumnsModel
      (link DAG: QMLComponents→CommonData, JASPDesktopLib→QMLComponents), so **ColumnsModel self-registers**
      into the Workspace in its ctor (desktop app) and **DataSetProvider self-registers** in its ctor +
      resetDataSet (engine/test worlds — JASPQuickTest has no MainWindow/ColumnsModel; DataSetProvider is
      already a schema-correct provider). `AnalysisForm::setAnalysisUp` points at Workspace::formProvider
      unconditionally (the analysis'-filter/shown-Filter fallbacks + the ctor's filterChanged→setProvider
      connect died); `RSyntaxHighlighter` falls back to `Workspace::varInfo()`; JAGSTextArea.qml binds
      `form.varInfo`; FormulaParser callers (formulabase/formulasource) pass `form()->varInfo()->provider()`.
      **ColumnsModel gained the missing provider-contract wiring**: its signals + per-dataset connects
      (bindLane: schemaChanged/datasetChanged/columnTypeChanged/modelReset/dataChanged/emptyValuesChanged/
      labelsReordered, all receiver=this so rebinds disconnect cleanly) now feed the VarInfoSignaller —
      "schemaChanged → provider refresh" EXISTS; the relay-VariableInfo in its ctor (provider-less,
      unconsumed) is gone; it also overrides `columnEncoder()` to serve the bound dataset's encoder.
      **DataSet**: setRowCountMetadata no longer resizes the default filter's mask (the reset announce
      stays); headerData's filter role returns true (v1: no filter compaction); getRowFilter deleted.
      FilterModel::processFilterResult is a logged no-op (orphan slot — no connect sites since Cut 1);
      ListModelFilteredDataEntry is mask-free (acceptedRows all-true; data-entry tables land with
      data_view). Tests: testFilterSetFilterVectorResizesToResult died with the mask (14→13);
      **13 JASPTest / 63 QuickTest (the provider gate — forms serve through DataSetProvider) / 5 / 8 green**.
      Note: workspace.h now includes variableinfo.h directly (filter.h used to drag it in transitively).
- [ ] **Cut 7 — sweep**: grep `_columns|column\(|Column \*|DatabaseInterface|DataSetSyncer`
      for survivors; remove now-dead QML (filter panels, label editor windows) or gate
      them; final handover consolidation.

## Validation per cut

Build `JASPDesktopLib` + `JASPTest` + `JASP`; the LANE test set stays green and GROWS a
cut-specific canary where sensible; the Rust suite is untouched (147). Legacy tests die
WITH their code (never leave tests for deleted subsystems failing).

## Dependencies to drop (as their last consumer dies — keep the build honest)

Grounded in the CMake files (2026-09-01):

| Dependency | Current consumer | Dies with |
|---|---|---|
| `SQLite::SQLite3` | `CommonData/CMakeLists.txt` (CommonData links it twice) | **Cut 3** (DatabaseInterface) |
| `LibArchive::LibArchive` | CommonData | .jasp zip packaging — Cut 3 (verify no other user; archivereader lives in CommonData too!) |
| `LIBREADSTAT_LIBRARIES` (+ rtools/apple variants) | `Desktop/CMakeLists.txt` | **DONE — Cut 2** (readstat importer: SPSS/Stata/SAS) |
| `LIBRDATA_LIBRARIES` | Desktop | **DONE — Cut 2** (rdata importer) |
| `freexl::freexl` (non-Linux) | Desktop | **DONE — Cut 2** (excel/ods importers; conan provisioning + conanfile requirement dropped too) |
| `find_package(nng)` | Desktop | **KEEPS** — the NEO client transport (jaspclient) |
| `include(Dependencies)` (the ReadStat prep, root CMakeLists) | root | **DONE — Cut 2** (file deleted) |
| libsodium | Desktop | check: orchestrator session auth? verify before touching |

Rules: every cut ends with a grep for the removed lib's targets/includes — no orphan
`find_package`/`include`/link lines left behind; and the build must still configure from
scratch (a stale cache can hide a missing dependency).

## P14 (re-pinned from the DataSetSyncer header, which Cut 1 deletes)

Backend sync (future era) is orchestrator-owned: watch source → reconvert via the
data_open machinery → revision bump + `data_changed` (cause `external`). An external
change is a FRESH RELOAD — nothing survives: no rebase of local edits, the dataset's undo
stack is cleared (every stored inverse blob is then base_revision < current — not
stale-refused, but meaningless against reloaded data). Candidate later exception ONLY:
the labels overlay — value-keyed (P11), reattaches to surviving values, cannot conflict.
