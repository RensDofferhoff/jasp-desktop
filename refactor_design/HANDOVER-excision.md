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
- [ ] **Cut 5 — Column + mirror paths + DataSetTableModel**: delete Column; DataSet's
      model-API legacy branches become THE implementation; getColumnIndex/column serve
      schema (or die if nothing needs them); ExpandDataProxyModel loses its legacy arm
      (GridModel only); ColumnModel's legacy branches die (adapter = the only path);
      label editor guts go (B2 rebuilds it).
- [ ] **Cut 6 — Filter internals + provider cutover**: Filter's guts (sqlite, boolvec,
      LabelFilterGenerator, FilteredData, VarInfoModelProxy) go; the VariableInfoProvider
      for forms becomes ColumnsModel (already schema-correct) — `AnalysisForm::
      setAnalysisUp` and `Workspace` point at it unconditionally; wire schemaChanged →
      provider refresh so forms update after edits. This CLOSES the "no variables /
      no types" bug class permanently (the schema is the one home).
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
