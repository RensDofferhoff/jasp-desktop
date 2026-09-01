# HANDOVER — Data Edit era: the `data_changed` rail (a+b), the lane engine (d1–d8), and the seam (e)

**Status:** 2026-08-31. Implements `refactor_design/data-edit-design.md` **rev 4** (decisions
D1–D11 frozen; do not relitigate — open items are its §11). Done: build-order steps **(a)**
(`data_changed` wire + orchestrator rail) and **(b)** (frontend dispatch → `applyRevision`),
plus **d1–d8** of the lane slicing — the edit family is COMPLETE and LIVE: all six ops
serve with their undo programs, `insert_block`'s declared `target_schema` (d6b, P13)
round-trips, the lane ADVERTISES `data_edit` (d8), and the real-lane e2e crown proves
the whole rail. **(c)** DONE (five `applyRevision` scenarios). **(e)** slices 1–2 LANDED
and UI-VERIFIED: cell edits + paste + undo/redo, and column-type switching via
`schema_change`. The 2026-08-31 big-data session (terror_tall, 30M×8) hardened the
view path (five memory fixes) and landed the undo byte cap — and set the DIRECTION:
**stop patching the legacy view machinery; remove it** (Phase B accelerated, §4).

All Rust tests green on real sockets (146: 91 + 44 + 11); clippy clean. C++ runnable
set 16/16 (§5). Release binaries current with d8+.

## 0. Read first

- **Resume pointer**: **(e)** slices 1–2 are LANDED and UI-VERIFIED (§1e): cell edits,
  multi-column paste, Ctrl+Z/Ctrl+Shift+Z, and column-type switching (header menu +
  status-bar toggle) all run in the real app against the real orchestrator. The
  big-data session hardened the view (five memory fixes + the undo byte cap, §1e) —
  but **memory still creeps on scroll and CPU sometimes stays high** (the leading
  suspect: `DataSetSyncer` running on lane datasets — §4 R1). **DIRECTION SET
  (2026-08-31, the user's call): stop patching the legacy view machinery — it is
  scheduled for deletion (Phase B). Next work is the REMOVAL plan in §4 (R1 syncer,
  R2 mirror audit, R3 dead code), then the remaining (e) features (row/col structural
  gestures, `setColumnName` rename, labels editor).** Environment note: ipc-dependent
  Rust suites need `unsandboxed`; the C++ suite's PRE-EXISTING breakage is §5.
- `data-edit-design.md` — the contract. §2 wire shape, §3 ops + atomicity, §4 type/label
  resolution, §5 undo, §6 `data_changed` + build order, §10 lane notes.
- `data-view-format.md` §1.2 — the TSV grammar (escape INTO and now parse BACK from).
- `HANDOVER-multidataset-fold.md` §3 — the (e)-era seam checklist still pending.
- This file §3 — invariants that are easy to break silently (now 12 of them).

## 1. What landed

### (a) `data_changed` — the Increment-4 return leg

- `orchestrator/src/messages.rs`: `Message::DataChanged(DataChanged)` (tag `data_changed`),
  `DataChanged{dataset_id, dataset_revision, rows?, schema? iff changed, invalidation, cause}`,
  `Invalidation` (the four normative shapes `{}` / `{rows_from}` / `{all:true}` /
  `{rows_from,rows_to}`), `DataChangedCause` + `ChangeKind{edit,derived,external}`;
  `DataResult` gains `invalidation` + `validation` (lane → orchestrator transport only).
- `orchestrator/src/main.rs`:
  - `dispatch_work` **edit arm**: exactly-one-`dataset_id` check, `edit`-presence check,
    **D11** base-revision check (`Work.revision` vs the dataset entry; mismatch →
    `send_stale_edit`: a synthesized `validationError` RESULT with the `stale_edit` code —
    same shape as a lane refusal, nothing dispatched), mints the NEXT revision +
    `<id>_<rev+1>.arrow` cache path, records `DataWorkEntry{op: Edit, revision: new}`.
    **`payload.source` = the pre-edit cache** (read), `payload.cache_path` = the new file
    (write) — mirrors `data_open`'s direction. The lane's typed parse CANNOT see the
    top-level `dataset_paths` map (serde drops unknown fields) — this is why source rides
    the payload (P3, design §2 sketch amended).
  - `route_result` **edit arm** = the revision-bump point: Complete → revision bump + path
    swap (old file retires via `release_paths` when refs drain; the `still_current` guard
    flips here); failure → janitor sweep of the partial file. Then: `dataset_revision`
    stamped POST-APPLY (views stamp at dispatch — different semantics, both documented),
    **D6 strip** (`rows`/`schema`/`invalidation` nulled on the forwarded result — undo
    material only), then `broadcast_data_changed` AFTER the result on the same channel.
    Cause is diagnostics-only.
  - `miss_work`: `DataOp::Edit → Some(LaneKind::RustData)` (edits park like the rest of the
    data plane — unadvertised until d8, so nothing routes yet).
- Tests: `edit_completion_bumps_revision_and_broadcasts_data_changed` (also asserts the
  `source` injection), `stale_edit_is_rejected_at_dispatch`,
  `failed_edit_leaves_revision_and_broadcast_silent`, `invalidation_serializes_the_four_shapes`,
  `data_changed_envelope_carries_the_wire_tag`.

### (b) Frontend dispatch → applyRevision

- `Desktop/jaspclient/jaspclient.{h,cpp}`: the skeleton `datasetChanged(QString, quint64)`
  (which read the WRONG field — `revision`) replaced by
  `dataChanged(datasetId, revision, rows, hasRows, schema, invalidation)` parsing the frozen
  shape. `cause` never carried (diagnostics; visible via `JASP_CLIENT_LOG=full`).
- `CommonData/workspace.{h,cpp}`: `dataSetByLaneId` + `applyLaneRevision` (unknown dataset →
  log + drop; a closed tab's push has nothing to invalidate).
- `CommonData/dataset.{h,cpp}`: `applyRevision` (guard `revision ≤ _laneRevision` → ignore,
  §6 idempotence; rows-iff-present; schema-iff-present via the extracted `landWireSchema`
  shared with `applySchema`; invalidation accepted but NOT range-applied — v1 whole-buffer);
  `laneRevision()`; `_laneRevision` reset to 0 by `applySchema` (fresh identity).
  Emits `schemaChanged` on every landing → `GridModel` restarts the view lane = the v1 drop.
- `Desktop/mainwindow.cpp`: `JaspClient::dataChanged` → `Workspace::applyLaneRevision` via a
  lambda (bridges Desktop signal types into CommonData, which cannot include jaspclient.h).
- `Desktop/data/gridmodel.cpp`: `startView()` resets the buffer at
  `_dataSet->laneRevision()` instead of hardcoded `0` — the fill identity stays honest
  across edits (chunks stamp dispatch-time revision; a stale pre-edit chunk is dropped by
  the revision/epoch guard and the filler refetches).
- Limitation recorded: the legacy-column mirror in `landWireSchema` only GROWS (never
  shrinks/renames) — column-deleting edits leave it stale until Phase B deletes it; the
  grid itself reads `_schemaColumns` and is correct.

### (d1) Wire types

`messages.rs`: `EditOp` (7 variants, `op`-tagged snake_case: `insert_block{row,col,
target_schema?}`, `insert_rows{at,count}`, `delete_rows{at,count}`,
`insert_cols{at,columns}`, `delete_cols{at,count}`, `schema_change{target_schema}`,
`apply_inverse{inverse}`), `NewColumnSpec{name, display_name?, type?: "scale"|"ordinal"|
"nominal"|absent=infer, levels?}`, `InverseMeta{format, base_revision, ops}` on
`DataResult.inverse`, `FORMAT_ARROW_IPC = "arrow/ipc"`. `DataWork.edit` is
**`Option<Box<EditOp>>`** — clippy's enum-size catch; open/view clones must not pay for it.
Tests: `edit_ops_round_trip_with_the_wire_shape`, `edit_result_carries_inverse_meta`.
`messages.schema.json` regenerated.

### (d2) Shared core (the no-copies rule made real)

`csv2arrow.rs` now exports (`pub(crate)`) the semantics the edit engine imports, never
re-implements: `Locale`, `Level` + `as_str`, `decide`, `ColStats`, `CatDict`/`prebuild_dict`,
`jasp_column_names`, `count_distinct_numbers`, `make_feather_writer` (generalized to
`make_ipc_writer<W: Write>` — the inverse blob uses the same LZ4 codec stack),
`WIRE_LEVELS_CAP`, `DISTINCT_CAP`, `null_spellings`. NEW one-home helpers:
`append_levels` (absorption extension), `unique_new_name` (naming convention, single-column
form), `jasp_field` (field decoration, extracted from `build_output_schema`),
`level_of_field` (its inverse), `cat_dict_from_values`, `numeric_levels_of`,
`column_info_json` (the wire-schema JSON — shared by open and edit results so the shapes
cannot drift). `arrowview.rs`: `footer_info` pub(crate); the §1.2 parse-back twins
`split_row` + `unescape_cell` beside `escape_into` (one grammar, one module).
`dataedit.rs` skeleton: parse-back + shape validation + cache-schema reading.

### (d3) `insert_block` — the engine

`orchestrator/src/dataedit.rs` (~1000 lines), `apply_insert_block`:

- **Resolution (§4)** per block column against the live schema (read from the Arrow field —
  Float64 = scale, Dictionary ordered = ordinal / unordered = nominal):
  - scale **absorption** (type kept, `all_integer` recomputed from old hint ∧ block cells);
  - scale → nominal **promotion** when any cell fails the locale parse: merged column
    re-inferred csv2arrow-style — old f64s become canonical `'g'`-10 level strings (P1),
    distinct accumulated in POST-EDIT first-appearance order (old[0..row) + block +
    old[row+R..N)), dictionary via `prebuild_dict` (value-sorts within `sort_limit`);
  - categorical **absorption** (canonicalize-aware lookup, unseen values append in
    first-appearance order, old keys stay valid, dictionary never prunes);
  - new columns: D5 inference (`ColStats::observe` + `decide`) from the block cells;
    overflow/empty hole columns named per `unique_new_name` (→ `V{j+1}`), empty = nominal.
- **Two-pass streaming (rung 2)**: pass A (projection = touched columns) reads dictionaries
  (first batch — see the v1 cache contract, §3), promotion distinct sets, and the inverse
  capture; pass B streams the rewrite — untouched columns **Arc-clone** through input
  batches; touched columns rebuild per row from a segment table (`Old` identity / `Block` /
  `Null` holes); a grown tail appends in `OUT_BATCH` chunks (a Pass categorical's tail null
  keys carry its own dictionary so the writer never sees a replacement). Memory
  O(batch + block + inverse); IO O(dataset) per edit (2 reads when promoting).
- **Inverse (D10)**: rect capture of the clipped rectangle normally; **full-column capture
  for ALL touched columns when any promotion** (a promotion is a whole-column change — §5's
  bound; also uniform row-count for the IPC batch). Serialized via `make_ipc_writer`
  (LZ4). `InverseMeta.ops` v1 JSON: `{v, op:"restore_block", anchor{row,col,rows,cols},
  old_rows, old_cols, capture:"rect"|"full", capture_rows, columns:[{old_index,name,type}],
  trim_rows_from?, trim_cols_from?}`. `base_revision = job.revision` (the D11-checked
  current). **P5 enforced**: inverse > `MAX_INLINE_PAYLOAD` − margin → visible fatal.
- **Stats + schema**: every column's `ColumnInfo` recomputed DURING the passes (scale via
  `ScaleStats` — the Arrow-native capped-distinct twin; categorical distinct = dictionary
  size, `numeric_levels` via the shared helper); the full wire schema always ships for
  `insert_block` (value counts move).
- **Invalidation (§6, normative)**: `all:true` iff column-set change OR type change
  (promotion) OR value-levels change (absorption appended); else `rows_from` on row growth;
  else the covered `{rows_from, rows_to}`.
- Runner wiring (`data_runner.rs`): `deframe_parts` (tails survive the parse), the
  `DataOp::Edit` branch (`EditJob` → `serve` → Complete with `FORMAT_ARROW_IPC` tail /
  `validationError` with structured issues / fatal). **Capability NOT advertised** — no
  production route until d8.

### (d4) `insert_rows` / `delete_rows` — the row-merge engine generalization

`dataedit.rs` grew from ~1000 to ~2700 lines, and the specialized insert_block pass B was
REPLACED by one shared engine (no copies — the op apply fns are now thin: validation +
plan + segments + inverse + a `stream_rewrite` call):

- **`Seg` generalized to shifts** (the d4 design note): `Copy{out, old, len}` maps old
  rows onto output rows under a fixed offset (identity for `insert_block`, shifted for
  the row ops); `Block{out, len}` is insert_block's overwrite window (pass columns read
  the old rows UNDERNEATH — a paste overwrites only its rectangle; build columns take
  block cells with precedence); `Null{len}` covers both insert_block's holes and
  insert_rows' fill. Segment tables tile the output rows; for every op, old-row order
  across segments = stream order, so ONE sequential cursor serves the walk.
- **`OldRows` cursor** over the input batches: loads one batch at a time, `skip_to`
  jumps read position past removed windows (decoded + dropped — rung-2 IO),
  `current()` is fatal on early stream end (integrity vs the footer's promised rows).
- **`stream_rewrite`** drives by segment in OUTPUT order, pulling input pieces;
  `emit_piece` writes one output batch per (segment ∩ input batch) piece. Untouched
  columns pass through as ZERO-COPY SLICES (an Arrow slice shares its buffers — and the
  dictionary with them — so the d3 Arc-clone-through-batch optimization became
  slice-through-any-piece, strictly more general). Null ranges emit typed nulls with the
  column's own dictionary values (`null_pass_array`; empty values only when the cache
  itself had zero rows — never a placeholder while data exists, see the eager capture).
- **Eager first-batch dictionary capture**: before ANY piece is emitted, every Pass
  categorical's dictionary is captured from the first input batch. Load-bearing: a null
  fill can precede the first data piece (insert_rows at 0) — emitting it with
  placeholder values would CHANGE the field's dictionary mid-file, which the IPC writer
  rejects. (This also fixed a latent d3 panic — `expect("pass categorical saw its
  dictionary in pass B")` — for zero-row caches; `wire_schema`'s `cat` now has an
  empty-dict fallback instead of an unwrap.)
- **`insert_rows {at, count}`**: `at ≤ rows`, `count ≥ 1` (else `range`); all-Pass plan;
  invalidation `{rows_from: at}`; NO schema ships (I7 — nulls move no count); inverse is
  METADATA-ONLY (`{v:1, op:"delete_rows", at, count, old_rows}`, no bytes).
- **`delete_rows {at, count}`**: `at+count ≤ rows` else `range` (§3's explicit rule);
  the schema ALWAYS ships (value_count drops — removed rows remove values; the
  dictionary never prunes, so levels/distinct stay); invalidation `{rows_from: at}`.
  The inverse is captured BEFORE the writer opens: the removed rows, every column
  (window slices via `read_range` → `concat` per column → one IPC frame), P5-checked.
  Program `{v:1, op:"restore_rows", at, count, old_rows, columns:[{old_index,name,type}]}`
  — undo is multi-step (re-insert, then restore; §5).
- `INVERSE_FORMAT_V1` const extracted ("arrow_ipc_v1" — was a literal in two places).
- Tests (+9: 48 data-runner now): shift+fill, top/end/empty-dataset inserts, delete
  capture bit-exactness, range refusals with atomicity, **the batch-boundary crown**
  (`row_ops_across_batch_boundaries` — insert AT a boundary, delete ACROSS one, delete
  a whole tail batch), and **`chained_edits_keep_the_cache_contract`** (an edit's OUTPUT
  feeds the next edit — insert_block → insert_rows → delete_rows, dictionaries verified
  at every hop; the v1 one-dictionary-per-field contract survives stream_rewrite's own
  files). New fixture `direct_cache` writes v1 caches with EXPLICIT batch boundaries
  (conversion's are byte-budgeted — hundreds of thousands of rows, too heavy for tests).
- Orchestrator: NO production-code change (the rail is op-agnostic) — one new test
  `row_op_edit_broadcasts_data_changed_without_schema` drives `insert_rows` through the
  real rail and asserts schema-ABSENT `data_changed` (the None path had never run);
  `lane_edit_result` generalized to `Option<Value>` schema.

### (d5) `insert_cols` / `delete_cols` — the plan does everything

The column ops confirmed the engine's shape: **rows never move, so the segment table is
ONE identity `Copy{0, 0, n_rows}` — all the work is the column plan.** No engine changes
were needed beyond the plan; `ColBuild::BuildFloat{cells: []}` / `BuildDict{keys: []}`
already emit all-null columns correctly (cells/keys are only consulted for block
windows — an `insert_cols` carries no cells).

- **`insert_cols {at, columns}`**: `at ≤ cols`, non-empty spec list (P7) else `range`;
  per-spec validation BEFORE planning (P8): unknown type string → `schema_mismatch`,
  `levels` on a scale spec → `schema_mismatch`, duplicate entries → `schema_mismatch`;
  absent type = nominal (inference over ZERO cells — `decide`'s empty-column answer).
  Names uniquify against the full post-edit list as each lands (P6). Declared levels
  pre-populate the dictionary VERBATIM via the new `csv2arrow::dict_from_list` — spec
  order IS the level order (never value-sorted; an ordinal's order is its meaning) —
  `[]` ≡ absent. Explicit `display_name` honored verbatim; absent → the uniquified name.
  Inverse metadata-only: `{v:1, op:"delete_cols", at, count, old_cols}` (nothing was
  destroyed). `{all:true}`; schema ships (column set changed).
- **`delete_cols {at, count}`**: window inside extent, ≥1 column kept (P7 — a zero-column
  dataset is degenerate: no schema, no grid; deleting every column in one action is a
  mistake atomic refusal serves better) else `range`. The inverse captures the removed
  columns FULL-WIDTH (projected read of just those columns — pass-A economy; whole-batch
  slices; zero-row caches get an empty typed column so the capture batch stays shaped),
  dictionary-typed so restore is type-faithful. Program
  `{v:1, op:"restore_cols", at, count, old_cols, columns:[{old_index,name,type}]}`.
- **csv2arrow**: `unique_new_name` now LOOPS until genuinely free (P6 refinement — the
  old single-append could emit a duplicate when `score_3` already existed; insert_block's
  growth naming gets the fix for free); NEW `dict_from_list(levels)` — a declared-list
  dictionary, no sort, no canonicalization (d6's `target_schema` levels come here too);
  `pass_col`/`plan_schema` helpers extracted in dataedit.
- Tests (+6: 54 data-runner): shifts+fills (spec-order levels, the ordinal ordered flag,
  null stats), the uniquification loop (score→score_4 when _2/_3 taken; ""→V3; "5"→V5),
  refusals with atomicity, full-width bit-exact capture, P7 refusals, and the two-batch
  boundary crown (capture concat across batches; insert at 0 shifting everything). TWO
  wrong first expectations were themselves the trap: the fixture's `prebuild_dict`
  value-sorts at build, so the engine "preserving verbatim" ≠ first-appearance order
  (invariant: the engine MOVES dictionaries, never re-sorts them); and a mislabeled
  test case tuple made a legal delete look like a refusal.
- No wire changes (d1 variants), no runner/orchestrator changes (rail op-agnostic).

### (d6) `schema_change` — the metadata op, the inverse classes, and the label overlay

`apply_schema_change` (~400 lines + two new `ColBuild` variants): the declared
`target_schema` is an ARRAY of entries in the NEW column order — `{name, display_name?,
type?, levels?, labels?}` where `name` is the IDENTITY (the column's CURRENT field name;
renames declare a new `display_name` and the field name derives from it, P4, uniquifying
per P6). Absent fields = keep; `labels: {}` DETACHES the overlay.

- **Match rule (§3 "never changes the column set")**: entry count must equal the column
  count, matched by current name, each exactly once — else `schema_mismatch`.
- **Rebuild classes (P9)**: `Keep` (rename/flag-flip/relabel — pure Pass; nominal↔ordinal
  is literally just `dict_is_ordered`, keys and values arrays identical);
  `Remap{new_levels}` (a declared list REPLACES the dictionary — `RemapKeys{dict,
  key_map}` rebuilds with the row→VALUE mapping preserved);
  `ToScale` (`FloatFromDict{old_values}` — per row: key → value string → locale parse);
  `ToCategorical{declared}` (reuses the promotion machinery: `BuildDict` with
  `promote_from_float: true`, P1 'g'-10 renders; dict declared verbatim or derived
  `prebuild_dict`-style from the rendered set).
- **Pass A (data-dependent validation + captures)**: projected read of the changed
  columns — per-level usage counts (the streaming key scan), scale→cat rendered sets,
  old dictionaries from batch one (v1 contract), full-width captures for re-encodes.
  Labels-only Keep columns read FIRST BATCH ONLY (the dictionary is all they need —
  a one-row `read_range`).
- **Validation**: cap rule both ways (current distinct ≤ `WIRE_LEVELS_CAP` to replace a
  list; rendered set ≤ cap to declare one for a retype) → `schema_mismatch`; declared
  list dropping an in-use level → `level_in_use` (10 examples); cat→scale with a
  non-numeric in-use level → `coercion`; scale→cat declared list missing a rendered
  value → `level_unknown`; labels keys must exist post-change → `level_unknown`;
  scale+levels/labels → `schema_mismatch`; duplicates → `schema_mismatch`.
- **Labels (P11, the real two-mappings model)**: `labels` writes the sparse
  `jasp:labels` field-metadata overlay via csv2arrow's new `attach_labels`/
  `labels_of_field` (one home beside `jasp_field`); a relabel never touches data
  (O(k)); `ColumnInfo.labels` (new wire field, sparse) carries it to the frontend.
  Labels-only → invalidation `{}` (the view renders VALUES today).
- **Inverse**: `{v:1, op:"restore_schema", old_order, columns:[{current, old_name,
  old_display, old_type, old_levels, old_labels, capture, capture_rows}]}` — JSON-only
  for Keep/Remap, one uniform full-width IPC batch for the re-encodes (P5-checked).
  `current` = the POST-edit name (what undo locates the column by).
- **Invalidation (§6)**: type change / value-levels change / column reorder → `all:true`;
  rename-only or labels-only → `{}`. Schema always ships.
- **all_integer** for cat→scale recomputes from the rendered set at plan time (the
  field is decorated before the walk, so it must be known upfront).
- Tests (+11: 65 data-runner): rename+reorder & rename-only invalidations, scale→nominal
  (P1 + bit-exact full capture), cat→scale with an orphaned level (absorption made it,
  an overwrite orphaned it, the capture keeps it — the never-prune chain), coercion
  refusal, flag flip, remap (verbatim order + preserved mapping + JSON inverse),
  level_in_use, labels (attach/wire/detach/unknown), 10 structural refusals, the cap
  rule on a real 10,001-level column, and the identity no-op. THREE wrong first
  expectations (the count-match rule correctly refused single-entry schemas on
  two-column fixtures; 10k distinct INTS convert to SCALE not nominal — need strings;
  and labels-on-Keep needed the first-batch dictionary read).
- csv2arrow: `attach_labels`/`labels_of_field`; `ColumnInfo.labels` + wire emission;
  conversion constructs it as `None` (CSV open never creates value≠label data).
- Remaining d6 scope → **d6b**: `insert_block` with a declared `target_schema`
  (overflow-column naming/types + adhere-or-error for covered columns); the serve()
  refusal still says d6b.

### (d7a) `apply_inverse` — the undo crown, five of six programs live

`apply_inverse(job, cache, meta, tail)` validates defensively (unknown `format` → Fatal;
`base_revision > job.revision` → `stale_edit` ValidationError — the LIFO guarantee is
broken; malformed/unknown programs → Fatal — a corrupt blob is infrastructure, not a
user error), then dispatches to the engine. Every result carries its OWN inverse (the
redo), invalidation is `all:true` (v1 honesty: a restore touches arbitrary regions),
and the schema always ships.

- **`delete_rows`/`delete_cols`** — DELEGATED to `apply_delete_rows`/`apply_delete_cols`
  (the undo of the inserts is a plain forward op); their own inverses ARE the redo.
- **`restore_rows`** — re-inserts the window via the NEW `Seg::Restore` (a Null segment
  whose BUILD columns carry restore cells — nulls underneath, IPC cells on top); all
  columns rebuild from the full-width capture. Redo: metadata-only `delete_rows`.
  Outside the window, Build arms read the current file — valid because `delete_rows`
  passes dictionaries through untouched (never prunes).
- **`restore_cols`** — ONE `Block` window over all rows: survivors pass underneath
  while restored columns replay; fields come from the IPC SCHEMA (Arrow persists field
  metadata — names, ordered flags, `jasp:labels` ride the capture verbatim!).
  Redo: metadata-only `delete_cols`.
- **`restore_block`** — the fullest program: window at `row` (rect) or `0` (full),
  trailing copy, trim to `old_rows × old_cols`; touched columns restore their pre-paste
  FIELD + cells from the IPC (a zero-ROW capture still matters — its values slot carries
  the pre-edit DICTIONARY); program columns with `old_index ≥ n` are RECREATED (the redo
  re-pastes overflow columns the undo trimmed). The ONE segment formula covers BOTH
  directions — shrink-undo and grow-redo (Block's saturating underneath turns missing
  rows into nulls, which grown rows were). The redo capture (taken before the write)
  covers the current window — full-width when any existing touched column re-encodes
  (I4) — PLUS the overflow columns' window slice.
- **`restore_schema`** → still Fatal (d7b).
- serve() is now EXHAUSTIVE over EditOp (no catch-all — the compiler forces new arms).

**The crown harness (pinned discipline, implemented)**: `snapshot(path)` captures each
column's WHOLE truth — name, display, level, ordered flag, all_integer, labels overlay,
dictionary ORDER + unused levels, every cell with f64 as `to_bits()` — and `round_trip`
asserts BOTH algebraic identities for every scenario: `undo ∘ edit = id` and
`redo ∘ undo ∘ edit = edit`. Matrix: paste in-range/growth/promotion · row ops all
variants + a batch-boundary delete · col ops incl. name collisions · the LIFO chain
(e1→e2→undo e2→undo e1→original) · refusals (unknown format, stale base, unknown op).

**Three real bugs the crown caught on the way** (the harness earning its keep):
1. `emit_piece`'s Build arms unconditionally `as_primitive`/`as_dictionary` — a restore
   plan pairs an f64 build with a dictionary source (undoing a promotion) → panic. Now
   PHYSICAL-TYPE-GUARDED (mismatched sources read as None; the window supplies cells).
2. d3's growth capture synthesized `new_null_array` for zero-rect-row columns — an
   EMPTY dictionary rode the blob, so undo restored cells but left the absorption-grown
   dictionary. Now a zero-row categorical capture carries the column's REAL pre-edit
   dictionary on its values slot (new `pre_edit_values` stash in pass A).
3. The redo had no way to re-paste overflow columns (their cells were never captured) —
   programs now carry `old_index ≥ n` recreation entries, and the redo capture includes
   the overflow window slice.

Tests (+10: 75 data-runner). Engine line count ~5200.

### (d7b) `restore_schema` — the crown complete, six of six programs live

`restore_schema_exec` (~330 lines): rows never move, so the walk is ONE `Block` window
over the extent — capture columns replay their IPC field+cells wholesale (metadata rides
Arrow: names, ordered flags, `jasp:labels`, `all_integer` — which is why a retype's undo
needs NO metadata reconstruction), everything else reads the current file underneath.

- **Locate by `current`** (the POST-edit name — undo runs after the edit); `old_order`
  restores POSITIONS: an `old_order` name resolves to its program entry (by `old_name`)
  or to a pure-Keep column passing through under its unchanged name. The coverage checks
  (every current column consumed exactly once, `old_order` a permutation of the pre-edit
  names) are defensive Fatal — a corrupt program is infrastructure (P12).
- **The metadata classes rebuild their field from `old_*`** via the one-home helpers
  (`jasp_field` + `attach_labels`; `all_integer` survives on the current field — a
  metadata-only change never moved the data, so the hint never moved). A categorical
  remaps keys (`RemapKeys`) into `dict_from_list(old_levels)` — the recorded list IS the
  pre-edit dictionary, verbatim order (invariant 9). Needs the CURRENT dictionary (a
  first-batch read, like d6's labels-only scan). Null-safety: every row's key maps —
  a row's value was in-use pre-edit, and d6's `level_in_use` check proved it survives
  the remap; `-1` (unmapped) hits only UNUSED levels, which no row references. Scale
  entries and the empty-dict case (a zero-row cache) are `Pass`.
- **The re-encodes replay the IPC blob** (`restored_of`/`restored_build`): field + full
column from the capture, `old: None` — the underneath is never consulted. The blob
decodes only when some entry declares a capture; bytes without a declared capture (or
the reverse) are Fatal. A metadata-only entry whose `old_type` disagrees with the
current physical type is Fatal (only re-encodes change physical type, and those carry
a capture — P9).
- **The redo records the post-edit state exactly as d6 recorded the pre-edit one**:
  entries swapped (`current` = the restored name — the redo locates columns AFTER the
  undo), `old_*` = the post-edit identity (dictionary from the same first-batch read,
  labels/display from the current field), `capture: full` for exactly the re-encode
  entries, blob = the current columns full-width taken BEFORE the write (P5-checked).
  The redo is itself a `restore_schema` program — full symmetry; undo always invalidates
  `all:true` and always ships the schema (P12).
- Tests (+9: 84 data-runner): seven crown scenarios — rename+reorder, flag flip + labels
  attach, level remap, scale→nominal, cat→scale with an ORPHANED level in the dictionary
  (absorb + overwrite first — the capture must restore it), a MIXED change (rename +
  reorder + retype + relabel in one program), and labels DETACH (attach first — CSV open
  never creates an overlay, so the source is a prior edit's output) — plus the output
  shape (P12 envelope, swapped redo entries, the redo's own capture) and five defensive
  refusals (ghost `current`, extent mismatch, family mismatch, unrestorable `old_order`
  name, capture without a blob).

All six programs live; `serve()` is exhaustive with no stubs. Engine ~5900 lines.

### (d6b) `insert_block`'s declared `target_schema` — the family completes (P13)

`parse_window_schema` + the declared arms of `apply_insert_block`: a **window-scoped
POSITIONAL array of NULLABLE entries** over the paste's columns (entry i ↔ output
column col+i; length must equal the paste width). `null` ≡ undeclared for that column
(exactly the no-schema behavior); a non-null entry is a strict PER-FIELD postcondition
(absent fields auto, declared fields adhere-or-error).

- **Covered entries** (binding an existing column): `name` optional but must echo when
  present (positional-alignment guard); declared `type` must EQUAL the current one
  (retype = `schema_change`'s job, P13 v1 — the d6c candidate in §6 extends this);
  `display_name` and `labels` keys refuse (rename/relabel are schema_change's job);
  declared `levels` REPLACE the dictionary per d6's Remap class — verbatim order
  (`dict_from_list`), coverage validation against the POST-edit in-use set (the old rows
  OUTSIDE the window — a level used only inside the window may drop, `level_in_use`
  fires with 10 examples otherwise), the cap rule, and pasted cells must exist in the
  list (verbatim lookup — never canonicalized).
- **Overflow entries** (new columns): `name` must be free (existing columns, other
  declarations, auto-name interleave — a postcondition is NEVER silently uniquified;
  insert_cols' P6 differs because its names are inputs); `type` honored (scale = cells
  must parse — NO promotion under a declaration; ordinal/nominal = declared list
  verbatim or derived from the cells); absent type = D5 inference; `display_name`
  verbatim, absent = the name.
- **The declared scale refusal happens at classification**: a covered scale entry with
  unparseable cells is `schema_mismatch` BEFORE any promote decision (adhere totally).
- **`RemapKeys` grew a paste window**: `keys: Vec<Option<i32>>` — non-empty ⇒ block
  cells take precedence inside the Block segment (BuildDict's overwrite rule) while
  Copy segments still remap the current rows underneath. Empty `keys` (the
  schema_change/restore_schema constructors) keeps the underneath semantics even in a
  Block segment — restore_schema's whole-extent window depends on that, unchanged.
- **I4 widened**: full-column inverse capture iff any touched column RE-ENCODES
  (promotion OR declared Remap) — `to_row`/capture follow; `levels_grew` covers the
  value-levels invalidation (I6 → `all:true`).
- **The redo's re-encode detector grew a dictionary check** (`restore_block_exec`):
  physical type equality is no longer sufficient — a Remap keeps the physical type but
  breaks the append-only prefix property that makes key-passthrough valid outside the
  window (absorption only APPENDS, so indices survive). A first-batch sniff read of the
  current columns feeds a prefix-either-way check on the values arrays; neither a
  prefix ⇒ the redo captures full-width. This is invisible to absorption (prefix holds
  both directions → rect, unchanged behavior) and the crown proves the remap redo
  bit-exact.
- **Shared derivations, one home**: `derive_dict` (D5's nominal derivation — stats,
  first-appearance set, canonicalize rules) now serves both inference and a declared
  categorical without levels; `declared_keys` is the one adherence lookup (refusal names
  the offending cell). The D5 inference body no longer duplicates the set loop.
- Tests (+7: 91 data-runner): `insert_block_declared_schema_{names_overflow,
covered_remap,covered_adherence,refusals}` and the crown `rt_insert_block_declared_{
overflow,remap,scale_growth}` — both algebraic identities for every declared path.
`dataedit.rs` is ~7300 physical lines (code + tests).

All six ops + six restore programs live; `serve()` has NO refusals left — d8 can
advertise honestly.

### (d8) Advertise + the real-lane e2e crown — the era's Rust side complete

- **The advertisement**: `Capability::Data { op: DataOp::Edit, formats: None }` joins
  Open+View in the data-runner's Register (`src/bin/data_runner.rs`) — format-agnostic
  like view (it reads the cache the orchestrator injects as `source`). Every routing
  path already knew the shape: `select_runner`, `try_dispatch_parked`, `miss_work`
  (`DataOp::Edit → LaneKind::RustData`), and the edit arm of `dispatch_work` (D11
  check, revision minting, source/path injection) were landed in earlier increments —
  d8 flipped the one flag that makes production edits routable.
- **The e2e crown caught a REAL RAIL BUG**: the orchestrator's work relay dropped the
  frame's BINARY TAIL on frontend→runner forwarding. `arm_frontend_channel` deframed
  with plain `deframe` (tail discarded) and `dispatch_work` re-framed with
  `frame_bytes` (no tail) — so the lane received the edit JSON but an EMPTY block
  ("the edit block is empty", the anchor refusal). The runner→frontend direction
  already carried tails (`RunnerData` + `route_result` re-frames verbatim — that's
  why views worked); the mock-lane orchestrator tests never send tails, so only the
  real-lane e2e could catch it. **Fix**: the tail now threads `FrontendData` →
  `on_frontend_message` → `route_work` → `dispatch_work` (`frame_parts`) and parks
  with the work (`ParkedWork.tail` → `try_dispatch_parked`) — the mirror image of
  the runner-tail path, bulk bytes never through the JSON parser.
- **`edit_undo_redo_crown_over_the_real_lane`** (tests/dataset_e2e.rs): open
  debug.csv → HOP 1 a declared-schema paste (P13: x's echo+scale-adherence entry, two
  nulls, Q4 nominal with declared levels) → `data_changed` rev 1 (D6-STRIPPED result
  asserted: identity + inverse only; the push carries rows/schema/invalidation) →
  HOP 2 `schema_change` (rename `group`→`Condition` + labels, a Keep-class change —
  JSON-only inverse asserted) → undo ×2 (`apply_inverse` with the stored (meta,
  bytes) VERBATIM; LIFO; undo always `all:true` + schema per P12) → the ORIGINAL,
  view-verified bit-exact (`undo∘edit = id` over the real rail) → redo → the edited
  state again (`redo∘undo∘edit = edit`). The revision ladder 0→5 is asserted at
  every hop, and views render from the swapped cache each time. Helpers added:
  `edit_dataset` (frames the tail; captures the result frame's own tail = the inverse
  bytes), `recv_data_changed`, `view_row0`.
- Incidental: debug.csv's `z` (few distinct ints) converts as ORDINAL — a declared
  `scale` entry on it is a covered type-change → P13 refusal (the first draft of the
  crown hit it; the refusal message was exactly right).
- Tests: +1 e2e (11 total). Suite: 146 green.

### (e) The frontend seam — slice 1: cell edits, paste, and Ctrl+Z live in the UI

The wire vocabulary lives with the client (the architecture rule: the rest of the
frontend never sees a wire format) — `Desktop/jaspclient/dataedit.{h,cpp}`:

- **`DataEdit::` builders**: `insertBlockOp(row, col)` (undeclared — D4's absent path,
  absorption/promotion), `applyInverseOp(meta)`, `escapeCell` (§1.2's mirror of
  `DataViewBuffer::unescapeCell`: `\\`/`\t`/`\n`/`\r`, `\N` for null cells, an empty
  string stays empty — the lane's null spellings null it, I3), `tsvFromCells`
  ([col][row] rectangle → tab-separated, LF-terminated rows).
- **`DataEditCommand : UndoModelCommand`** — ONE command wraps ONE wire edit.
  `redo()` SUBMITS the op (`revision = laneRevision()` read at call time — D11);
  the result's inverse (`Result.inverseMeta` + the frame-tail bytes) is stored
  VERBATIM (D10); `undo()` resubmits it via `apply_inverse`. Redo re-submits the OP
  (the design's pin — sound under strict LIFO, and the stored blob stays the one
  thing undo ever needs: undo→redo→undo reuses the ORIGINAL blob every time).
  Failures log loudly (v1 surfacing); a refused edit changed nothing (§3 atomicity).
- **`JaspClient`**: `submit(work, handler, binary)` — outbound frame tails (§18.1,
  the mirror of the result-tail path); `submitDataEdit(datasetId, baseRevision, op,
  tail, handler)` builds the envelope (ViewFiller's data_view shape with `op
  data_edit` + the adjacently-tagged edit; ingest carries the system decimal only
  when comma — the lane default is `.`); `Result.inverseMeta` parsed beside the
  existing tail handling.
- **The surface**: `GridModel::flags` gains `ItemIsEditable` when the dataset
  `isOpen()` (+ a public `dataSet()` accessor); `ExpandDataProxyModel` — the editing
  gate becomes "legacy source OR live NEO dataset" (`gridSourceDataSet()`), `setData`
  builds ONE 1×1 `insert_block` per commit boundary, `pasteSpreadsheet`'s NEO branch
  (previously a silent drop!) builds ONE `insert_block` over the whole rectangle —
  identity mapping (the NEO view has no filter compaction; an anchor past the extent
  GROWS the dataset remotely — `data_changed` restarts the view, no local resize).
  Labels/colNames are deliberately not this op's business (the label editor /
  header-rename surfaces own them).
- **The loop as it runs**: type/paste → proxy command → `JaspClient::submitDataEdit`
  → lane applies → result (D6-stripped) files the inverse in the command →
  `data_changed` → `applyRevision` (the (c)-tested contract) → `schemaChanged` →
  GridModel restarts the view at the new revision (the v1 whole-buffer drop).
  Ctrl+Z → `undo()` → `apply_inverse` with the stored blob → same return leg.
- Validation: JASPDesktopLib + JASP app build clean; the runnable JASPTest set
  (15: savLabels + 7 syncer + 5 lane scenarios) green with the new code linked in.
  **UI-VERIFIED 2026-08-31**: cell edits + multi-column paste (incl. one-paste undo)
  confirmed working in the real app against the real orchestrator on a 100k×30
  dataset.
- **The 2026-08-31 smoke test caught TWO seam bugs, both fixed**:
  1. `JaspClient::submit` MINTED the correlation id but never wrote it into the
     outgoing JSON (`const Json::Value&` — it couldn't), and `submitDataEdit` set
     neither `id` nor `work_id` — both REQUIRED envelope fields. The frame left the
     client (TX logged), failed the orchestrator's typed parse, and vanished.
     Every prior caller set its own ids, so the path was never exercised. Fix:
     `submit` copies the envelope and INJECTS the minted `work_id` + `id`. Rule: a
     frame's identity must be IN the frame, not just in the slot map.
  2. The orchestrator DROPPED unparseable frames in total silence — TX on one side,
     nothing on the other, both logs innocent. Fix: `arm_frontend_channel` eprints
     `undecodable frame (N bytes) — dropped` (never-swallow on parse failure).
  Diagnostics added alongside (keep them): the proxy's NEO refusal paths and
  `DataEditCommand`'s submit/apply stages log via `Log::log` — the stage that dies
  now names itself.
- Known QML wart (not blocking, pre-existing): `DataTableViewEdit.qml:181` logs
  `Unable to assign [undefined] to bool` on edit-item transitions — worth a look
  when the editing UX gets polished.
- NOT yet (later slices): `setColumnName` rename (schema_change), row/col insert-delete
  gestures → their ops, range-aware invalidation, the paste dialog's `target_schema`
  declarations, the labels editor (B2).

### (e2) Column-type switching — the crash and the feature

The header's type menu reached the LEGACY path (`proxy::setColumnType` →
`SetColumnTypeCommand` with a NULL dataset for a NEO source → `_dataSetID = -1` →
`UndoModelCommand::dataSet()` asserted). Fixed by routing the gesture where the design
always said it goes — `schema_change` (`c52e581af`):

- `DataEdit::schemaChangeTypeOp(ds, names, newType)` — d6's count-match shape: every
  column entry by CURRENT field name, `type` on the targets (`wireTypeOf`: nominalText
  rides as nominal).
- `proxy::setColumnType` NEO branch submits one `DataEditCommand` (no tail);
  `GridModel::toggleColType` un-stubbed: the status-bar toggle cycles
  scale → ordinal → nominal → scale (double-click: ALL columns in one op).
- Known cosmetic wart: the header type menu's icons resolve through
  `JaspTheme::currentIconPath()` to a non-existent qrc prefix (`variable-nominal.svg`
  lives under `QMLComponents/icons`) — labels render, icons don't.

### (e3) The big-data session — terror_tall (30M×8): five fixes, one direction

Loading terror_tall ate 10+ GB. The hunt found FIVE row-sized/usage-sized offenders in
the legacy view path (each fixed + committed) and — the real lesson — that they all live
in machinery whose designed fate is DELETION (Phase B). **Direction set: stop patching,
start removing (§4).**

| # | Offender | Cost on 30M×8 | Commit |
|---|---|---|---|
| 1 | `startView` overwrote `_buffer`/`_iller` — every restart leaked a full TSV buffer AND a still-running filler (the repeating fill cycles in the orchestrator log) | unbounded ×dataset | `6560a0fae` |
| 2 | `landWireSchema` → `setRowCount(rows, false)` — "metadata only" in comment only; it resized every mirror Column's `_dbls`/`_ints` to rowCount + the filter vector | ~2GB instantly at "dataset ready" | `9bf4ab1ae` |
| 3 | `_storedDisplayText` — per-visited-row map, never trimmed | ~0.7KB × rows scrolled | `baad0849b` |
| 4 | `_storedLineFlags` — same disease (1 byte of flags on ~430B of map nodes per cell) | ~0.4KB × rows scrolled | `a6526730b` |
| 5 | item pools (`_textItemStorage`…) never shrank — flings left permanent balloons | KBs × peak items | `a6526730b` |

- Fixes 3–4 use the SAME pattern: **editable (lane) cells serve live from the model and
  never enter the cache** — the lane's roles are cheap (the `lines` role is pure index
  arithmetic); legacy datasets keep their caches.
- **The undo byte cap LANDED** (§11.4, `a6526730b`): `UndoModelCommand::undoBytes()`
  virtual; `DataEditCommand` counts inverse blob + forward tail;
  `UndoStack::enforceUndoByteCap()` after every push sums top-down and drops OLDEST at
  ~250MB via the `setUndoLimit(n)`-then-`(0)` trick (QUndoStack cannot remove arbitrary
  commands); over-cap macros log but keep their children (§5's honest minimum).
- **STILL OPEN after all five**: memory creeps on scroll (slower) and CPU sometimes
  stays high. Leading suspect for BOTH: `DataSetSyncer` running on lane datasets
  (§4 R1) — the FileMenu open path starts file syncing/watching on a 200MB CSV the
  lane owns; periodic re-reads/hashes = sustained CPU; the cross-thread warning in
  the user's log is it. Secondary suspects: glibc arena retention from the 200MB
  chunk churn; QML relayout churn from per-chunk `dataChanged` emissions.

### (c) The C++ harness scenarios — `applyRevision`'s contract pinned

Five slots in `Tests/testall.*` (after the syncer section), with a lane-bound fixture
`_newLaneDataSet` (a fresh `DataSetPackage` + `applySchema` as the open result — no
import) and wire-schema builders (`laneColumn`/`laneSchema` — the lane's column-info
JSON shape exactly):

- `testLaneRevisionLandsRowsAndSchema` — the base contract: the open lands
  (isOpen/datasetId/revision-0/schema-verbatim-levels), a schema-carrying push adopts
  the revision + new levels + fires `schemaChanged` (the GridModel restart hook), and
  a rows-only push (I7 — the schema never ships) lands rows with the last schema
  standing.
- `testLaneRevisionIgnoresStalePushes` — the §6 ordering rule: replaying the current
  revision is a no-op; an OLDER push is ignored EVEN schema-carrying; and a dataset
  with no lane identity (legacy) takes no revisions at all.
- `testLaneRevisionSchemaSwap` — a `schema_change` return leg: rename (old name GONE
  from `schemaColumn`, new one present), retype (scale→nominal via the wire type),
  levels VERBATIM in wire order (the engine never re-sorts — invariant 9 on the C++
  side).
- `testLaneRevisionRowGrowthWithoutSchema` — the row-op return leg: rows grow (3→8,
  `{rows_from}`) and shrink with the schema intact both times.
- `testLaneRevisionOutOfOrderPushes` — the stale-interleave: a gap (rev 3 lands
  before 2) adopts the high-water mark; the late rev-2 push is dropped no matter what
  it carries — mid-fill interleaves resolve identically at the DataSet level.

**Harness gotchas pinned along the way**: (1) a SECOND `new DataSetPackage` within one
  test trips `DatabaseInterface`'s `!_singleton` assert — the fixture reuses one
  package (`createDataSet` twice on it; the workspace holds multiple datasets fine);
  (2) `testall.h` is moc-compiled standalone — `Json::Value` in a signature needs a
  forward declaration (`namespace Json { class Value; }`).

## 2. Decisions pinned during implementation (beyond D1–D11)

| # | Decision | Why |
|---|---|---|
| P1 | Promoted scale→level strings use canonical `'g'`-10 (`'.'`, no grouping) via `render_double` | Matches the caches' dictionary-canonicalization rule; display-stable |
| P2 | Ragged/unterminated/empty block → validation code `anchor` | Block-shape problems are anchor problems |
| P3 | Pre-edit cache rides `payload.source` | Zero wire change; symmetric with `data_open`; the lane's typed parse drops `dataset_paths` |
| P4 | `schema_change` rename updates both field name and `jasp:display_name` | v1 has no other name-identity consumer; views select by display name |
| P5 | Oversized inverse → visible `fatalError`, never truncation | §5 "the honest minimum"; stack cap is §11.4 |
| I1 | `DataWork.edit` boxed | clippy enum-size; open/view clones stay cheap |
| I2 | `target_schema: null` ≡ absent (parses as `None`) | D4 already defines them equal |
| I3 | Null spellings (incl. `""`) apply to block cells for ALL types | Conversion parity: the same spellings that nulled at open null again |
| I4 | Full-column inverse capture iff any touched column promotes | Uniform IPC row count; a promotion is whole-column anyway |
| I5 | `insert_block` always ships the schema | value_count/distinct move on every edit (§4 result note) |
| I6 | Value-levels change → `all:true` | The §6 table says so, literally |
| I7 | Rows ops' schema policy: `insert_rows` ships NONE (nulls move no count), `delete_rows` ALWAYS (value_count drops) | §4 "they change on every edit" means *when they move*; the dictionary never prunes so levels/distinct stay |
| I8 | Row-op geometry refusals use the `range` code (not `anchor`) | P2's `anchor` is about block SHAPE; `insert_rows` has `at ≤ rows`, `delete_rows` has the §3-explicit `at+count ≤ rows` |
| I9 | `insert_rows` inverse is metadata-only (`delete_rows` program, no bytes) | §5's honest bound: a structural no-data change has no old cells to restore; redo symmetry comes free at d7 (the delete computes its own restore blob) |
| P6 | `insert_cols` names uniquify against the full post-edit list via `unique_new_name`, which LOOPS until genuinely free (`_N`, N = 1-based position, then increments) | A name generator that can emit a duplicate is a bug; earlier inserts may own `score_3` already. Explicit `display_name` honored verbatim |
| P7 | A dataset must keep ≥ 1 column (`delete_cols` leaving zero → `range`); empty spec list → `range` | 0 rows is a real state (header-only CSV); 0 columns is degenerate — no schema, no grid, nothing renderable, and legacy agrees |
| P8 | Spec `levels` pre-populate the dictionary VERBATIM (`dict_from_list` — no sort, no canonicalize; spec order IS the level order); absent type = nominal (inference over zero cells); scale+`levels`, duplicates, or an unknown type string → `schema_mismatch` | Declared order is intent (ordinal's meaning); adhere-or-error for contradictions |
| P9 | d6 retype inverses by class: pure metadata (rename, nominal↔ordinal flag) → JSON-only; key remap (reorder/append) → JSON-only; physical re-encode (scale↔categorical) → full-column capture (the I4 pattern). Undo NEVER casts back | Casts are lossy both ways ('g'-10 drops bits; cat→scale collapses levels) AND the dictionary is not a function of the data (unused levels, chosen order) — only capture restores faithfully |
| P10 | Inverse transport v1 = whole-column LZ4 IPC inside the message; LZ4-vs-ZSTD study DEFERRED (common case ≪ P5 ceiling: ~20 MB compressed / ~100 MB uncompressed); disk-spill "probably never"; P5 stays the guard | Don't optimize before real data exists (design §10 ladder); §11.4 owns the stack policy |
| P11 | Labels are legacy's two mappings KEPT, not collapsed (neo-jasp.md §8): dictionary values = the DATA VALUES; `jasp:labels` = a sparse JSON value→label overlay in FIELD METADATA (entry only where label ≠ value — absent when identity, which is why CSV-open v1 never emits the key). Relabel = O(k) metadata edit (validated ~15–20× vs rebuild); reorder never touches the overlay (value-keyed, order-independent) | "The label is what you read; the value is what you compute with and what identifies a level" — presentation-only relabel survives with no sidecar (field metadata IS the schema). REVISED 2026-08-29: an earlier draft of this row wrongly claimed a labels≡levels collapse |
| P12 | Undo contract (d7a): `apply_inverse` refuses unknown `format`/malformed programs with Fatal (a corrupt blob is infrastructure, not a user error) but `base_revision > current` with `stale_edit` ValidationError; undo results always invalidate `all:true` (v1 honesty) and always ship the schema; every result carries its own inverse (redo) | Defensive-never-trust (§5); the all:true descriptor is the conservative drop — refinement is a later optimization, not a correctness need |
| P13 | `insert_block`'s declared `target_schema` (d6b): a **window-scoped POSITIONAL array of NULLABLE entries** — entry *i* binds to output column `col+i`; array length ≠ paste width → `schema_mismatch`. `null` ≡ undeclared for that column (exactly the no-schema behavior: covered columns absorb/promote, new columns infer per D5 + auto-name `V{j+1}`); a non-null entry is a strict **per-field** postcondition — declared fields adhere-or-error, absent fields take the auto path (ChangeEntry's absent=keep semantics, per-field granularity). `name` on a COVERED entry (one binding an existing column the paste overwrites) is optional but must echo the current field name when present (shifted-declaration guard); a covered entry declaring a type ≠ current → `schema_mismatch` in v1 (retype is `schema_change`'s job — two ops; EXTENDABLE later to atomic paste+retype, see §6); covered `display_name`/`labels` keys refuse (rename/relabel are schema_change's job). `levels` on a covered entry REPLACE per d6 Remap rules (coverage validation: `level_in_use` + the cap rule, against the POST-edit in-use set; verbatim lookup, never canonicalized — invariant 9); paste cells must exist in declared lists. A declared name colliding with a live column → `schema_mismatch` (a postcondition is never silently uniquified; `insert_cols`' P6 differs — its names are inputs, not assertions). Full-column inverse capture iff any touched column re-encodes (I4 widened from "promotes" to "re-encodes": promotion OR declared Remap) | §0's "complete post-edit column list" was written with `schema_change` (the whole-schema op) as the archetype — each op declares only its own degrees of freedom (`insert_cols` already declares per-new-column). Entry-null ≡ undeclared is I2's philosophy one level down (field-null ≡ absent). The nullable-window pattern was discussed pre-d1 but never recorded anywhere — recovered from memory + pinned 2026-08-29 |
| P14 | Backend sync (future era, not this one) is orchestrator-owned: watch source → reconvert via the data_open machinery → revision bump + `data_changed` (cause `external`). An external change is a FRESH RELOAD — **nothing survives**: no rebase of local edits, and the dataset's undo stack is **cleared** (dropped, not refused: every stored inverse blob is then `base_revision < current` — not the `stale_edit` refusal, but meaningless against reloaded data). Candidate later exception ONLY: the labels overlay — value-keyed (P11), it reattaches to surviving values and cannot conflict. Pinned in code: `DataSetSyncer`'s class comment + `DataSet::applyRevision`'s doc | "For now we simply say nothing survives" (user, 2026-08-31); levels/dictionaries cannot survive a reload (re-derived from the new source), labels are the one sane candidate |

## 3. Invariants — easy to break silently

1. **No copies**: `dataedit.rs` must not contain a `parse::<f64>` on a user string or a
   dictionary-build loop outside the csv2arrow helpers. Drift = the "two inference paths"
   bug class. The same for the wire-schema JSON (`column_info_json`) and field decoration
   (`jasp_field`).
2. **`read_range` boundary lesson**: process the batch that CONTAINS `to_row`
   (`if base < to_row`), not `bend > to_row` — the off-by-one silently zeroed pass A
   whenever the range ended on a batch boundary (every full read). If you touch range
   reads, re-run the promotion + growth tests first.
3. **v1 cache contract**: one dictionary per field, emitted once (first batch carries it;
   later batches share the values). `cat_dict_from_values` re-derives `canonicalize` from
   content — faithful because all-numeric content under an active locale canonicalizes at
   build. The IPC writer REJECTS dictionary replacement — never emit two different values
   arrays for one field (this is why tails carry the column's own dictionary on null keys).
4. **Design-correctness traps** that already bit once (as wrong test expectations):
   - an OVERWRITTEN value leaves the post-edit dictionary (promotion dropped "2.5"
     correctly — the data no longer contains it);
   - a block at col C does not touch columns < C (growth test asserted score wrongly);
   - appending a level IS a whole-dataset invalidation (I6).
5. **Orchestrator D6 strip**: the forwarded EDIT result carries identity + inverse only.
   Anything new for view-consistency must go through `data_changed`, not the result.
6. **Revision only climbs**; undo will bump it too. The only equality check is D11's echo
   (orchestrator) + the blob's defensive `base_revision ≤ current` at apply time (d7).
7. **One dictionary per field, across the whole file — including the engine's OUTPUT**:
   every piece of a field shares one values Arc (slices preserve it; null pieces carry
   the captured dictionary; BuildDict pieces carry the build dict; empty values only
   when the source itself had zero rows). The eager first-batch capture runs BEFORE any
   piece — a null fill preceding data would otherwise swap the dictionary mid-file. If
   you touch `stream_rewrite`/`emit_piece`/`null_pass_array`, re-run
   `insert_rows_at_the_top_and_end` + `chained_edits_keep_the_cache_contract` first.
8. **Old-row order = segment order** (the cursor's monotonicity assumption). Every op's
   segment table must consume input rows in stream order — `delete_rows`' removed window
   is SKIPPED (`skip_to`), never revisited. A new op that needs out-of-order reads needs
   a second cursor, not a bend of this invariant.
9. **The engine MOVES dictionaries, never re-sorts them.** A pass-through column's
   dictionary is the input's, verbatim (slices share the values Arc); a declared list
   (`dict_from_list`) is verbatim spec order — even when a fixture's `prebuild_dict`
   sorted at build time. First-appearance vs sorted order is decided ONCE at the
   dictionary's creation; edits never revisit it. (The d5 boundary test bit this: the
   "wrong" expectation was the trap.)
10. **emit_piece's old-value reads are PHYSICAL-TYPE-GUARDED** (d7a): a restore plan can
    pair an f64 build with a dictionary source column (undoing a promotion) — the guard
    makes such reads `None` instead of panicking; the restore window supplies the cells.
    If you add a ColBuild variant, guard its source reads the same way.
11. **A capture ALWAYS carries the pre-edit dictionary** — even at zero captured rows
    (the values slot of a 0-row dictionary slice). An empty-dictionary blob silently
    restores cells but leaves an absorption-grown dictionary behind (d7a bug #2). The
    d3 `pre_edit_values` stash exists for exactly this.
12. **The round-trip harness grows with every program**: any new edit op or restore
    program gets a `round_trip` scenario (both identities) in the same session. It has
    already caught three real bugs that field-level asserts waved through — that is the
    standard, not an optional extra. **d8 extension**: the REAL-LANE crown is the same
    discipline one level up — it caught the orchestrator's dropped work tails (the
    mock-lane tests send JSON-only frames and could never see it).
13. **Work-frame tails are load-bearing on BOTH directions** (d8): a frontend→runner
    work frame's binary part (an edit's §1.2 TSV cells, an `apply_inverse`'s IPC bytes)
    forwards VERBATIM — `FrontendData` carries it, `dispatch_work` re-frames with
    `frame_parts`, and PARKED work keeps it (`ParkedWork.tail`). If you touch the work
    relay, re-run `edit_undo_redo_crown_over_the_real_lane` — a dropped tail reads as
    "the edit block is empty" at the lane, silently degrading every edit.

## 4. Next: the REMOVAL plan (Phase B accelerated) + the remaining (e) features

**The pinned direction (2026-08-31, the user's call): STOP patching the legacy view
machinery — five fixes went into code whose designed fate is deletion (§8's Phase B).
Patches only where something is actively on fire; the cure is removal.** The removal
work, ordered by safety and payoff (audit grounded 2026-08-31):

- **R1 — lane datasets never sync (small, do first; the CPU suspect).** The FileMenu
  open path starts legacy syncing alongside the NEO open: `FileMenu::setCurrentDataFile`
  L207 (`ds->syncer().startFileSyncing(path)`), `FileMenu::setDataFileWatcher` L238,
  `MainWindow::dataSetIOCompleted` L1985/L1996 (`startFileSyncing`/
  `startDatabaseSyncing`). Gate each on `!ds->isOpen()` (lane-bound datasets skip).
  Kills the file watcher, the periodic re-reads/hashes of a huge source CSV (the
  sustained-CPU suspect), the sqlite interval machinery, and the cross-thread
  QObject warning. The (c) syncer tests cover the LEGACY path — they must stay green
  (they use legacy-loaded fixtures, so the gate should not affect them — verify).
- **R2 — kill the legacy Column mirror (audit DONE 2026-08-31; four-step plan pinned).**
  The mirror (`landWireSchema` L2320-2322, grow-only) exists to serve the variable
  editor's legacy paths — and those paths DIVERGE silently on lane datasets (mirror
  edited, schema untouched, grid never shows it). Pinned decisions: the variable editor
  gets a ColumnInfo ADAPTER, not a gate ("make sure nothing uses the old shit"); the
  mirror's fate is DELETION (stage 1 gate now, stage 2 physically delete once zero
  lane-path readers remain). The four steps, in order:
  1. **Write-routing** (the divergence hazards) — **LANDED 2026-08-31** (uncommitted):
     rename — the editor's Name AND "Long
     name:" fields → `schema_change` (d6 Keep-class; legacy title == wire display_name,
     P4, so one op covers both; JSON-only inverse, invalidation `{}`); new column — the
     `_virtual` commit (`setColumnNameQ` — was NO null guard: inserted ghost Columns
     the grid never sees) → `insert_cols` with `NewColumnSpec{name, type}`; the
     copy-columns gesture (`DataSetView::_copy` → `serializedColumn`) → guarded (null
     serializations never pushed; plain cell copy works). New builders
     `DataEdit::schemaChangeRenameOp` + `insertColsOp` (dataedit.{h,cpp}); NEO branches
     in `setColumnNameQ`/`setColumnTitle`/`setColumnType` (the variables-window dropdown
     was a THIRD type-switching surface still on `SetColumnTypeCommand` — now the same
     `schema_change` op as the proxy path); gates on description/dropLevels/hasLabels/
     computed-type/filter/createComputedColumn (each logs the never-swallow line for why);
     `isColumnNameFree` serves the schema on lane (GridModel's twin). Identity note: the
     chosen column resolves by SCHEMA INDEX (`chosenColumn()` is the mirror index; the
     mirror is grown in schema order but never renamed, so names go stale after NEO
     renames — the index correspondence holds). Runnable C++ set 16/16 after.
  2. **Gates on the no-wire-yet controls**: description (zero `description` in
     orchestrator/src — C++ ColumnInfo reads a key the lane never emits; support later
     as `jasp:description` field metadata + a schema_change entry key, folded into the
     labels-editor slice — same metadata family as jasp:labels), dropLevels (the lane
     dictionary never prunes ≡ keep; meaningless until the analyses era), "Use labels"
     (hasLabels — NEO semantics = the jasp:labels overlay, B2; when built, enable off
     `distinctCount` vs WIRE_LEVELS_CAP — legacy has NO threshold, we do), computed
     type/filter (future era). NB: with the adapter making `column()` null on lane,
     most of these self-neutralize via their `if(column())` guards — the virtual branch
     is the exception (routed in step 1).
  3. **The adapter** — **LANDED 2026-08-31** (uncommitted, with step 1): ColumnModel serves
     reads from `schema()` when `isOpen()`. New `laneSchemaColumn()` helper (chosen column
     by SCHEMA index — `chosenColumn()` is the mirror index, and the mirror is grown in
     schema order, the correspondence the adapter relies on); schema-backed
     `columnNameQ`/`columnTitle`/`currentColumnType`/`columnDescription` (honest "" until
     the wire carries description)/`hasLabels` (false until B2). New Q_PROPERTYs
     columnName/columnTitle/columnDescription/hasLabels + the QML rebinds
     (ColumnBasicInfo name/Long-name/description, VariablesWindow's "Use labels" checkbox)
     — the fields previously bound `columnModel.column.*`, serving STALE MIRROR names: a
     NEO rename landed but the editor kept showing the old name, the user retyped, and
     the retypes raced (see the §6 RACE bug). `setChosenColumnByName` resolves by SCHEMA
     name on lane (a failed MIRROR lookup silently turned the editor into a virtual
     "new column" — a renamed column couldn't be chosen anymore). And the refresh hook:
     `DataSet::schemaChanged` never reached ColumnModel (only GridModel/ColumnsModel
     listen to it; the legacy chain runs on datasetChanged) — shownDataSetChangedHandler
     now also connects schemaChanged → laneSchemaRefreshed (refresh + notifyColumnChanged).
  4. **The mirror gate + canary** — **LANDED 2026-08-31 (uncommitted, with steps 1+3):
     THE MIRROR IS GONE.** `landWireSchema` no longer creates legacy Columns for lane
     datasets; `columnCount()` serves `schema()` when `isOpen()` (was the grow-only
     mirror count — stale after delete_cols); `ColumnModel` needs no mirror on lane:
     `laneSchemaColumn()` reads `_columnIndex` (maintained by the choose paths —
     setChosenColumn stores the view's schema-order index, setChosenColumnByName's NEO
     branch stores schemaColumnIndex), `chosenColumn()` serves it, `setChosenColumnByName`
     resolves by schema name with `_column = nullptr` (labels/computed stay inert-by-null
     — gated future eras). CANARY: `testLaneDatasetsHaveNoMirrorColumns` — after applySchema
     AND after a rename+drop revision: `columns().empty()`, `column(...)` nullptr,
     columnCount == schema size. Runnable set 17/17. **What still reads `column.*` in QML:
     ComputeColumnWindow + LabelEditorWindow only (gated: computed/labels eras; their
     ternary guards handle null).** The phantom-rename guard also landed: the Name/Long-name
     fields' `editingFinished` fires on plain FOCUS-OUT (clicking another column) with the
     previous column's text still in the field — it renamed the NEWLY CHOSEN column to the
     previous one's name (the smoke log's w3: col_7 → "x" the instant the user switched;
     the engine uniquified field names to x_6/x_8, hence the garbage). Fix: a
     `textEdited`-set dirty flag per field — only a real user edit may submit
     (ColumnBasicInfo.qml). The same phantom exists for legacy datasets' description field
     (TextArea, untouched — same pattern applies if it ever bites).
     **Step-4 followup bug (found by the user's smoke, fixed same day):** removing the
     mirror left `_virtual = !chosenColumn` in setChosenColumnByName — chosenColumn is
     ALWAYS null on lane now, so every clicked column opened as the empty "new column"
     form and typing a name INSERTED one. Fix: `_virtual = !chosenColumn &&
     laneSchemaIdx < 0`. Lesson pinned: ColumnModel was GUI-side and completely untested —
     canary 2 now exists (`testLaneColumnModelServesSchema`: choose-by-name is non-virtual
     + serves the schema's name/index/type; the CLICK path setChosenColumn(int) resolves
     via the schema (the legacy responder `DataSetTableModel::columnName` — MainWindow's
     `columnNameForIndex` connection — reads the EMPTY legacy model on lane and the ""
     fallthrough opened the virtual form for every click: the SECOND shipped step-4 bug);
     an index at the extent stays virtual (the "+" slot); a rename landing on the CHOSEN
     column is served by the same choice — the refresh hook; an unknown name IS virtual).
     Runnable set 18/18.
     **Infection era, fully closed (three layers, found by the user's smokes):**
     (1) `editingFinished` fires on plain FOCUS-OUT with the previous column's text —
     guarded by `textEdited`-set dirty flags (ColumnBasicInfo). (2) VariablesWindow's
     `onBeforeChangingColumn` force-committed ALL FIVE fields on EVERY switch — now
     dirty-flag-guarded (name/title, aliased up from ColumnBasicInfo) + equality-guarded
     (description); type/computed have C++ equality guards. (3) THE READ SIDE —
     `notifyColumnChanged()` never emitted `columnTitleChanged`/`columnDescriptionChanged`/
     `hasLabelsChanged`, so those fields NEVER REBOUND on a switch and kept serving the
     previous column's values (why "name fixed but longname/description not") — now
     emitted. Plus `setColumnTitle`'s NEO branch got the missing equality guard (a no-op
     force-commit would otherwise mint a spurious revision).
  Audit findings (lane-path mirror readers): ColumnModel (the big one), `columnCount()`
  itself, `ExpandDataProxyModel::serializedColumn` L636, `DataSetPackage::refreshColumn`
  L350 (null-safe already). Legacy-only rails (NOT lane-reachable, no work): undostack's
  Column commands, DatabaseInterface, Importer::syncDataSet, the computed-column
  machinery (naturally dormant — no analyses on lane data; NEO guard already at
  analysis.cpp L990).
- **R3 — delete the bypassed caches' dead branches** (cosmetic, last): after R2, the
  `_storedDisplayText`/`_storedLineFlags` legacy branches and the `DataSetTableModel`
  edit paths are dead on the lane route; remove when comfortable.
- **Remaining (e) features (after or interleaved with the removal):** `setColumnName`
  rename → `schema_change`; row/col insert-delete gestures → their ops
  (`insert_rows`/`delete_rows`/`insert_cols`/`delete_cols` — all served by the lane
  since d4/d5); range-aware invalidation instead of the whole-view restart; the paste
  dialog's `target_schema` declarations; the labels editor (B2). The undo byte cap is
  DONE (§1e3).

## 5. Validation + environment quirks

```bash
cd orchestrator
cargo test                       # 91 (data-runner) + 44 (orchestrator) + 11 (dataset_e2e) — ALL GREEN
cargo clippy --all-targets       # clean (keep it that way on every touched target)
cargo run --bin jasp-orchestrator -- --schema > messages.schema.json   # regen after wire changes
                                  # (d4–d8 changed NO serde wire types)
# C++ (the runnable set; see the breakage note below):
QT_QPA_PLATFORM=offscreen build/Desktop_Qt_6_11_0-Debug/Tests/JASPTest testSavLabels \
  testSyncerStartStopFileSyncing testSyncerFileChangeEmitsSignal testSyncerStartStopDatabaseSyncing \
  testSyncerSyncNowWithoutDataSource testSyncerMultipleStartStop testSyncerReleasesSyncGuardOnCompletion \
  testSyncerRetriesFileChangeMissedDuringSync testUndoColumnDropLevels \
  testLaneRevisionLandsRowsAndSchema testLaneRevisionIgnoresStalePushes testLaneRevisionSchemaSwap \
  testLaneRevisionRowGrowthWithoutSchema testLaneRevisionOutOfOrderPushes   # 16/16 green
```

- 2026-08-31 (big-data + undo-cap session): the five view fixes + the undo byte cap
  (§1e3); runnable C++ set **16/16** (the undo regression test joined); app + release
  orchestrator rebuilt. UI-verified by the user on terror_tall (30M×8): load survives,
  memory "much much better" but STILL creeps on scroll + CPU sometimes high → §4 R1.
- 2026-08-31 (seam session): (e) slices 1–2 UI-verified; the two seam bugs (§1e);
  runnable set 15/15 at that point.
- 2026-08-29 (d8 session, unsandboxed): full suite green — 91 + 44 + 11 = 146 on real
  sockets; clippy clean; release binaries rebuilt WITH d8 (orchestrator included — the
  tail relay changed).
- 2026-08-29 ((c) session): C++ `JASPTest` — the five new lane scenarios + sav-labels
  + all seven syncer tests: **15 passed, 0 failed** (`QT_QPA_PLATFORM=offscreen` — no
  xvfb on this box; `CCACHE_DISABLE=1 CCACHE_DIR=/tmp/ccache`, build dir
  `build/Desktop_Qt_6_11_0-Debug`). **PRE-EXISTING environment breakage (verified on the
  pristine pre-era tree `bfd42a515`): `testDataImport` CSV/TSV hardcoded-JSON
  mismatches and `testJaspDataImport`/`testFilterLabels` crashing on a
  `DatabaseInterface::dataSetName(-1)` assert** — the .jasp/DB-loading path is broken
  in this environment, unrelated to the era. A full-suite run therefore ABORTS at
  `testJaspDataImport`; run slots individually past it.
- 2026-08-29 (d6b session, unsandboxed): full suite green — 91 + 44 + 10 = 145 on real
  sockets; clippy clean; release binaries rebuilt WITH d6b.
- 2026-08-29 (d7b session, unsandboxed): full suite green again — 84 + 44 + 10 = 138 on
  real sockets; release binaries rebuilt WITH d7b.

- **2026-08-29 (d4 session, unsandboxed run): the FULL suite passed on the real sockets** —
  `dataset_e2e` 10/10 and `routing_over_ipc` green for the first time since d1–d4 landed.
  The dev-sandbox AF_UNIX quirk below still applies to sandboxed runs; use `unsandboxed`
  (or a real machine) whenever ipc-dependent suites are in play.

- **Sandbox quirk (dev box)**: AF_UNIX socket creation is EPERM → `routing_over_ipc` and
  `dataset_e2e` fail under the sandbox for environmental reasons (proven on pristine
  HEAD). RESOLVED for d4 by running unsandboxed (see above) — the quirk remains for
  plain sandboxed runs. Release binaries rebuilt 2026-08-29 WITH d4.
- C++ build: `CCACHE_DISABLE=1 CCACHE_DIR=/tmp/ccache cmake --build
  build/Desktop_Qt_6_11_0-Debug --target JASPDesktopLib` (ccache's cache dir is read-only in
  the sandbox). `jasp-build/` is STALE (configured from `/work`) — don't use it.
- Release binaries were built: `orchestrator/target/release/{jasp-orchestrator,
  jasp-data-runner}`. NB: `--version` is not a flag — the binary starts the broker.

## 6. Open items / known debts

- **THE RACE — FIXED 2026-09-01 (the edit chain)**: two edits dispatched at the same base
  before either completes used to mint the SAME revision twice and write the SAME output
  path (last writer wins, ladder desyncs, later edits fail count-match). The fix is the
  user's design: **the orchestrator serializes — ONE edit in flight per dataset, extras in
  a per-dataset FIFO** (`Router::queued_edits` / `QueuedEdit`), because each edit's SOURCE
  is the previous edit's OUTPUT (the chain dependency — true concurrency is impossible;
  two counters would only fix the symptom). Mechanics: the dispatch arm (after D11, which
  still proves the edit ARRIVED in order against realized state) enqueues + sends the
  §25.5 `running` marker; EVERY terminal edit result (success AND failure) drains —
  popping, RE-STAMPING the base to the realized revision (env.body AND the Work param —
  identity in the frame), and dispatching (fresh mint + fresh path; the lane validates the
  op content against live data — count-match/ranges — the authoring-context check where
  the data lives). Views never queue (they read realized + self-heal on data_changed);
  different datasets never block each other. Edges: a retry of an in-flight work_id is
  re-acked (never a duplicate application); no lane at drain → park_work (provisioner
  re-spawns; try_dispatch_parked continues the chain) or a visible no-runner failure; a
  dataset dying (failed open / lane-evicted open) FLUSHES its queue with stale_edit-shaped
  refusals (never silent loss); a lane eviction under an in-flight edit drains the chain.
  CROWN: `edit_chain_queues_concurrent_edits_per_dataset` (A dispatched, B → running
  marker + the lane stays quiet, A completes, B dispatches re-stamped to 1 minting a
  DIFFERENT path with A's output as source, ladder ends at exactly 2, C dispatches
  immediately — not wedged; also taught the mock to echo re-stamped result revisions,
  the §23 stale guard earning its keep). Suite 147 green; clippy clean; release binaries
  rebuilt. C++: `DataEditCommand` ignores `running` markers (the client invokes handlers
  for non-terminal results too — the failure log would have fired on every queued edit).
  KNOWN residual (accepted v1): a positional op queued behind a geometry-changing edit
  re-stamps and lands at shifted coordinates — refusal via intervening invalidation
  descriptors is a later refinement if it ever bites.

- **THE RACE (found 2026-08-31, the rename smoke test — REAL)**: ~~UNFIXED~~ **FIXED
  2026-09-01 — the edit chain; see §6's first bullet for the full design.**

- Design §11 items stand (mirror-collapse detail, range-aware invalidation on the
  FRONTEND — the descriptor is normative and now computed correctly, map-to-null level
  deletion, undo stack memory cap).
- **Undo-stack cap policy** (§11.4; user leaning 2026-08-29): byte-based, ~250 MB warn
  threshold, then DROP OLDEST-first. Decide from RSS once real edits exist; nothing to
  build until (e).
- **LZ4 vs ZSTD for inverse blobs**: deferred (P10) — measure on real columns first.
- **Backend sync (future era)**: orchestrator-owned (watch source → reconvert → revision bump +
  `data_changed` cause `external`); policy P14 — nothing survives a reload, undo stack cleared,
  labels overlay the candidate later exception. The legacy `DataSetSyncer` stays for legacy
  datasets until that lands; lane datasets get NO sync (R1 gates the starts).
- **Labels-vs-levels (P11, revised)**: the two-mappings model stands (dictionary values =
  data; `jasp:labels` = sparse field-metadata overlay) and d6 implements it. STILL OPEN,
  in rough order of need: (1) the VIEW lane renders dictionary values today — it must
  apply the overlay when rendering cells once labels can EXIST in real data (guard on
  present-and-non-empty, the validated sparse skip; until then labels-only changes are
  invisible in views, which is why their invalidation is `{}`); (2) paste parse-back into
  a labelled column: does typing a LABEL resolve to its VALUE? (leaning: accept both,
  labels as synonyms); (3) legacy per-label `description` → `jasp:description` field
  metadata; (4) SPSS/excel imports map natively (values in the dictionary, labels in the
  overlay).
- **0-row caches cannot carry dictionaries through the file** (v1 contract: dictionaries
  ride batches): `insert_cols`' declared levels on a zero-row dataset vanish on the next
  edit's read. Fix options: emit one empty batch to carry the dictionaries, or move
  levels to field metadata. Low priority (empty datasets rarely carry declared labels).
- Pass-A reads from row 0 (no `set_index` skip) — when `row ≥ N` without promotion it reads
  the whole file just for first-batch dictionaries. Optimize with the footer block map
  (`CacheSchema.block_rows` is kept for exactly this) if it ever matters.
- `ViewFiller` still sends `Work.revision = 0` for views — correct (views are not D11-
  checked) but worth a comment when (e) lands.
- The C++ mirror growth-only limitation (§1b) until Phase B.
- `distinct_count` for scale columns reports `DISTINCT_CAP` when capped ("at least") —
  matching conversion semantics; don't "fix" it to an exact number.
- **d6c candidate (extension, not debt)**: covered entries declaring a RETYPE refuse in
  v1 (P13) — widening to d6's re-encode classes (`ToScale`/`ToCategorical` + paste
  coercion into the declared post-type) would make the legacy "convert this column?"
  paste dialog ONE atomic op. Nothing calls for it until (e)'s paste UX does; the
  machinery already exists (d6), so it is a scope decision, not a design problem.
