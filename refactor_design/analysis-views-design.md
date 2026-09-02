# Analysis Views — materialized per-dataset projections for runners

**Status:** design converged 2026-09-02 (a long planning session; **nothing is implemented**).
This document is normative-for-when-we-build. It sits in the family of:
`data-model-design.md` (the wire schema), `data-edit-design.md` (the edit rail — D1–D11),
`data-view-design.md` (**the grid's chunked `data_view` op — a different artifact**, see §11),
`HANDOVER-post-excisions.md` (the frontend as it stands), `neo-jasp.md` §8 (the cache).

## 1. The problem

Today the runner reads the **base cache** itself: `dataset_paths` → mmap → `read_jasp_data`
applies the analysis' column spec, requested-type coercions, label overlay and filters **at
read time, in R** (§8.3 of the spec — validated and benchmarked, but R-side). That leaves:

- every runner re-deriving the projection per work unit, per rotation;
- multi-dataset analyses with no clean story (different arity!);
- the R side owning a second coercion engine (the CSV lane already owns value parsing,
  typing, level extraction, `numeric_levels` — there are *two* engines today);
- remote runners with no compact artifact to fetch.

## 2. The concept

> **A view is a single-dataset, content-addressed projection: the requested columns,
> each materialized at its requested type with levels relabeled, the filter mask applied.
> The frontend orders views; the worker builds them; work units staple one fetch ref per
> dataset input; runners read plain typed Arrow and do only language adaptation.**

The frontend already consumes views (grid chunks). This design gives runners the same
treatment: the worker becomes the **single presentation layer** — display strings for the
grid, typed+relabeled casts for analyses.

## 3. Decisions (AV1–AV12, frozen unless reopened deliberately)

**AV1 — Views are single-dataset projections.** A multi-dataset analysis gets **N views**
(ordered like `dataset_ids`); the runner receives `data[[i]]`. **Joins are dataset-manager
ops, not views** — a merge produces a new dataset with its own identity/revision. Views
never fuse arity-mismatched data. (Killed in planning: stitching + dataset-prefixes.)

**AV2 — Push spec derivation.** The frontend derives each view's spec from **options +
schema + QML form metadata** — formalizing the preload pipeline that already exists
(`ColumnEncoder::encodeColumnNamesinOptions`; the module audit found ~95% of analyses
options-derivable, see §12). The frontend is the only party holding all three inputs.

**AV3 — `fullDataset` flag.** Description.qml gains `fullDataset: true` (next to
`preloadData`, which is the precedent — 22 modules already declare it). Conditional
bindings allowed (`fullDataset: dataType !== "raw"` — jaspSem's raw branch stays lean).
Semantics: "my read-set is not derivable; give me everything". A registry lint fails any
R code using `all.columns=TRUE` / `.allColumnNamesDataset()` without the flag. The
schema (column-name list) is always available to everyone for free — cheap, useful.

**AV4 — The worker owns all casts and relabeling.** Each spec entry is
`{name, as: scale|nominal|ordinal}`; the worker materializes one field per entry:
dictionary→scale = the **values** parsed once (k levels, not N rows); nominal/ordinal =
label-or-value overlay + `ordered` flag; text-as-scale → null. The §8.3 semantics move to
Rust — **the CSV lane's typing crate is the seed** (it already parses values under
locale, builds dictionaries, knows `numeric_levels`). The R-side read-coercion engine is
retired after differential parity (§10).

**AV5 — Encoding starts and ends at the language boundary.** Views carry **real (display)
names** as field names (Arrow accepts arbitrary UTF-8); the real↔token map rides in view
metadata; **the map is the protocol** — nobody re-derives tokens. The runner renames
symbols if its language requires (R: hex), encodes user-authored R code (`encodeRScript`),
and decodes results via lookup. The system (worker/orchestrator/specs) never encodes.
The measurement-level vocabulary (`scale`/`nominal`/`ordinal`) is system-native — it is
already the wire schema's vocabulary — so casts keyed by it are *not* R lore.

**AV6 — Field naming: `<real name>__<type>`, always.** Uniform even when a column is
requested once (deterministic → content-hash stable). Type is closed vocabulary; the real
name may itself contain `__` — parsing anchors at the **last** `__`, unambiguous because
the type vocabulary never contains `__` (`a__b__scale` → `a__b`; `foo__scale__scale`
round-trips). R symbol: `HEX(name)__type`. **No prefixes** (killed by AV1), **no revision
tokens in names** (churns every symbol per edit; breaks future user-code references).
Hex derivation should be **name-derived, not index-derived** (stable under insert/reorder
— index-derived tokens would cascade and churn view hashes). *Verify current derivation
before pinning.*

**AV7 — Two-step ordering with pins.** `view_build {spec}` → worker builds (or hash-hit)
→ `view_ready {view_id, stats}` — the view is **pinned**. The frontend submits work
referencing it; on the work's terminal status, `view_release {view_id}` — unpinned =
**evictable, never hard-deleted** (soft release dodges mmap/unlink races, esp. Windows).
Frontend bugs = bounded leaks until session teardown. Pin table lives in the worker (the
store-keeper); the orchestrator holds only `view_id → path` identity (same class as
`dataset_id → path`).

**AV8 — Content addressing.** `view_id = hash(spec, base data revision)` →
`<session>/views/<hash>.arrow` (Feather V2 + LZ4, like the cache). Same hash ⇒ byte-identical
rebuild; an edit bumps the revision ⇒ new hash; stale views are unreachable, not invalid.
Revision pinning rides the existing D11-echo lifecycle machinery.

**AV9 — Materialization policy.** Two-step forces a holder (decoupled compute/consume =
pre-warm pipelining; the alternative — compute-at-dispatch — couples them and was
rejected, see §13). So: files are the norm, bounded — **LRU + disk budget**, delete on
session teardown, **tmpfs backing is a config knob** (the "never touch disk" desire,
satisfied by mount, not code). **Pass-through:** `columns: all` + no filter resolves to a
reference to the base file — zero copy, zero pin cost. This is what keeps both the disk
blow-up and the pin blow-up fears bounded.

**AV10 — Fetch stapling & the delivery ladder.** The work unit carries **one fetch ref
per dataset input**; the runner fetches, mmaps, goes. Delivery is a metadata stamp:

| Runner | Delivery | Router's job |
|---|---|---|
| local (v1) | path — runner mmaps | `view_id → path` lookup; **zero bytes** |
| remote (later) | URL — runner HTTP-GETs from the worker's content-addressed store (CAS pattern) | stamp the URL |

> **The inline rule:** *inline what you already hold in RAM; never relay what you must
> read from disk.* The router (single-threaded) must never block on an FS read, so view
> bytes never ride through the orchestrator. The §18 binary tail stays for
> frontend-authored bytes only (edit cells). "Inline if it fits" transmutes into "small
> views are cheap HTTP GETs" — a transport policy, not a protocol feature.

**AV11 — Pre-warm as the primary flow.** The frontend re-derives specs as options settle
(debounced during editing) and orders views in the background — by submit time they're
mmap-ready, the build hidden behind runner warmup. Stale pre-warms are harmless (unused
hashes awaiting GC).

**AV12 — The pull model is a design reserve, not code.** If a future module genuinely
cannot declare itself (not even via flag/schema), the runner-side dynamic spec is the
documented shape. Nothing in v1. (Demoted after the audit: see §12.)

## 4. The spec

```jsonc
// per dataset input, on the work unit
{ "dataset_id": "d1…",
  "columns": [ {"name": "Age (years)", "as": "scale"},
               {"name": "group",       "as": "nominal"},
               {"name": "group",       "as": "scale"} ],   // multiplicity is structural
  "filter":  "mask-col-ref",          // a derived boolean column reference (see §9)
  "all":     false }                  // or columns: null + all: true  → pass-through if unfiltered
```

- Types appear **only** here (structured), never as string conventions.
- The audit's "three conventions to replicate" (`encodeColNames`, `name.type` suffixes,
  interactions-as-arrays) reduces to **zero string-convention replication**: the type
  dimension becomes the structured `as` field; interactions stay options-side derivation.

## 5. The view artifact

Feather V2 (Arrow IPC + LZ4), one per (dataset, spec, revision):

- fields: one per spec entry, named `<real name>__<type>`, typed per AV4
  (float64 / dictionary<utf8, ordered=0|1>), levels relabeled (label-or-value);
- rows: post-filter extent;
- a **base-row index** column (int32) mapping view rows → base rows — analyses that
  report per-row results (outliers, influence) must be able to name base rows;
  *pinned as a must, exact spelling open*;
- schema metadata: the real↔token map (the decode authority), base revision, filter ref,
  spec digest, stats (rows, per-column distinct counts where cheap).

## 6. Wire (all additive; `v` stays 1 per neo-jasp.md §28)

- `view_build` — a `kind:"data"` work: `payload {op:"view_build", spec}`.
- `view_ready` — its terminal result: `{view_id, fetch: {path|url}, stats}` (pin held).
- `view_release` — `{view_id}`; fire-and-forget soft release.
- `work` gains `views: [{dataset_id, view_id}]` (order-preserving vs `dataset_ids`); the
  runner form resolves to fetch refs. Runners that don't speak views keep receiving
  `dataset_paths` (migration bridge, §10).

## 7. The R side (all it has left to do)

1. fetch/mmap each view; `read_feather` → factors/ordered factors fall out natively
   (the `ordered`-flag round-trip is validated);
2. rename: split each field name at the **last** `__` → middle is the real name →
   `names(df) <- paste0(HEX(name), "__", type)`. Invertible by construction; the map in
   metadata is the authority if ever in doubt;
3. `encodeRScript` over user-authored code (real names in code → `HEX(name)__type`
   symbols) — *the "which type does bare `age` mean in user code?" question is a
   computed-columns-era decision, parked in §13*;
4. results decode: lookup in the map; display drops the type.

Single-dataset analyses keep the scalar `dataset` argument (compat); genuinely
multi-dataset kinds receive `data[[i]]`. Decide scalar-when-one vs always-list
**consciously** in jaspBase before the first multi-dataset kind — one line, but decide it.

## 8. Ownership (the one-table summary)

| Concern | Owner |
|---|---|
| Spec derivation (options+schema+QML) | frontend (reuses the columnencoder machinery) |
| Ordering, pins, release, pre-warm | frontend lifecycle; pin table in the worker store |
| Casts, relabeling, filter application, stitch—no, **no stitch** | data worker |
| Store: content-addressed, LRU+budget, tmpfs-able, pass-through | data worker |
| `view_id → path/url` identity | orchestrator (metadata only, never bytes) |
| Fetch + mmap + symbol renaming + user-code encoding + decode | runner (language boundary only) |

## 9. Dependencies

- **Filters as derived boolean columns** (the pinned NEO return-design): the worker can
  only apply a filter that *is data* — an R expression cannot be evaluated in Rust. View
  filter application and the derived-column design lock together.
- **Lane coercion parity**: §8.3's semantics ported to the lane crate; differential tests
  are the gate (same spec → old R read vs worker view → identical frames).
- Nothing else blocks v1: HTTP CAS, remote delivery, multi-dataset *kinds* are all later.

## 10. Migration (incremental, reversible)

1. Worker: view store + `view_build`/`view_ready`/`view_release` + casts seeded from the
   lane crate; differential harness vs the R reads.
2. Frontend: spec derivation behind a flag; two-step ordering; staple fetch refs.
3. jaspBase: the `readDataSet*` seam gains view mode (read the view instead of the
   cache — a path swap) while the R read path stays as fallback.
4. Module registry: `fullDataset` flags + lint; audit the offenders (§12).
5. Flip the default; retire the R-side read-coercion engine (the Phase-6 acceleration).

## 11. Relationship to the grid's `data_view` (naming hazard!)

`data-view-design.md` / `data-view-format.md` describe the **grid's chunked reads** —
ephemeral display strings, §1.2 escaped TSV, `render` locale spec. Analysis views are
**typed Arrow files for compute**. Same worker, same presentation-semantics home, sibling
artifacts. A future unification (chunks served from view machinery) is plausible; not a
design constraint today.

## 12. The module audit (2026-09-02; `/home/sp42/modules-registry/{Official,Community}`)

- **Preload path dominates**: ~276/292 Official analyses; the wanted-columns derivation
  already exists engine-side (options + QML metadata). 78 of ~83 explicit
  `.readDataSetToEnd` sites are options-derived. `.readDataSetHeader`/`all.columns`:
  **5 sites, 3 modules** — jaspSem (`sem.R:78`, matrix branch; the raw branch is
  options-derived — hence conditional flags), jaspMetaAnalysis ×2 (MASEM wide matrices),
  jaspBain ×2 (lavaan-syntax scanning). jaspJags AST-walks user code
  (`jagsModule.R:1703-1727`) — flag it, or extend derivation with an identifier-tokenizer
  for code-bearing options (over-inclusive specs are safe: specs need only be **supersets**).
- **No hardcoded dataset-column reads found.** Create-then-read-back patterns (jaspAudit
  `critical`/`indicator_col`, jaspMetaAnalysis `esCol*`, jaspML predictions) dissolve in
  NEO: computed columns return as derivations materialized in the dataset, so they exist
  in the schema at spec time.
- The engine's own heuristic (`columnencoder.cpp:882-893`: any option string equal to a
  column name silently becomes wanted) means derivation needs **options + schema** —
  which the frontend has; the orchestrator's registry deliberately does not cache schema.

## 13. Open questions (parked, not blocking the shape)

1. Stable column identity across renames (hex token vs derived name) — affects edit
   targeting and spec staleness; ten-minute code check then pin.
2. User-code symbol references: which type reading does a bare name get inside computed
   columns / filters (computed-columns era).
3. View store on web: object store vs disk (same question as the cache).
4. `HEX` derivation: confirm name-derived (AV6).
5. Spec filter-ref spelling once derived-columns lands.
6. Do grid chunks ever route through view machinery (§11 unification)?

## 14. Considered and rejected (so we don't relitigate)

| Idea | Why it died |
|---|---|
| Stitching multi-dataset views (one fused frame) | arity mismatch; it's a join; joins are dataset ops (AV1) |
| Dataset prefixes in field names (`ds1__…`) | died with stitching; within-dataset display names are unique |
| Revision tokens in names (`…_rev4__…`) | symbol churn per edit; breaks future user-code refs |
| Compute-at-dispatch / inline view bytes in NNG | router would block on FS reads; "inline what you hold, never relay what you read" (AV10) |
| Pull-first (runner declares specs) | demoted by the audit: derivation already exists, offenders are 3 modules; kept as reserve (AV12) |
| Coercions staying R-side | two engines already exist; consolidating into the worker leaves one (AV4) — runner keeps only language adaptation |
| RAM-cached views instead of files | tmpfs gives the same without a new subsystem (AV9) |

*Transcribed 2026-09-02 from the planning session — the design that got smaller every
time it was doubted.* 📐
