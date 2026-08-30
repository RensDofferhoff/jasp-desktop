# Data View — Wire Format & Buffer Format (focused spec)

**Status:** rev 1, 2026-08-16. Scope deliberately narrow: **only** the bytes on the wire and the
frontend's in-memory buffer. Routing, the model seam, guards and updates live in
`data-view-design.md` and are NOT part of this discussion.

## 1. Wire format

### 1.1 Frame layout

Uses the existing framing (§18.1) with its optional binary part:

```
[ u32 BE json_len ][ JSON envelope ][ TSV bytes …            ]
                    ├── json_len ──┤└── binary payload part ──┘
```

The envelope `format` field is `"text/tsv"` (new §18.3 entry). All metadata travels in the JSON
envelope; **all cell bytes travel in the binary part** — never inside a JSON string (no escape
inflation, the orchestrator forwards the bulk without parsing it, any future frontend gets raw
bytes).

**View request** (frontend → orchestrator → lane; `kind:"data"` work payload):

```jsonc
{ "op":"data_view",
  "row_offset": 0,          // first row of the window
  "row_limit":  null,       // null = until end of dataset (still capped by max_bytes)
  "columns":    null,       // null = all columns, schema order; else display names (later)
  "max_bytes":  20000000,   // budget for THIS response, counted on the TSV bytes exactly
  "render": {               // absent = lane default: "." decimal, no grouping, precision 10
    "decimal":   ",",       // decimal separator (frontend's QLocale is the authority)
    "thousands": ".",       // grouping separator; "" = no grouping (legacy useThousandSeps pref)
    "precision": 10         // significant digits ('g') — legacy parity (§1.2)
  }
}
```

**View result** (lane → orchestrator → frontend; typed additions on the `kind:"data"` payload):

```jsonc
{ "dataset_id":"ds-6",          // orchestrator-filled (echo)
  "dataset_revision": 0,        // orchestrator-filled: dataset revision at dispatch
  "rows": 50000,                // lane: TOTAL rows of the dataset at serve time
  "row_offset": 0,              // lane: first row carried in the binary part
  "row_count": 1234,            // lane: rows carried in the binary part
  "truncated": true }           // lane: stopped at max_bytes before row_limit/end
```

`max_bytes` counts the **binary part** (escaped TSV) exactly. The lane stops at a **row
boundary** — a chunk is always whole rows — with a **progress guarantee**: it emits rows while
budget remains, and if nothing has been emitted yet it emits the next row **regardless of size**.
So `row_count ≥ 1` unless the offset is at the end (then: zero bytes, `row_count: 0`), and a row
larger than the budget simply produces an oversized chunk. Chunk size constant:
`JASP_VIEW_CHUNK_BYTES = 20 000 000` (≈20 MB).

**The hard cap is the wire ceiling**, not the chunk budget: a single row whose escaped TSV would
exceed `max_inline_payload` (256 MiB − envelope margin) cannot fit in one message — the lane
answers `fatalError` ("row too large to transport", with the row index) instead of emitting.
Consequences, all accepted and documented:

- The frontend's 200 MB budget is checked **before** requesting, so the buffer ends at
  ≤ budget + max(CHUNK, biggest row): the budget is **soft** ("≈200 MB").
- No mid-row continuation: the "chunks are whole rows" invariant holds unconditionally, so the
  grammar and the buffer carry no partial-row state.
- A 256 MB single row is absurd in practice (it would take ~10k columns of 25 KB cells); failing
  visibly there is the honest cap.

### 1.2 TSV grammar (normative)

- **Encoding:** UTF-8. **Column separator:** TAB (0x09). **Row terminator:** LF (0x0A) after
  *every* row, including the last. **Shape:** rectangular — every row has exactly
  `schema.len() - 1` separators (Arrow guarantees it; the lane never emits ragged rows).
- **Escapes** (backslash; nothing else is escaped): `\t` → TAB, `\n` → LF, `\r` → CR,
  `\\` → backslash.
- **Missing/null:** the whole cell is `\N`. A literal cell text `\N` is encoded `\\N` —
  unambiguous by construction. **Empty string** (zero-length cell) is a value, distinct from null
  (matches the lane's "zero is a value" semantics).
- **The byte-level uniqueness property:** because real TAB/LF bytes are always escaped inside
  cells, the separators are *unambiguous at the byte level*. Splitting rows/cells is therefore a
  raw scan for 0x09/0x0A with **no escape logic**; unescaping is a per-cell concern and is skipped
  entirely for cells containing no backslash.

**Value rendering** (the lane owns it — the frontend displays strings, it never parses values).
Scale values are rendered per the request's `render` spec with **C/Qt `'g'` semantics — exact
legacy parity** (verified: legacy grid = `QLocale::toString(dbl, 'g', 10)`,
`CommonData/columnutils.cpp:227` + `QMLComponents/utilities/qutils.cpp:509`):

| Arrow type | Rendering |
|---|---|
| float64 (scale, incl. `all_integer`) | `'g'` format at `precision` significant digits (default 10): round to `precision` sig digits; exponent form exactly when the exponent < −4 or ≥ `precision` (exponent as `e±NN`, ≥2 digits: `2e-07`, `1e+10`); the plain form gets `thousands` grouping on the integer part and `decimal` as separator; the exponent form localizes only the mantissa's decimal separator |
| dictionary (nominal/ordinal) | the dictionary **value** string, verbatim — data, never formatted |
| null (any type) | `\N` |

- `all_integer` needs no special case: under `'g'`, integral values render without a decimal
  point anyway (`42`, and `1e+12` at the magnitude edge — same as legacy showed).
- `'g'` rounding (10 sig digits) is what users see today; it is lossy beyond 10 digits by design
  (legacy parity beats round-trip fidelity here — the edit path, when it lands, parses the same
  display string back, exactly like legacy).
- Explicit separator **characters** in the request, not a locale id: the frontend's `QLocale` is
  already the authority (legacy worked via that callback), and the lane stays dependency-free
  (no bcp47→separator tables in Rust). Unicode separators (French U+202F etc.) pass through
  UTF-8 — none collide with the escape grammar.
- Reserved: NaN/±Inf cannot occur in v1 caches (the CSV lane maps unparseables to null); if a
  future write path introduces them, they get dedicated renderings — pinned then, not now.

**Division of labor (v1) — the conversion happens in exactly one place: the lane.**

| Step | Who |
|---|---|
| Choose the format parameters from the user's `QLocale`/preferences | frontend — sends them as the `render` object of every view request |
| **float64 → locale-formatted string** | **the lane — the only converter** |
| Display | frontend shows the received bytes verbatim; **no numeric parsing or formatting anywhere in the frontend** |
| Locale/separator preference change | frontend drops the buffer and refills with the new spec (§2.3) — a rare, settings-level event |

The single deferred exception (edit era, §3.2): seeding the edit field by stripping the known
grouping char — mechanical string-stripping, not number formatting, and not part of v1.

Column order = the schema order of the open result; v1 always requests all columns.

### 1.3 Chunking & why these choices

- **Many ~20 MB chunks, never one big message:** small NNG frames, small orchestrator
  transients, progressive display (the grid renders chunk 1 while the rest streams in), retry
  granularity (a chunk is idempotent by `(dataset_revision, row_offset)`), and the same mechanism
  later serves sliding mode. The lane is serial, so there is nothing to gain from pipelining.
- **Text, not Arrow IPC.** Arrow IPC *is* available in browsers/WASM, but it would force an Arrow
  runtime onto every frontend implementation (the C++ desktop would need arrow-c++ as a new heavy
  dependency) and the consumer is a string grid. TSV parses dependency-free and is eyeball-
  debuggable. The upgrade path stays open and cheap: the request/reply envelope is
  encoding-agnostic — a later `format:"arrow_ipc/stream"` swaps the payload encoding without
  changing chunking, offsets, budgets or revision stamps.

### 1.4 Test vector (pins the grammar)

Schema: `score` (scale), `group` (nominal), `note` (nominal);
`render = {"decimal":",", "thousands":".", "precision":10}` (a nl/de-style locale). Rows:

| row | score (value) | group | note (cell content) |
|---|---|---|---|
| 0 | 1234567.891 | A | `hello` |
| 1 | *null* | B | `has` TAB `tab and \N literal` |
| 2 | 0.0000002 | A | `line1` LF `line2` |

Binary part, exactly (`⇥` = real 0x09, `␤` = real 0x0A; everything else literal characters):

```
1.234.567,891⇥A⇥hello␤\N⇥B⇥has\ttab and \\N literal␤2e-07⇥A⇥line1\nline2␤
```

Checks: row 0 col 0 — `'g'` at 10 sig digits stays plain (exponent 6 < 10), grouped with `.` and
`,` decimal; row 1 col 0 is null (`\N`) and its `note` unescapes to a real TAB plus the *literal
text* `\N` (not null — encoded `\\N`); row 2 col 0 — exponent −7 < −4 → exponent form `2e-07`;
its `note` unescapes to a real LF. `row_offset:0, row_count:3, truncated:false`.

**Oversized-row vector:** same dataset, `max_bytes: 10` — row 0 alone is 21 bytes > 10, but the
progress guarantee emits it: `row_offset:0, row_count:1, truncated:true`, binary part = the first
row only. The next request continues at `row_offset:1`.

## 2. Buffer format (frontend)

### 2.1 Storage

The unit is the **chunk** — exactly what one response delivers, kept verbatim:

```cpp
struct ViewChunk {
    uint64_t     rowOffset;   // dataset row of the first row here
    uint64_t     rowCount;
    std::string  tsv;         // the binary part as received (all rows LF-terminated)
    // row anchors: one (rowIndex -> byte offset) every K rows, u32 offsets (chunk < 4 GB)
    uint32_t     anchorStride;             // K, constant 32
    std::vector<uint32_t> anchorByte;      // anchor i -> byte offset of row (i*K) in tsv
};

class DataViewBuffer {
    std::deque<ViewChunk> _chunks;   // sorted by rowOffset; fill appends at the end
    uint64_t _rowsTotal = 0;         // from the responses
    uint64_t _revision  = 0;         // ALL resident chunks share one revision
    uint64_t _frontier  = 0;         // next row to request
    size_t   _bytes     = 0;         // Σ tsv.size()
    size_t   _budget;                // 200 000 000 (frontend policy, not a wire limit)
    bool     _complete  = false;     // frontier == rowsTotal
    // + the split-row cache, §2.2
};
```

**Why anchors instead of per-row offsets:** per-row `u32` costs 4 B/row — on tiny-row data
(200 MB of ~10-byte rows ≈ 20M rows) that is 80 MB of offsets on a 200 MB buffer (40% overhead).
Anchors every K=32 rows cost ~0.125 B/row (~2.5 MB for 20M rows), and locating a row means
scanning forward ≤ 32 row-terminators from its anchor — which only happens on a split-cache miss
(§2.2), i.e. the first touch of a row.

### 2.2 Cell access

```
cell(row, col) ->
  1. find the chunk (binary search on rowOffset; chunks are disjoint + sorted)
  2. row byte range: anchor[(row-rowOffset)/K] + forward scan of ≤ K LFs   [cache: see below]
  3. cell byte range: from row start, scan `col` raw TAB bytes (escape-free by §1.2)
  4. if the range contains no backslash -> QString::fromUtf8 directly (the fast path;
     most cells); else unescape (\t \n \r \\); whole-cell \N -> null/missing
```

**Split-row cache.** The grid's access pattern (verified in
`QMLComponents/datasetviewbase.cpp:556`) is **column-major over the viewport**: it iterates
`for col … for row …` across the *visible* cells only, asking several roles per cell. The hot set
is therefore exactly the visible rows (~20–60), revisited once per visible column. The buffer
keeps a small LRU of recently split rows:

```cpp
struct SplitRow {                 // one entry per hot row
    uint64_t              globalRow;
    std::vector<uint32_t> cellStart;   // ncols+1 offsets, relative to the row start
};
// LRU capacity: 128 rows (≥ any plausible viewport height)
```

First touch of a row pays one full-row split (raw byte scan); every later cell of that row is an
O(1) offset lookup. Worst-case cache memory: 128 × (10 000+1) × 4 B ≈ 5 MB at 10k columns —
bounded regardless of dataset size. Without this cache, column-major access at column 9000 would
re-scan 9000 cells per access; with it, a viewport paint costs ~one split per visible row.

The grid also asks ~6 roles per cell (`DisplayRole`, `noSepaDisplay`, `shadowDisplay`, `label`,
`value`, `filter`…); in the read-only increment they all answer the same cell string or a
constant — `GridModel` computes the cell once and reuses it across the role calls (model detail,
no format impact).

### 2.3 Lifecycle & invariants

- **Append-only fill:** chunks arrive in `row_offset` order and are appended; the sorted/disjoint
  invariant holds by construction. `_frontier = last.rowOffset + last.rowCount`.
- **One fill identity — `(dataset_revision, epoch, render spec)`:** all resident chunks share it.
  The `epoch` is a frontend-local counter bumped on every refill trigger. Refill triggers: a
  response stamped with a newer `dataset_revision`, a `data_changed`, and a **render-spec change**
  (locale / decimal-separator / thousands-preference change — the buffer's strings are
  locale-baked, so a locale switch is a drop-and-refill). Responses belonging to a superseded
  identity (stale revision or stale epoch) are dropped. No mixed-identity state, ever.
- **Budget:** stop requesting when `_bytes + CHUNK > budget` → "buffered prefix" state, or
  `_complete` when `_frontier == _rowsTotal`. The check is pre-request, so the last accepted
  chunk may overshoot (§1.1): the budget is soft, ≈200 MB.
- **Sliding mode (later) is the same structure with an eviction policy** — pinned in §2.5.

### 2.5 Sliding mode — eviction & scroll behavior (increment 3; policy pinned now)

- **Distance is 1D.** Chunks are column-complete row ranges, so the only meaningful distance is
  **row distance from the viewport** — no Euclidean/2D metric applies unless column-window tiles
  (ultra-wide data) are ever added.
- **The buffer always aims to be FULL — prefetch is the default, not the exception.** Memory is
  bounded by the budget anyway, so every resident byte is free hit-probability. Two request
  classes share the serial lane (at most one view request in flight — "priority" just decides
  what the *next* request is):
  1. **Urgent — viewport miss.** The viewport needs rows that are not resident: fetch that range
     next (possibly at a smaller chunk size for lower latency). Hard guarantee: viewport ±
     margins is always resident.
  2. **Background fill — keep the budget full.** Whenever nothing urgent is pending, continue
     filling outward from the viewport (down-biased — scrolling down dominates), or from the
     fill frontier; if the user jumps past the frontier (scrollbar jump / go-to-row), the
     background fill re-anchors to the viewport. It stops only at the budget or dataset end.
- **Eviction only makes room:** eviction happens when a fetch would push the buffer over budget —
  drop the chunks **farthest from the viewport** first, so the prefetch tail survives while
  there is room. Evicting a chunk also drops its split-row cache entries. Deterministic memory,
  no LRU bookkeeping.
- **Net behavior:** after open (or any jump), the 200 MB fills in the background; steady
  scrolling almost always hits local data; only jumps/flicks beyond ~200 MB of prefetch show
  placeholders. Same tradeoff class as any remote-data grid.
- **Scroll speed expectations.** *Buffered mode*: no fetching while scrolling at all — every cell
  is an arena lookup through the split-row cache; the limit is the existing `DataSetViewBase`
  QML-item machinery, i.e. the same as JASP today. *Sliding mode*: inside the resident set
  identical; outside it, cells render as **placeholders** until their chunk arrives (lane slice
  + serialize + hop — tens to hundreds of ms in release). Test-tunable knobs: margin sizes,
  background-fill direction/bias, and chunk size per request class.

### 2.4 Memory accounting (worst case at full 200 MB budget)

| Component | Size |
|---|---|
| TSV arenas (the budget itself) | ≤ 200 MB |
| Row anchors (0.125 B/row; 20M tiny rows) | ≈ 2.5 MB |
| Split-row LRU (128 rows × 10k cols) | ≤ 5 MB |
| Chunk structs / deque overhead | negligible |

No per-cell ownership anywhere: cells are byte ranges into the arenas, materialized to `QString`
only for visible, requested cells.

## 3. Open items (format-scoped only)

1. ~~Float rendering standard~~ — resolved: legacy-parity `'g'` at 10 sig digits + locale
   separators (§1.2), replacing the earlier ECMA-262 proposal.
2. **Display/edit duality (deferred to the edit increment).** Legacy serves two variants per
   scale cell: grouped display (`DisplayRole`) and ungrouped raw (`noSepaDisplay`, used by the
   edit field). V1 is read-only, so the wire carries the grouped variant only. When editing
   lands, the raw form is reconstructed frontend-side by stripping the (known) `thousands` char
   from scale cells — mechanical, unambiguous (`decimal ≠ thousands` in every locale), no wire
   change. If that ever proves wrong, the fallback is a lane-rendered second variant.
3. Chunk size (20 MB) — constant; trivially tunable, no format impact.
4. Trailing-LF choice is pinned to "every row terminated" — the lane's write loop and the row
   scanner are both simpler; the byte count of a chunk includes the final LF.
