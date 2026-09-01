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
- [ ] **Cut 2 — Importers/Exporters + legacy open path**: delete the directories; the
      AsyncLoader/FileMenu legacy open arms go; non-CSV formats fail with a clear
      "not supported in NEO yet" message (no silent nothing); their test blocks go.
- [ ] **Cut 3 — DatabaseInterface**: delete; strip DataSet/Column/Filter/Workspace/
      DataSetPackage db* methods; fixture fallout in tests.
- [ ] **Cut 4 — legacy undo commands**: delete the Column-command family; UndoStack keeps
      macros + byte cap + DataEditCommand.
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
| `LIBREADSTAT_LIBRARIES` (+ rtools/apple variants) | `Desktop/CMakeLists.txt` | **Cut 2** (readstat importer: SPSS/Stata/SAS) |
| `LIBRDATA_LIBRARIES` | Desktop | **Cut 2** (rdata importer) |
| `freexl::freexl` (non-Linux) | Desktop | **Cut 2** (excel/ods importers) |
| `find_package(nng)` | Desktop | **KEEPS** — the NEO client transport (jaspclient) |
| `include(Dependencies)` (the ReadStat prep, root CMakeLists) | root | Cut 2 cleanup |
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
