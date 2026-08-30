# Data Edit — Design (wire ops, undo contract, data_changed, edit-era consequences)

**Status:** rev 4, 2026-08-28. Decisions frozen before implementation (merge-multidataset.md
discipline: frozen decisions are not relitigated; open items are §11). Scope: the `data_edit`
op family on the wire, the undo contract, the `data_changed` push (Increment 4), type/label
resolution, and the consequences the edit era forces (proxy surface, metadata mirror).
**Not** in scope: the derivation family (§9 — deferred, wire slot decided), orchestrator
format expansion, the SQLite strip (HANDOVER-multidataset-fold.md §7 order stands).

**Read first:** `HANDOVER-multidataset-fold.md` §6 (the brief this implements),
`data-view-format.md` §1.2 (the TSV grammar forward edits reuse),
`orchestrator/src/messages.rs` (the implemented catalog these ops extend),
`orchestrator/src/csv2arrow.rs` (`jasp_column_names`, `WIRE_LEVELS_CAP`, inference),
`CommonData/undostack.h` (the legacy command vocabulary being replaced).

## 0. Vocabulary (used throughout)

| Term | Meaning |
|---|---|
| **edit op** | One operation inside a `data_edit` work unit (`insert_block`, `delete_rows`, …). |
| **inverse** | The **opaque, session-bound blob** returned by the lane that exactly undoes a just-applied edit. The frontend stores it verbatim and resubmits it verbatim — it never interprets, merges, or serializes it. |
| **declared schema** | An optional `target_schema` on an edit: the complete post-edit column list, as the frontend intends it. Absent → the lane recomputes. |
| **absorption** | The lane's default when no schema is declared: new values that fit the column's current type just extend it (levels get appended). |
| **promotion** | A type change forced by data (scale column receives text → becomes nominal). Only ever targets nominal. |
| **invalidation** | The descriptor on `data_changed` saying which rows are stale (`{rows_from[, rows_to]}`, `{all:true}`, or `{}`). |
| **revision** | An opaque stamp: received, compared for staleness, echoed — **never reasoned about, never resubmitted**. |

## 1. Frozen decisions

| # | Decision |
|---|---|
| D1 | **Undo = frontend stack, backend-computed inverse.** Every edit result carries the exact blob that undoes it. The lane is the only data holder, so the lane computes the inverse. |
| D2 | **Wire shape = nested.** `op:"data_edit"` + adjacently-tagged `edit:{op:…}` enum. No flattening into `DataOp`. |
| D3 | **Op set (v1):** `insert_block`, `insert_rows`, `insert_cols`, `delete_rows`, `delete_cols`, `schema_change`, plus `apply_inverse` (the undo/redo entry point, §5). User-facing ops stay the first six unless a legacy undo command requires more. |
| D4 | **`target_schema` is optional.** Absent → lane recomputes (absorption/promotion, §4). Present → adhere-or-error (`validationError`, atomic). |
| D5 | **`type:null` = lane infers** (csv2arrow path, dataset's stored ingest params). The frontend never re-implements inference (no double classes). |
| D6 | **Results carry undo material; `data_changed` carries view-consistency material.** Edit results = identity + inverse + validation detail. Schema/rows/invalidation ride `data_changed` (§6). |
| D7 | **Derivations (transpose, long↔wide) are a separate deferred family** — new dataset id, not edits (§9). |
| D8 | **`data_update` stays the derived-data write path** (computed columns/filters → cache), distinct from `data_edit`: not undoable, not user edits. |
| D9 | **Mirror collapse direction: `schema()` is the metadata holder**, fed by the wire (§8). |
| D10 | **The inverse is an opaque, session-bound, versioned blob.** v1 format = edit-op metadata (JSON) + the old cells as **Arrow-IPC** in the frame's binary part — machine-exact, typed, LZ4_FRAME-compressed, never rendered through the lossy display grammar. Future optimizations (lane-side tokens, overlay references) are new `format` values inside the same slot — never a contract change. The frontend treats `(meta, bytes)` as one inseparable unit. |
| D11 | **`Work.revision` on an edit = the base dataset revision, echoed back to the orchestrator; the orchestrator checks it** (mismatch → `validationError stale_edit`). Echo-only — the frontend never interprets it (§0). The inverse blob carries its own lane-embedded base revision, checked by the lane at `apply_inverse` time. |

## 2. Where it sits on the wire

`DataWork` gains one optional adjacently-tagged `edit` field; defaults keep `data_open`/
`data_view` shapes valid (the established pattern — `row_offset`/`render` ride the same way).
Serde-derived in `messages.rs`, so an op-field typo is a compile error.

```jsonc
// frontend → orchestrator → lane
{ "type":"work", "work_id":"w-12", "revision":6, "dataset_ids":["ds-4"],   // revision = base (D11)
  "kind":"data", "payload":{ "op":"data_edit",
    "edit":{ "op":"insert_block", "row":1234, "col":2,
             "target_schema":null },        // absent/null = lane recomputes (D4)
    "source":"", "cache_path":"", "format":"",
    "ingest":{…} } }                        // orchestrator injects cache_path as for data_view
// frame binary part: the block's cells, §1.2 grammar (escaped TSV, \N, dictionary cells = value strings)
```

- **Capability:** one entry — `Capability::Data{ op:"data_edit", formats:null }`. One
  capability covers the family; the lane that owns the cache serves all edit ops.
- **Forward edit data** rides the binary part in the **§1.2 TSV grammar** — symmetric with
  `data_view` ("the edit path parses the same display string back, exactly like legacy").
  Parse locale = the dataset's stored `IngestParams` + the request separators. Deliberately
  lossy beyond display precision, exactly like legacy typing.
- **Inverse data** rides the binary part as **Arrow-IPC** (D10) — a different encoding for a
  different author: forward data is frontend-authored (display strings), inverse data is
  lane-authored (machine-exact). The asymmetry is the design.
- **Division of labor (unchanged):** frontend declares intent and owns stack ordering; the
  lane validates, applies, and computes the inverse + invalidation; the orchestrator mints
  identity/revision, checks the base revision (D11), and broadcasts `data_changed`.
- **Edit result** (`DataResult`, slim per D6):

```jsonc
{ "dataset_id":"ds-4",
  "dataset_revision":7,          // the NEW revision post-apply — views stamp at dispatch,
                                 // edits stamp post-apply. Different semantics, documented.
  "inverse":{                    // the BLACK BOX (D10) — opaque to the frontend
      "format":"arrow_ipc_v1",   //   the only field the frontend may observe (log/size)
      "base_revision":6,         //   lane-filled, lane-checked on apply — echoed blindly
      "ops":[…] },               //   v1 internals: op kinds, anchors, schema fragment (JSON)
  "validation":[…] }             // iff status:"validationError" (§3)
// frame binary part: the inverse's Arrow-IPC payload (old cells, exact, compressed)
```

## 3. The ops

| `edit.op` | Semantics | Legacy command (`undostack.h`) → op |
|---|---|---|
| `insert_block` | **Paste:** anchor `(row, col)` + block in binary part. Overwrites covered cells. Extent grows to `max(current, anchor+shape)`; holes → null; anchor may name not-yet-existing rows/cols. Paste overflow creates columns (infer per D5 unless declared; named per `jasp_column_names`, overridable via `target_schema`). | `SetDataCommand` (1×1 or coalesced) |
| `insert_rows` `{at, count}` | **Shift down**; null fill. | `InsertRowsCommand` |
| `insert_cols` `{at, column}` | **Shift right**; one column spec = an open-schema column entry (`{name, display_name, type:…\|null, levels?}`). | `InsertColumnCommand`, `InsertColumnsCommand` |
| `delete_rows` `{at, count}` | Shift up. `at+count ≤ rows` else validation error. | `RemoveRowsCommand` |
| `delete_cols` `{at, count}` | Shift left. | `RemoveColumnsCommand` |
| `schema_change` `{target_schema}` | **Metadata only:** rename, retype, set/reorder levels, reorder columns. **Never changes the column set or row count** — structural changes go through insert/delete ops, so there is exactly one way to say "add a column". | `SetColumnTypeCommand`; the label family (`AddLabel`, `SetLabel`, `SetLabelOriginalValue`, `DeleteLabel`, `MoveLabel`, `ReverseLabel`) — levels ride the post-schema |
| `apply_inverse` `{inverse}` | Submits a previously returned inverse blob verbatim; the lane validates (format known, embedded base revision acceptable) and applies atomically. The undo/redo entry point; not user-facing. Result carries its own inverse (redo material) — symmetric. | — (replaces undo()/redo() bodies) |

`insert_block` vs `insert_rows` is **overwrite/expand vs shift** — both real UX, not the same
op. Cell edits are `insert_block` 1×1, batched at commit boundaries (§5).

**Atomicity (all ops):** two-phase — validate everything (anchor sane, base revision matches
(D11), all cells parse under the resolved type, every dictionary value exists in the resolved
levels), then apply. Failure = `status:"validationError"`, revision unchanged, nothing
applied, no inverse, no `data_changed`. `apply_inverse` validates format + base revision the
same way, then applies as one unit.

**Validation detail** (rides `DataResult.validation` when status is `validationError`):

```jsonc
"validation":[ { "column":"score", "code":"coercion", "message":"'abc' is not a number",
                 "count":4096, "rows":[12,13,14] } ]   // rows: ≤10 example indices
// codes: anchor | range | coercion | level_unknown | level_in_use | schema_mismatch | stale_edit
```

## 4. Type & label resolution

The schema param is the **declarative postcondition** — it turns "recalc types, check label
maps" into validation. The frontend declares intent only when the UX demands a specific
outcome (user set the type, label editor, "add empty scale column"); otherwise it omits it.

**No declared schema — the lane resolves, by two rules:**

1. **Absorption keeps the type.** Values that parse under the column's current type extend
   its levels: appended in first-appearance order; ordinal appends at end only. Works beyond
   `WIRE_LEVELS_CAP` — the lane owns the untruncated dictionary internally.
2. **Promotion targets nominal, only.** A scale column that can no longer parse (received
   text) becomes nominal — the merged column re-inferred csv2arrow-style, old numeric values
   becoming level strings.

Never automatic: categorical→scale, anything→ordinal. Both are lossy intent decisions and
require a declared schema (coerce-or-error).

**Declared schema — adhere-or-error:** the post-schema is the ordered array (position encodes
where new columns land); every dictionary cell must exist in the declared levels; every cell
must coerce; `type:null` = infer (D5).

**The level cap, plainly.** A nominal column can have far more distinct values than the wire
will enumerate: the schema's `levels` list is capped at `WIRE_LEVELS_CAP` (it feeds dropdowns
and the label editor; `distinct_count` stays exact). Since a declared levels list
**replaces** the whole list, declaring one for a column beyond the cap would mean replacing
a list the frontend has never seen in full. Rule: **you may only rewrite the full label list
of a column whose labels you could have seen in full** (`distinct_count ≤ WIRE_LEVELS_CAP`,
else `schema_mismatch`). Absorption (above) has no such limit.

**Deleting a level whose values are present → `level_in_use`** (explicit map-to-null variant
is a later op knob, not v1).

**Result:** the canonical schema with recomputed `value_count`/`distinct_count`/
`all_integer`/levels rides `data_changed` (§6) — the form constraint gates re-read exactly
these, and they change on every edit.

## 5. Undo (the contract)

```
edit applied → result{ inverse: <blob> }            // lane-computed, authoritative, opaque
             → QUndoCommand{ opSubmitted, inverse } // stores (meta, bytes) as one unit
undo = submit data_edit{ op:"apply_inverse", inverse: <blob verbatim> }
redo = submit the blob returned by THAT application // symmetric, same black box
```

- **Opaque by contract (D10).** The frontend stores verbatim, resubmits verbatim, never
  interprets, merges, or serializes. Extensions (lane-side tokens, overlay references) are
  new `format` values later — the contract cannot change. The blob is session-bound: it dies
  with the stack (in-memory, per-dataset, never persisted) — no lifetime coupling, no
  cleanup protocol.
- **Inside the v1 blob (lane internals, non-contractual):** the op(s) that restore state —
  undoing `delete_rows` is genuinely multi-step (re-insert rows, then fill old cells);
  undoing a growing paste is overwrite-then-trim — plus the old schema fragment and the old
  cells as Arrow-IPC (exact; a lossy display-precision encoding would corrupt f64 values on
  restore — that is why the inverse is NOT TSV).
- **Stack-side coalescing is dead by design** — merging blobs would mean interpreting them.
  Coalescing lives at the **capture side**: the proxy submits cell edits at commit
  boundaries (enter, focus-out, paste), which we want anyway since every submission is a
  wire round trip + revision bump + `data_changed` broadcast. Undo-menu labels come from the
  *forward* op (frontend-authored); the blob is never read for UX.
- **Sound because the stack is strict LIFO:** by the time N is undone, state ≡ pre-N, so the
  inverse computed back then applies exactly. **Revision never rewinds; it only climbs** — it
  is a staleness stamp, not undo identity. Safe against derived recomputes bumping revisions
  off-stack (users edit source columns; derived recompute touches derived columns).
- **Bounded by the change, not the dataset.** A cell edit's inverse is one cell plus the
  affected columns' old schema fragment; a paste's inverse is ≤ the paste. The pathological
  case — deleting a whole column on 30M rows — produces a large inverse because the change
  itself was large; that is the honest minimum for exact undo. A stack memory cap trims
  oldest-first if it ever matters.
- **Legacy contrast worth recording:** `SetDataCommand` stores `_oldValue`/`_oldLabel`
  frontend-side because legacy held the cells. That dies — the frontend no longer holds cells
  outside the viewport; the backend inverse replaces the captured old state.
- **Not on the stack:** `data_update` (derived recompute, D8), derivations (D7), label-filter
  state (§8).

## 6. `data_changed` (Increment 4 — the prerequisite return leg)

**The split (D6): results carry undo material; `data_changed` carries view-consistency
material.** One uniform buffer-invalidation path for every revision bump, whatever caused it.

```jsonc
{ "type":"data_changed",                 // unsolicited push on the data channel (like `modules`)
  "dataset_id":"ds-4", "dataset_revision":7,
  "rows":50001,                          // always — the new total
  "schema":[…],                          // present IFF the schema changed (the lane decides)
  "invalidation":{ "rows_from":1234 }    // always — the lane computed what is stale
                                       |  { "all":true }
                                       |  { "rows_from":1234, "rows_to":1300 }   // rows_to exclusive
                                       |  {}                                      // schema-only (rename)
  "cause":{ "kind":"edit"|"derived"|"external", "work_id":"w-12"? } }  // diagnostics only — never load-bearing
```

- **The lane computes invalidation once** (it knows what changed); the orchestrator stamps
  identity and broadcasts to every frontend holding the dataset — including the editor. The
  editor's command callback handles only the result (stores the inverse); the buffer/view
  handles only `data_changed`. No dedup logic, no special cases.
- **Invalidation table (normative):**

| Situation | Descriptor |
|---|---|
| `insert_block`, extent unchanged | `{rows_from: r, rows_to: r+n}` (the covered range) |
| `insert_block` extending rows | `{rows_from: r}` (to end — includes the new rows) |
| `insert_rows` / `delete_rows` | `{rows_from: at}` (everything below shifts) |
| any column-set / order / type / value-levels change | `{all:true}` |
| rename-only | `{}` (headers change via `schema`) |

- **Ordering & idempotence:** per-dataset, `data_changed` arrives in revision order, after the
  triggering result, on the same channel. Frontend rule: `revision ≤ current` → ignore.
- **In-flight views interoperate for free:** chunks are accepted only on exact revision match
  (the inc3 stale-identity rejection) — a chunk dispatched at rev 6 landing after a rev-7
  bump is dropped; the filler refetches.
- **Triggers:** edits now; `data_update` (filter/computed recompute) later — same rails;
  eventually external sync (`cause:"external"` — the legacy DB-interval polling subsumes here).
- **v1 frontend implementation may still drop the whole buffer** (sliding mode makes the
  refetch ~2 MB urgent-class); the descriptor makes range-aware invalidation a drop-in later.

**Build order (each step merges green):**
(a) `messages.rs` variant + orchestrator broadcast at the revision-bump point (filled from
the lane's result fields) — no real trigger yet; (b) frontend: JaspClient needs a new
dispatch path (`data_changed` has no `work_id` — the completion-lambda routing doesn't
apply) → `Workspace` → shown `DataSet::applyRevision(rev, rows, schema?, invalidation)` →
buffer/model/filler handling; (c) harness scenarios (mid-fill invalidation, row growth,
schema swap, stale-view interleave, idempotent ignore); (d) lane `data_edit` (Rust, cargo
tests mirroring csv2arrow discipline: §1.2 parse-back vectors, promotion rules, atomicity,
inverse round-trip exactness); (e) the seam + surface flip (§7).

**The build item that will be missed otherwise:** `GridModel::rowCount` is fixed at open
today. Row-growing edits make it **dynamic** — the model needs rowsInserted/rowsRemoved
handling, not just dataChanged. Buffer side: update `_revision`/`_rowsTotal`, drop chunks
intersecting the range, emit, and the filler's viewport re-plan covers the holes.

## 7. Surface & seam (frontend wiring)

- **Seam:** proxy `setData` → **commit-boundary batching** → `QUndoCommand` → JaspClient
  submission. Commands never write anywhere themselves — `redo()` submits the op,
  `undo()` submits `apply_inverse` with the stored blob (§5). No `mergeWith`.
- **Checklist** (HANDOVER-multidataset-fold.md §3): flip `ItemIsEditable` on
  `ExpandDataProxyModel` when the source dataset `isOpen()`; un-stub `toggleColType` /
  `setColumnName`; route cell edits through proxy → command → submission. Any new QML the
  merge brought gets checked against GridModel's invokable surface.

## 8. Mirror collapse (direction decided — D9) & Phase B

The frontend is now both the **declarer of schema intent** and the **holder of the canonical
schema the wire feeds back**. That picks the collapse direction: **`DataSet::schema()` is the
metadata holder, fed by `data_changed`/open results**. Migration steps:

- **B1** — form providers read `schema()` (the mirror becomes redundant-but-present).
- **B2** — label editor ↔ `schema_change` (edit era; the editor reads canonical schema, not
  Column labels).
- **B3** — filters/computed columns via their work kinds (R lane — separate track; also the
  missing consumer of `runComputedColumn`).
- **B4** — delete mirror creation in `applySchema`; Column's label/type members die with it.

Label-filter state (`FilterLabelCommand`): **construct state on `DataSet`**, input to the
future `filter` work kind. Not wire schema, not an edit.

## 9. Deferred: the derivation family (recorded, not built)

Transpose and long↔wide reshapes are **not edits** — they produce a **new dataset with a new
id**. Modeled as "**open with a transform**": the source is dataset id(s) instead of a file
path, and the entire `data_open` identity machinery applies unchanged (orchestrator mints
`ds-N`, assigns the revision-0 cache path, lane writes it, terminal result
`{dataset_id, schema, rows}`).

- Semantics: reads A (must be Ready) → mints B (revision 0). A untouched. No inverse, not
  undoable — closing the tab discards.
- Performance: lane-native Arrow (pivot/hash-aggregation, no cell round-trip through any
  frontend). This is why it lives in the lane.
- Wire slot: `Work.dataset_ids` is already a `Vec<String>` — the same shape later carries
  multi-input derivations (join: two in, one out).
- Named members: `transpose`, `pivot_longer` (wide→long), `pivot_wider` (long→wide).
- Held off until the edit era lands; the slot above is decided so this can never accrete
  into the `data_edit` family.

## 10. Lane notes (non-normative) — including the per-edit storage ladder

Per-edit apply cost is the main *implementation* risk of the edit era; the wire contract is
already shaped so none of these choices ever show up on it. Escalation ladder:

1. **Stream-copy + patch:** write the new Feather by streaming the old one, replacing the
   edited region. Dumb, correct, O(dataset) IO per edit — fine for small data, seconds on
   multi-GB.
2. **Affected-batch rewrite:** only decompress/recompress the record batch(es) containing
   edits; stream-copy the rest byte-identical. Still IO-bound but no full re-encode.
3. **Overlay journal + background compaction:** keep base file + a small ordered log of
   edits; reads (views) merge base+overlay; compact to a full file when the log grows or on
   close. The actually-good answer; the same overlay machinery would also serve future
   inverse-token formats if those ever exist.

Keystroke-frequency edits are throttled by capture-side batching regardless. Old cache files
still retire to the janitor as refs drain — no retention policy is load-bearing for undo.
The lane is serial: per-dataset edit order = revision order.

Inverse encoding details for the implementer: IPC payloads use the same codec stack as the
caches (LZ4_FRAME default; ZSTD acceptable); dictionary columns stay dictionary-typed; the
`(meta JSON, IPC bytes)` pair is opaque to everything outside the lane.

## 11. Open items

1. Mirror-collapse migration detail beyond the B1–B4 sketch (Phase B plan proper).
2. Range-aware invalidation implementation (descriptor is normative; whole-drop is the v1
   implementation).
3. Explicit map-to-null variant for `level_in_use` deletion (later op knob).
4. Stack memory cap policy (count-based `undoLimit` vs byte-based trim over blob sizes) —
   decide from RSS once real edits exist.
