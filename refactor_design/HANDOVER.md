# HANDOVER — JASP Frontend/Backend Refactor

> Read this first. It captures the decisions and their rationale from the design
> sessions. The two spec docs (below) are the normative artifact; this file is the
> "why" and the "where are we." When in doubt, the spec docs win over this file.

## 0. How to work with this user

- **Terse. Dislikes huge chat summaries** (said so twice). Keep responses tight and
  scannable; put the detail in the docs, not the chat.
- Wants **decisive opinions**, not "it depends." Give a recommendation, then the
  caveat. They'll push back if they disagree — that's the working style, not friction.
- Building toward a **clean** design and willing to make bold calls. Speaks bluntly
  about the legacy code — an intertwined monolith with real technical debt — and wants
  it replaced.
- Pattern that works: **give your opinion → fold it into the docs → offer the next step.**
- They will say "continue" meaning "keep going / finish / do the thing."

## 1. The vision (one paragraph)

Split JASP's monolith into three clean components talking over **NNG**:
- **Frontend** — thin, **language-agnostic**. Knows only analyses. Sends
  `anova("stupid column name", …)` with the *real* names the user sees; gets real-named
  results back. Knows nothing about R, engines, paths, or encoding.
- **Orchestrator** — headless process. Owns the dataset cache, capability/module
  registry, and the runner pool. Routes work. **Name-agnostic per-analysis** (builds the
  name map once, then never touches column names).
- **Runners** — standalone **pure-R** processes (`jaspRunner` package). One-or-more
  modules each. Serve `work` until told to `shutdown`. Do all column-name translation.

## 2. The north star (the "why")

The whole refactor negates **four original sins**:

| Original sin | Target design |
|---|---|
| R embedded in C++ (`RInside`) — the C++/R seam runs through the middle of the program | Standalone R runner; nothing else embeds R |
| Homegrown IPC (`IPCChannel`, boost shared memory) | Standard message-passing (NNG), transport-agnostic |
| Column-name encoding smeared across ~5 sites / 2 languages | Canonicalize once at ingestion; translate only at the user boundary |
| Data not a first-class artifact (C++ `DataSet`, `RBridgeColumn` column-copy, preload/non-preload) | One shared, zero-copy Arrow/Feather cache |

**Key insight:** the encoding smear is a *symptom* of embedding R in C++. Make R its own
process with a clean message boundary and translation snaps back to the boundary — once
each way. Sin #1 and sin #3 are the same fix.

## 3. The docs (what's where)

All in `refactor_design/`:

- **`architecture-refactor-plan.md`** (+ `.html`) — the design narrative.
  - **§0** Design Principles (north star, four sins table).
  - **§2** Target architecture (3-component diagram).
  - **§3** Component breakdown. **§3.2** orchestrator responsibilities (make-before-break
    rotation lives here). **§3.5** Arrow cache data path (sequence diagram, now shows
    canonicalization at cache-build). **§3.6 Data Model & Column-Name Encoding** (the
    dedicated encoding section — one-liner, who-does-what table, migration).
  - **§5** Runner pool (module-typed LRU). **§5b** R interruption + queue processing.
  - **§10** Web/container. **§11** Self-attaching runners.
- **`comms-protocol-spec.md`** (+ `.html`) — the normative wire protocol.
  - **§3** Transport + connection handshake. **§4** Framing/envelope.
  - **§5** Message catalog (all messages, by direction). **§5.2** `dataset_ready` carries
    `display_name`. **§5.3** `work` (self-contained). **§5.5** registration. **§5.6**
    runner work-queue reconciliation (evict-and-append).
  - **§9** Data plane. **§9.4** column-name canonicalization & encoding contract.
  - **§11** Lifecycle/timing + **Deferred: memory-based pool sizing** note.
- **`render.py`** — embeds each `.md` into its self-contained `.html` viewer. Run
  `python3 refactor_design/render.py` (or `render.py comms` / `render.py arch`) after
  editing a `.md`. **Edit the `.md`, never the `.html`** (the html is generated).

## 4. Key decisions + rationale (the meat)

These are settled. Each is in the docs; the *rationale* lives here.

1. **NNG for comms** (over ZeroMQ/gRPC). NNG is the only *actively maintained*
   brokerless lib (ZeroMQ/nanomsg have ~0 recent commits). The killer feature is
   **`nanonext`** (R binding, under `r-lib`/CRAN, C-implemented, async + TLS + ws).
   Perf differences are irrelevant at JASP's message rates (analysis = 100ms–60s; IPC is
   µs). Transport-agnostic: `ipc://` → `tcp://`/`tls+tcp://`/`ws://` is a config change.

2. **`work` is self-contained** (carries options + output `settings`). No separate
   `settings` message. "Options changed" *and* "settings changed" are both just a re-sent
   `work` with the same `analysis_id` + bumped `revision`. Makes the runner stateless and
   the routing the orchestrator's only concern.

3. **Evict-and-append queue, NOT in-place swap.** (User's explicit call, overriding my
   in-place suggestion.) Newer revision for an existing `analysis_id` → evict the entry
   (cancel if running) + append to the **back**. One uniform code path; avoids mutating a
   possibly-executing item. Consequence: an edited analysis goes to the back of the line
   (accepted). Only special case: stale-revision guard (`rev₀ > rev` → ignore).

4. **Module-typed runners + version-aware routing.** Each runner advertises one-or-more
   modules at specific versions: `register.modules = [{name, version}]`. Routing matches
   on name **and** version. This is what lets a **dev runner (WIP version)** and the
   **installed runner (release version)** coexist.

5. **Multi-frontend support.** Orchestrator accepts ≥1 concurrent frontends, each on its
   own PAIR channel via the control handshake. Tracks `analysis_id → {runner, session_id}`
   to route results home; broadcasts module-list changes to all frontends. `session_id`
   scopes tenancy.

6. **Versioned orchestrator URL.** URL embeds the protocol version
   (`ipc:///run/jasp/orch/v1.sock`) so a second frontend can detect a running compatible
   backend. Validation via `hello`/`welcome` handshake exchanging versions.

7. **RAM budgeting SCRAPPED for now** → fixed `pool_size` (default 4). The memory
   accounting research (PSS not RSS, etc.) is preserved as a **Deferred** note in comms
   spec §11. Don't rebuild it unless the user asks.

8. **Runner self-attaching (dev mode).** Runners self-register (`register` advertises
   modules + version + `qml_root`). Orchestrator prefers attached/dev runners. Solves the
   dev workflow (no code signing, no bundled R). `form_reload` tells the frontend to load
   QML from the dev's `qml_root`.

9. **Make-before-break rotation.** When a managed runner crosses soft `max_invocations`,
   the orchestrator spawns a replacement (same lib set) but the old runner **keeps
   serving** until the replacement is **READY**; only then stop routing to it + shut it
   down. Pool briefly runs at N+1. **The orchestrator drives retirement** — there is **no
   `draining` message from the runner**; the runner is oblivious and just serves `work`
   until `shutdown`. Invariant: retire only after replacement confirmed READY.

10. **Data loading = one zero-copy Arrow cache.** Orchestrator ingests the source
    (CSV/SPSS/Excel) **once** on `dataset_open` → single Feather V2 (Arrow IPC) file.
    Runners read via `arrow::read_feather()` (mmap, zero-copy). **Kills** the
    `rbridge_readDataSet*` zoo, preload/non-preload, `RBridgeColumn` column-copy, and the
    `static datasetStatic` global. Frontend uses `dataset_id` only; paths never cross the
    frontend boundary.

11. **Encoding = canonicalize once, translate at the boundary.** At cache-build the
    orchestrator assigns each column a clean **canonical** (R-syntactic) name and stores
    the **real↔canonical map in Arrow schema field metadata** (`jasp:display_name`). Built
    once, in the data itself. **Frontend** = language-agnostic (real names in/out).
    **Orchestrator** = name-agnostic per-analysis. **Runner does ALL translation** (encode
    options real→canonical incl. user R-code via `encodeRScript`; decode results
    canonical→real for display) as cheap in-R lookups. **C++ `ColumnEncoder` retired.**
    Decode still happens at every display exit (plots rasterized in R) but is now a
    trivial lookup; encode collapses from ~3 smeared sites to **1** (`encodeRScript`, the
    user-R-code case that can't be wished away — lives in the runner because it's R code).

12. **LZ4 compression, mandatory, everywhere.** All Feather cache files and all inline
    Arrow IPC payloads (bulk `data_edit`, `data_update` columns) use LZ4 frame
    compression. Decompresses at ~4 GB/s per core — free at read time — cuts size
    ~2–4× on typical data. No uncompressed mode, no toggle. ZSTD is the documented
    fallback if a future dataset class needs better ratios. (Comms spec §9.5.)

13. **Data mutation flow: runner → orchestrator → frontend → rerun.** Runners can
    produce new/updated columns (computed columns). The runner sends `data_update`
    (Arrow IPC payload) to the orchestrator and forgets — **no sync/ack** (runners
    are stateless). The orchestrator applies it to the cache and broadcasts
    `data_changed` to **all** frontends. The frontend decides which analyses to rerun
    (it knows which analyses reference which columns). The orchestrator does **not**
    auto-rerun or track analysis↔column dependencies. (Comms spec §5.2 `data_changed`,
    §5.4 `data_update`.)

14. **`.jasp` file format: SQLite → Arrow.** The `internal.sqlite` (dynamic
    `DataSet_1` table, `Columns`/`Filters`/`Labels`/`DataSets` metadata tables) is
    **fully replaceable by Arrow IPC + metadata**. Raw data → Arrow columns. Per-column
    metadata (title, description, type, compute filter, labels) → Arrow field metadata.
    Dataset-level metadata (filters, description, revision) → Arrow file-level schema
    metadata. `analyses.json` + binary resources stay as separate ZIP entries. New
    layout: `manifest.json` + `data.arrow` (LZ4) + `analyses.json` + `resources/` +
    `index.html`. **Kills** `internal.sqlite`, `internalDbDefinition.sql`, all of
    `databaseinterface.cpp`'s SQL generation, the `Dataset_1`/`DataSet_1` casing bug,
    `assert(dataSetId == 1)`, and the SQLite dependency for data storage.
    (Arch plan §7 item 9.)

15. **Type representation in Arrow (arch §3.7, comms §9.6).** scale = `float64` always;
    integer-ness is a display hint `jasp:all_integer` (recomputed on every cache rewrite,
    never gates logic). **The Arrow type IS the measurement level**: ordinal =
    `dictionary<int32, utf8, ordered=1>`, nominal = `dictionary<int32, utf8, ordered=0>`,
    using Arrow's **native `ordered` flag** on the DictionaryType (the format's own
    ordered-categorical mechanism; pandas uses it too) — no `jasp:column_type`/`jasp:order`
    metadata for the distinction. The dictionary holds the data **values** (each level's
    `originalValue` — int/float/**string** — stringified); `jasp:labels` is a sparse
    value→display-label overlay (the label editor's content; empty where a label would just
    repeat the value). **Read-as-scale returns the values directly** (not the labels); the
    per-cell `intsId` surrogate **dies** (Arrow's dictionary indices replace it). Ordering
    is **R3**: the dictionary sequence order is the ranking (that's what `ordered=1`
    means); reorder = reorder dictionary + reindex — free (rewrite happens anyway), the
    **values stay stable**, and `jasp:labels` is value-keyed (no realignment) — so it's
    still a faithful port of current JASP (values are the stable identity, `Labels.ordering`
    is the separate ranking). Per-column requested-type reads replace preload/non-preload +
    the Rcpp bridge with one runner function. String categoricals are dictionary-only;
    as-scale = `NA`. **PoC item:** confirm our Arrow library preserves the `ordered` flag on
    write→read (some implementations, e.g. Arrow.jl historically, have dropped it).

    **Verified in the current code:** order is *mostly canonical, not smeared* — one chain
    (`Column::_labels` vector position → `Labels.ordering` → R factor levels, per the
    contract at `CommonData/label.h:20`), read everywhere. The migration **collapses** the
    parasitic mirrors (`Label::_order` field, DB `ordering`, the JSON `"order"` key with
    its two inconsistent read-back conventions) into one place (the dictionary sequence —
    the Arrow-native `ordered` order), and **fixes
    the one real smear**: `reorderFactor()` in jaspBase
    (`R/friendlyConstructorFunctions.R:91`, used by `replaceNA.ordered`/`ifElse.ordered`)
    re-sorts and ignores the dataset's canonical order — computed columns must preserve
    source order in the new `data_update` path.

## 5. Open questions / deferred

- **Orchestrator implementation language.** Recommended: **Rust** for prod (no GC, `nng`
  crate, `arrow-rs`, memory-safe), **Python** for prototype (`pynng` + `pyarrow`).
  C++-without-Qt is the fallback if the team won't do Rust. NOT decided yet.
- **Module-repo greps (pre-commit verification, outside this repo):** (a) where do
  table/results titles get decoded? (b) do any modules call `encodeColNames`/
  `decodeColNames` directly? These determine whether "modules don't change" holds fully
  or a few need a touch. **Not done yet.**
- **~~Computed-column write-back wire detail~~** → **Answered: `data_update` message**
  (comms spec §5.4) + `data_changed` broadcast (§5.2). Runner sends Arrow IPC payload,
  orchestrator applies to cache + notifies frontends. No sync/ack (runners stateless).
- **`nanonext` PoC** — verify: per-peer channels, `pipe_notify` deferred-close (there's a
  known bug nanomsg/nng#1665 where the removal callback fires once if you close a socket
  from within the callback — must defer the close), `recv-size-max` (defaults ~1MB,
  silently drops larger messages — must raise to `max_inline_payload`).
- **Decode-in-frontend for tables** (optional future optimization; plots must stay in R
  since they're rasterized there).
- **Memory-based pool sizing** — deferred, research preserved in comms §11.
- **Web/container deployment details** — arch §10 / comms §10.
- **`.jasp` Arrow migration detail** — the exact Arrow field-metadata key names
  (`jasp:title`, `jasp:labels`, `jasp:filters`, …) and the read/write code in the
  orchestrator. Decided at the format level (arch §7 item 9); implementation TBD.
- **Computed columns must preserve canonical level order** — `reorderFactor()` (jaspBase
  `friendlyConstructorFunctions.R:91`) currently re-sorts and drops the dataset's order.
  The `data_update` computed-column path (comms §5.4) must carry/preserve source order.

## 6. Next concrete steps (queued, in roughly this order)

1. **`jaspRunner` package skeleton** — the §5.6 reconcile loop + nanonext connection +
   the `data.R` Arrow-read layer, as actual R code.
2. **`nanonext` PoC checklist** — per-peer channels, `pipe_notify` deferred-close,
   `recv-size-max`.
3. **Module-repo greps** (decoding sites + direct encode/decode calls).

## 7. Gotchas learned about the CURRENT code (for context)

- **The column-name encoding is a stored MAP, not a reversible function.**
  `ColumnEncoder::encode()` does `encodingMap().at(in)` (throws if unknown). There's a
  main `columnEncoder()` + a second `extraEncodings` encoder (`JASPColumn_…_For_Replacement`,
  for computed/replacement columns) + deliberate encode→decode **asymmetry** ("for the
  results to be less ugly"). All in C++.
- **`.fromRCPP(name, …)`** (jaspBase `common.R`) is a stringly-typed dispatcher into Rcpp
  `InternalFunction`s. Plus a *separate* `setColumnFuncs(...)` mechanism injecting XPtr
  function pointers. Two bridging styles — both die when the runner is pure-R over NNG.
- **The encoding is concentrated in jaspBase**, not smeared through every module: encode
  in C++ `_encodeColumnNamesinOptions` (options) + `encodeRScript` (R code) + R
  `formula.R::formulaEncode`; decode in R `writeImage.R::decodeplot` (recursive walker
  over the ggplot/grob object) + `formula.R` validation. `.v()`/`.unv()` are deprecated
  ("JASP handles encoding automatically"). **Table-title decode site not yet located**
  (likely module-side or results serialization — hence open question #2).
- **Two data-read paths today:** preload (`preloadData=TRUE`, default —
  `.readDataSetRequestedNative` upfront) vs non-preload (`.readDataSetToEnd` per call,
  marked "deprecated" in a code comment). Both die with the Arrow cache.
- **`IPCChannel`** (CommonData) = boost interprocess shared memory + JSON, with manual
  resize + file-based heartbeat — a fragile homegrown comms channel. Dies → NNG.

## 8. One-line summary

Standalone R runners over NNG, one Arrow cache, canonicalize-once + translate-at-the-
boundary, thin language-agnostic frontend — climbing out of four original sins toward the
design JASP should have shipped with.
