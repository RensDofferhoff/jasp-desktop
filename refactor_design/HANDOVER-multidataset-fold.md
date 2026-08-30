# HANDOVER — Multi-dataset merge + fold: committed; GUI verification pending; data-edit design is next

**Date:** 2026-08-28. **Status:** three commits landed (merge → fold → rename). Builds green,
quick tests + both harnesses green. **GUI NOT YET VERIFIED** — the smoke checklist in §5 is the
gate for everything after. Next session: **design the data edit era** (§6 is your brief).
**Read first:** `merge-multidataset.md` (the merge policy + verification log §7 — decisions
frozen there are not to be relitigated), `HANDOVER-dataview-inc3.md` (the view machinery this
all rides on), `data-view-format.md` §2.5.

## 0. What landed (three commits, in order)

| Commit | What |
|---|---|
| `90650401f` | **Merge** origin/development = multi-dataset PR #5783 (Joris Goosen, squashed as `85024a2c0`, 310 files). Constructs kept (Workspace, tabs, per-dataset encoder/undo, computed datasets); SQLite lane kept *compiling* (strip is deferred — see §7); engine lane deletions kept (rbridge/databridge/enginesync dead); their QML needed GridModel surface additions (§3). |
| `ec0644f63` | **Fold** — one dataset truth. `DatasetRegistry` + `DataModel` deleted. `DataSet` carries identity + wire schema; `Workspace::shownDataSetChanged` is THE activation signal; GridModel owns the view lane. |
| `bfd42a515` | **Rename** — the "lane" prefix is gone from identifiers (`applySchema`, `schema()`, `isOpen()`, `startView`/`dropView`, …). |

Push (not yet done at time of writing): `git push rens development:neo-wip`

## 1. Who owns what (the answer to "what happens where")

```
DataSetPackage  = the DOCUMENT   — one .jasp session: currentFile, isModified, save/load,
                                    analysesHTML/Data, the NEO open entry (neoOpenDataset).
                                    A leftover name for what is really "the session shell".
Workspace       = the dataset LIST — which datasets exist (tabs), which is shown.
                                    shownDataSetChanged(DataSet*) is the switch signal.
DataSet         = ONE dataset     — datasetId (orchestrator id, ONE id space), schema()
                                    + schemaColumns* (the wire schema), isOpen(), and the
                                    constructs: labels, filters, computed columns,
                                    per-dataset UndoStack + ColumnEncoder. NO CELLS, EVER.
GridModel       = the view lane   — owns DataViewBuffer + ViewFiller for the SHOWN dataset
                                    only; drops them on switch (memory ceiling = 1× budget).
                                    Registered to QML as `dataSetModel`.
Orchestrator    = the DATA        — Arrow caches, windowed reads, analyses. The only process
                                    that ever touches a full dataset.
```

The two flows:

- **Open:** `DataSetPackage::neoOpenDataset` → JaspClient `data_open` → orchestrator converts
  to Arrow, mints `ds-N` → terminal result → `shownDataSet()->applySchema(id, rows, schema)`
  → metadata mirrored (§2) → `schemaChanged` → GridModel `startView` → filler pulls windows.
- **Switch tab:** `shownDataSetChanged` → GridModel `dropView` + `startView`; ColumnsModel and
  the forms' providers re-point at the same dataset. That's the whole switch.

Viewport: `DataSetViewBase::viewportRowsChanged` → MainWindow lambda → `_gridModel->setViewportRows`.

## 2. The metadata mirror (the one deliberate double representation)

Their UI reads column info through `form.varInfo → provider = shown dataset's Filter →
Filter::provideInfo → legacy Column objects`. Under NEO the open flow populates no Columns —
so `DataSet::applySchema` ALSO creates legacy Columns (name + type + rowCount, **never cells**;
labels/levels deliberately NOT mirrored — value-indexed store, would corrupt the maps; label
editor inert until the edit era). Two readers, one writer, one source (§ diagram in the
merge-policy doc). The edit era must collapse this: either Column becomes the metadata holder
or the providers learn `schema()`. **Filters/computed columns will force that choice** — it is
a first-order design question for the next session.

## 3. Post-merge API notes (things that bit us once)

- **QML calls methods directly on `dataSetModel` (= GridModel).** Their new QML renamed
  `getColumnTypesWithIcons()` → `columnTypesWithIcons()` (we alias both). GridModel also gained
  `columnFilter` (stored, logs that filtering is edit-era), `currentTypeIcons` (empty until
  selection wiring), `toggleColType` (logged no-op — fail loudly). Any new QML the merge brings
  must be checked against GridModel's invokable surface.
- `ExpandDataProxyModel` = their `QIdentityProxyModel` rework + our read-only guard, keyed on
  `dataSetSourceModel() != nullptr` (their `useUndoStack()` degenerated to
  `sourceModel() != nullptr` — useless as a gate). `flags()` strips `ItemIsEditable` for
  orchestrator-backed sources; `setData` passes through to the source. **This is the surface the
  edit era re-enables.**
- `runComputedColumn`/`runComputedDataSet` signals are intentionally UNCONNECTED (engine lane
  dead; route via the orchestrator R lane when it lands). Same for `toggleColType`, cell edits,
  `setColumnName` — all logged no-ops.
- Computed datasets: UI exists (ComputeDataSetPanel), computation dead on arrival by design.

## 4. Verification state (as of the rename commit)

- **Builds:** `CommonData`, `JASPDesktopLib`, `Desktop/JASP`, all test targets —
  `build/Desktop_Qt_6_11_0-Debug` (Qt 6.11.0, BUILD_TESTS=ON; the other QtC dirs are stale).
  NOTE: the AGENTS.md `build/` dir does not exist on this machine; the QtC dirs are the real
  build trees. Fresh configures elsewhere fail on R/Rcpp (system R too new; the QtC cache
  carries prebuilts) — don't try a clean-dir configure to "help".
- **Tests:** ColumnEncoderContext 5/5, CsvPrev 8/8, DebugData 15/15 (offscreen; no xvfb on
  this box — `JASPQuickTest` was never run).
- **Harnesses:** `run_viewbuffer_harness.sh`, `run_viewfiller_harness.sh` — ALL OK. The filler
  harness constructs `ViewFiller("ds-test", &buf)` (id string ctor, post-fold).
- **Known-failing (all in the SQLite/legacy-import zone, documented in merge-multidataset.md
  §7):** `JASPTest testDataImport` CSV+TSV goldens (empty-values application — likely stale
  goldens vs changed upstream semantics; needs ONE run on origin/development to confirm);
  `debug-0.18.3.jasp` load asserts in `DatabaseInterface::dataSetName(-1)`; `JASPTestDbMigration`
  `:memory:` load error.

## 5. GUI smoke checklist (THE GATE — run before trusting any of this)

1. Open a CSV: analysis form shows columns; header shows type icons; grid shows data.
2. Open a SECOND CSV: new tab, becomes active, shows ITS data (exercises the fold end-to-end).
   Known limitation: the open flow targets the SHOWN dataset — sequential opens are correct,
   concurrent opens would land on the wrong one (documented, acceptable until the multi-dataset
   open flow).
3. Open an excel/.sav (legacy path — untouched by the fold, must behave exactly as before).
4. terror_tall jump-storm per `HANDOVER-dataview-inc3.md` §5 (urgent fetches, eviction,
   budget-stop, failure-recovery on jump).
5. In QtC: run `JASPTest testDataImport` on `origin/development` once — settles whether the
   golden failures are pre-existing upstream (my bet: yes).

## 6. Brief for the next session: designing data edit

The merge handed us the edit **vocabulary** (their per-dataset undo commands — `SetDataCommand`,
`InsertRowsCommand`, … — the stacks, the proxy surface) while the edit **store** is the
orchestrator (Arrow + revision). The edit era is the marriage. Design questions, in order:

1. **Wire ops.** `data_edit`/`data_update` shapes (data-view-format.md). Their undo commands
   are ≈1:1 candidate payloads (command fields → op fields). Decide the op set: cell writes,
   column add/remove/rename, type change, rows insert/remove.
2. **The seam.** Does `SetDataCommand::redo()` become a JaspClient submission, or does the
   UndoStack stay frontend-pure with a listener replaying commands into the lane? Undo =
   reverse op; the orchestrator's revision makes replay idempotent-safe. My instinct: commands
   translate to submissions; the stack never writes anywhere itself.
3. **Return leg = Increment 4 (`data_changed`), a prerequisite.** Edit → orchestrator applies →
   revision bump → push → buffer invalidation of affected ranges → GridModel refetch. Without
   it every edit leaves a stale view. Design them together but land `data_changed` first.
4. **Re-enable the surface** (§3): flip `ItemIsEditable` when the source dataset `isOpen()`;
   un-stub `toggleColType`/`setColumnName`; wire cell edits through the proxy → command → lane.
5. **The mirror collapse (§2) gets forced here.** Filters and computed columns write Columns;
   edits write the lane. Decide: Column becomes the metadata holder fed by the wire, or
   providers learn `schema()`. Pick deliberately — it shapes Phase B.
6. **Constraints (frozen):** one id space (`DataSet::datasetId` — every edit op names it); no
   double classes, no translation maps; per-dataset undo stacks stay (theirs); frontend never
   holds cells outside the viewport cache; edits to 30M-row datasets are chunk-shaped.
7. **Computed columns recompute** on edit — needs the orchestrator R lane (also the missing
   consumer of `runComputedColumn`). At minimum: fail loudly, define the hook.

Deliverable suggestion: `refactor_design/data-edit-design.md` — same discipline as
`merge-multidataset.md`: decisions frozen before implementation.

## 7. Sequencing truth (learned the hard way — do not start the SQLite strip early)

SQLite is still **load-bearing**: `DataSet::createColumn` writes db rows (the metadata mirror
inserts rows today), and `.jasp` load/save rides the whole substrate. The strip only becomes
possible after **the orchestrator owns every import format** (csv is done; excel/.sav/.ods/
.rdata → the lane is `csv2arrow`-generalization work). SQLite dies as the *tail* of that,
together with the legacy heaps. Order: `data_changed` → edit era → format expansion → strip.

## 8. Gotchas

- The AGENTS.md `build/` dir doesn't exist here; use `build/Desktop_Qt_6_11_0-Debug` (and
  re-run cmake configure after adding/removing files — GLOB).
- No xvfb on this machine; `QT_QPA_PLATFORM=offscreen` works for everything except
  `JASPQuickTest` (needs both, per AGENTS.md).
- The filler harness compiles `viewfiller.cpp` from a scratch copy (quote-include resolution);
  the script's `-I` list now includes `stubs/utilities` — don't "clean that up".
- Fresh CMake configures fail on R/Rcpp (system R too new; QtC cache has prebuilts). Work in
  the existing QtC build dir.
- `Workspace` still contains dead db code (dbLoad/dbUpdate/syncer) until §7's tail — its
  presence is NOT an invitation; don't build on it.
- reporter.cpp's db-sync accessors are package-level no-op compat stubs
  (`synchingExternally`/`isDatabaseSynching`/`setSynchingExternally`) — die with the strip.
