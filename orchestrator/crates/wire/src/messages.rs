//! The NEO wire protocol, as Rust types.
//!
//! This module is the *single source of truth* for the message shapes spoken between the
//! frontend, this orchestrator, and (later) the runners. Because the types are defined here
//! with `serde` + `schemars`:
//!
//! * (de)serialization is **derived**, not hand-written — a field typo is a compile error;
//! * a **JSON Schema** for the whole protocol is generated from these very types
//!   (`cargo run -- --schema`), so the C++ and R sides can be validated against it;
//! * the types double as **living documentation** of neo-jasp.md §18–§19.
//!
//! Modeling notes:
//!
//! * `Envelope` carries the fields every message has (§18.2) and `#[serde(flatten)]`s a
//!   `Message`, which is *internally tagged* by the `type` field — so `{"type":"work", …}`
//!   selects the `Message::Work` variant.
//! * A `work`'s payload is *adjacently tagged*: `kind` names the variant, `payload` holds its
//!   fields (§19.1, analysis vs rcode).
//! * A `result`'s payload is adjacently tagged the same way (§19.2 *Result payloads by kind*):
//!   one work kind, one result shape. Division of labor: producers fill content; the
//!   orchestrator fills identity & location — as typed fields on the payload, never by surgery
//!   on an opaque tree.
//! * `AnalysisResult.results` is deliberately an opaque `serde_json::Value`: the orchestrator
//!   forwards the jaspResults tree verbatim and never interprets it.
//!
//! Only the messages the alpha needs are fleshed out; the rest of the catalog (data_close,
//! data_update, form_reload, …) follows the identical pattern — one variant + one struct
//! each. `data_changed` (data-edit-design §6) IS fleshed out: the Increment-4 return leg,
//! broadcast by the orchestrator at every dataset revision bump. The `data_edit` op family
//! (§2–§3) is typed too: `DataWork::edit` + `DataResult::inverse`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ─── Wire constants (§18.4, data-view-format.md §1.1) ────────────────────────

/// §18.4 `max_inline_payload`: the cap on the binary part of a single inline message.
/// One ceiling across all units — view chunks (~20 MB) sit far below it; the hard cap for
/// a single view row IS this ceiling (a bigger row cannot fit in one message; the lane
/// answers `fatalError` instead of emitting, format doc §1.1).
pub const MAX_INLINE_PAYLOAD: usize = 256 * 1024 * 1024;

/// Envelope margin on top of [`MAX_INLINE_PAYLOAD`] when raising socket `RecvMaxSize`
/// (§18.4/§4.4): a maximal binary part plus its JSON envelope + framing must always fit.
pub const RECV_MARGIN: usize = 16 * 1024 * 1024;

/// Socket `RecvMaxSize` every endpoint raises to (§18.4/§4.4): the inline ceiling plus the
/// envelope margin. libnng's ~1 MiB default **silently discards** larger messages
/// (neo-jasp §25.5); nanonext/R raises it internally — only the Rust and C endpoints need
/// this. (Used by the data-runner binary; the orchestrator computes the same value from its
/// configurable `max_inline_payload`.)
#[allow(dead_code)]
pub const RECV_MAX_SIZE: usize = MAX_INLINE_PAYLOAD + RECV_MARGIN;

/// The default view-request chunk budget: ~20 MB of escaped TSV per response
/// (`JASP_VIEW_CHUNK_BYTES`, format doc §1.1). Many chunks, never one big message.
pub const VIEW_CHUNK_BYTES: u64 = 20_000_000;

fn default_view_chunk_bytes() -> u64 {
    VIEW_CHUNK_BYTES
}

/// The envelope every message shares (§18.2), wrapping a type-discriminated [`Message`].
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Envelope {
    /// Protocol version. Currently 1.
    pub v: u32,
    /// Correlation id for this message.
    pub id: String,
    /// The `id` this message answers (responses only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<String>,
    /// Tenancy scope; a single value on desktop.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Names the binary payload encoding (§18.2/§18.3); **required when a binary part is
    /// present** (e.g. `"text/tsv"` on a view result), absent on JSON-only frames.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    /// Orchestrator-stamped epoch-ms.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ts: Option<u64>,

    /// The type-discriminated body; its `type` tag and fields are flattened into the envelope.
    #[serde(flatten)]
    pub body: Message,
}

/// Every message on the wire, discriminated by the `type` field (internally tagged).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
    Work(Work),
    Result(ResultMsg),
    Abort(Abort),
    WorkClose(WorkClose),
    Ping,
    Pong,
    Error(ErrorMsg),
    Register(Register),
    RegisterAck(RegisterAck),
    Hello(Hello),
    Welcome(Welcome),
    Activity(Activity),
    Deregister(Deregister),
    ListModules,
    Modules(ModulesMsg),
    /// Unsolicited dataset-revision push on the data channel (data-edit-design §6).
    DataChanged(DataChanged),
    // The remaining catalog messages are added the same way: one variant + one struct each.
}

/// Frontend → Orchestrator: run a work unit.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Work {
    pub work_id: String,
    pub revision: u64,
    /// Revision to seed incremental recompute from — the last result the frontend is iterating
    /// off. The orchestrator resolves it to a concrete `base_results_dir`; the runner copies that
    /// finished dir into its own `results_<revision>` before running (copy-on-seed). Absent →
    /// full recompute (no seed). The frontend is the authority on revision numbers (§17.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_revision: Option<u64>,
    pub dataset_ids: Vec<String>,
    /// `kind` + `payload`, flattened so they sit at the work level (adjacently tagged).
    #[serde(flatten)]
    pub payload: WorkPayload,
}

/// The kind-specific part of a `work` message (adjacently tagged by `kind`/`payload`).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "payload")]
pub enum WorkPayload {
    #[serde(rename = "analysis_r_classic_jaspbase")]
    AnalysisRClassicJaspbase(AnalysisWork),
    #[serde(rename = "rcode")]
    Rcode(RcodeWork),
    /// Synthesized data-plane work the orchestrator routes to a data lane (§5.4): a
    /// `dataset_open` from the frontend becomes a `kind:"data"` work unit carrying a
    /// `data_open` op. The lane writes the converted Arrow to the orchestrator-assigned
    /// `cache_path` and replies `{schema, rows}` — data never crosses the wire.
    #[serde(rename = "data")]
    Data(DataWork),
}

/// The work/result kind discriminator (§19.1/§19.2) — orchestrator-side bookkeeping where
/// the full kind-specific payload is not needed (routing records, kind-correct synthetic
/// results). Not a wire type: on the wire the kind is the tag of the adjacently-tagged
/// payload enums.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkKind {
    AnalysisRClassicJaspbase,
    Rcode,
    Data,
}

impl WorkPayload {
    /// The kind discriminator of this work (§19.1).
    pub fn kind(&self) -> WorkKind {
        match self {
            WorkPayload::AnalysisRClassicJaspbase(_) => WorkKind::AnalysisRClassicJaspbase,
            WorkPayload::Rcode(_) => WorkKind::Rcode,
            WorkPayload::Data(_) => WorkKind::Data,
        }
    }
}

/// A data-plane work unit (frontend → orchestrator → data lane). The lane is **stateless**:
/// the orchestrator assigns identity at dispatch — for `data_open` it mints the `dataset_id`,
/// fills in the `cache_path` (the lane writes the converted `.arrow` there), and tracks the
/// work → dataset mapping; the lane's terminal result — a `kind:"data"` result the
/// orchestrator fills with the `dataset_id` (§19.2) — is the frontend's "dataset ready". An
/// open is just a work; the orchestrator manages the dataset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DataWork {
    /// The operation to perform (`data_open` / `data_view`).
    pub op: DataOp,
    /// The source file to read (`data_open`: the user's CSV/… file). The lane reads it
    /// directly — the orchestrator never touches the bytes. Unused by `data_view`.
    pub source: String,
    /// **Orchestrator-assigned at dispatch** (identity, not I/O): `data_open` gets a fresh
    /// path the lane writes (`<state_root>/<session>/datasets/<dataset_id>_<revision>.arrow`);
    /// `data_view` gets the dataset's `current_path` injected so the lane can slice it.
    /// The sender leaves it empty.
    pub cache_path: String,
    /// Routing key selecting the lane + parser (`"csv"`, later `"spss"`, `"arrow"`, …).
    /// Meaningful for `data_open`; `data_view` is format-agnostic (it reads the cache).
    pub format: String,
    /// Ingestion settings captured at open; forwarded verbatim on every op.
    pub ingest: IngestParams,

    // ── data_view window (data-view-format.md §1.1; defaults keep the data_open shape valid) ──
    /// First row of the window.
    #[serde(default)]
    pub row_offset: u64,
    /// Window length; null = to the end of the dataset (still capped by `max_bytes`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_limit: Option<u64>,
    /// Columns to serve, by DISPLAY name; null = all columns in schema order.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub columns: Option<Vec<String>>,
    /// Budget for THIS response, counted exactly on the escaped TSV bytes. The lane stops
    /// at a row boundary (whole rows only), with a progress guarantee of ≥ 1 row.
    #[serde(default = "default_view_chunk_bytes")]
    pub max_bytes: u64,
    /// Locale rendering spec — the lane is the only float→string converter (format doc §1.2).
    /// Absent = lane default: `.` decimal, no grouping, precision 10.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub render: Option<ViewRender>,

    // ── data_edit additions (data-edit-design §2–§3) ──
    /// Present iff `op` is `data_edit`: the adjacently-nested edit operation. Defaults keep
    /// the `data_open`/`data_view` shapes valid (the established pattern — `row_offset`/
    /// `render` ride the same way). The block's forward cells ride the frame's binary part
    /// in the §1.2 TSV grammar; for `apply_inverse` the tail is the inverse's Arrow-IPC bytes.
    /// Boxed: the op family is bigger than the rest of the work combined, and only `data_edit`
    /// ever carries it — open/view works shouldn't pay its size on every clone.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edit: Option<Box<EditOp>>,
}

/// One edit operation inside a `data_edit` work unit (data-edit-design §3), internally
/// tagged by `op`. The op set (v1): `insert_block`, `insert_rows`, `insert_cols`,
/// `delete_rows`, `delete_cols`, `schema_change`, plus `apply_inverse` — the undo/redo entry
/// point (§5), not user-facing.
///
/// Semantics in one line each: `insert_block` = paste (overwrite/expand, holes → null,
/// anchor may name not-yet-existing rows/cols); `insert_rows`/`insert_cols` = shift down/
/// right with null fill; `delete_rows`/`delete_cols` = shift up/left; `schema_change` =
/// metadata ONLY (rename, retype, levels, column order) — never the column set or row count,
/// so there is exactly one way to say "add a column".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum EditOp {
    /// Paste at anchor `(row, col)`; the cells ride the frame's binary part (§1.2 TSV).
    /// Overwrites covered cells; the extent grows to `max(current, anchor + shape)`; holes
    /// → null; overflow columns are created (inferred per D5 unless declared, named per
    /// `jasp_column_names`, overridable via `target_schema`).
    InsertBlock {
        row: u64,
        col: u64,
        /// The declarative postcondition (D4): absent/null → the lane recomputes
        /// (absorption/promotion, §4); present → adhere-or-error (`schema_mismatch`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target_schema: Option<Value>,
    },
    /// Shift rows down at `at` by `count`; null fill.
    InsertRows { at: u64, count: u64 },
    /// Shift rows up: removes `[at, at+count)`. `at+count ≤ rows` else `range` error.
    DeleteRows { at: u64, count: u64 },
    /// Insert columns at `at` (shift right). One entry per column — an open-schema column
    /// spec; `type: null` (absent) = the lane infers (D5, the csv2arrow path).
    InsertCols {
        at: u64,
        columns: Vec<NewColumnSpec>,
    },
    /// Shift left: removes `count` columns at `at`.
    DeleteCols { at: u64, count: u64 },
    /// Metadata only: rename, retype, set/reorder levels, reorder columns — the complete
    /// post-edit column list as the frontend intends it. **Never changes the column set or
    /// row count.**
    SchemaChange { target_schema: Value },
    /// Undo/redo entry point (§5): submits a previously returned inverse blob VERBATIM —
    /// `inverse` echoes the result's meta; the blob's Arrow-IPC bytes ride the frame's
    /// binary part. The lane validates (format known, embedded base revision acceptable)
    /// and applies atomically; the result carries its own inverse (redo material) — symmetric.
    ApplyInverse { inverse: InverseMeta },
}

/// One column of an `insert_cols` edit (data-edit-design §3): an open-schema entry.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct NewColumnSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// `"scale" | "ordinal" | "nominal"`; absent/null = the lane infers (D5).
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub column_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub levels: Option<Vec<String>>,
}

/// Locale rendering spec for `data_view` requests (data-view-format.md §1.2): explicit
/// separator **characters**, not a locale id — the frontend's `QLocale` is the authority and
/// the lane stays dependency-free. A locale/separator change is a drop-and-refill on the
/// frontend (the buffer's strings are locale-baked).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ViewRender {
    /// Decimal separator.
    #[serde(default = "ViewRender::default_decimal")]
    pub decimal: String,
    /// Grouping (thousands) separator; `""` = no grouping.
    #[serde(default)]
    pub thousands: String,
    /// Significant digits — C/Qt `'g'` semantics (legacy parity at 10).
    #[serde(default = "ViewRender::default_precision")]
    pub precision: u32,
}

impl ViewRender {
    fn default_decimal() -> String {
        ".".to_string()
    }
    fn default_precision() -> u32 {
        10
    }
}

impl Default for ViewRender {
    fn default() -> Self {
        ViewRender {
            decimal: Self::default_decimal(),
            thousands: String::new(),
            precision: Self::default_precision(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnalysisWork {
    pub module: String,
    pub module_version: String,
    pub analysis: String,
    /// Opaque analysis options — the orchestrator never interprets them.
    pub options: Value,
    /// Preload the used columns before the run (from the module's `AnalysisEntry`;
    /// missing -> true, compat). Opaque to the orchestrator — the R runner uses it to choose
    /// between a pruned aliased preload frame and lazy on-demand reads
    /// (HANDOVER-runner-data-pruning.md §3.1).
    #[serde(
        rename = "preloadData",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub preload_data: Option<bool>,
    pub settings: Settings,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Settings {
    pub ppi: u32,
    #[serde(rename = "numDecimals")]
    pub num_decimals: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RcodeWork {
    pub code: String,
    /// Opaque environment for the code.
    pub env: Value,
}

/// Orchestrator → Frontend: a result (possibly one of a stream) for a work unit. This is the
/// **same message the runner sends** (§19.4); the orchestrator forwards it, filling only the
/// identity & location fields of the typed payload (§19.2, division of labor).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResultMsg {
    pub work_id: String,
    pub revision: u64,
    pub status: Status,
    /// `kind` + `payload`, flattened so they sit at the result level (adjacently tagged) —
    /// mirrors the work side (§19.1).
    #[serde(flatten)]
    pub payload: ResultPayload,
    /// Module version that produced this result (provenance, §19.4); analysis results.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub module_version: Option<String>,
    /// Human-readable detail on error/aborted (§19.2).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// The kind-specific part of a `result` message (adjacently tagged by `kind`/`payload`) —
/// mirrors [`WorkPayload`]. One work kind, one result shape (§19.2 *Result payloads by kind*).
/// Division of labor: **producers fill content; the orchestrator fills identity & location** —
/// as typed fields on the payload, never by surgery on an opaque tree.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", content = "payload")]
pub enum ResultPayload {
    #[serde(rename = "analysis_r_classic_jaspbase")]
    AnalysisRClassicJaspbase(AnalysisResult),
    /// Terminal result of a dataset-open work — replaces the removed `dataset_ready` message:
    /// the ready notification IS this result.
    #[serde(rename = "data")]
    Data(DataResult),
    /// Reserved — `{output, value, …}`, pinned when the rcode work kind lands (§19.2).
    #[serde(rename = "rcode")]
    Rcode(Value),
}

/// The `kind:"analysis_r_classic_jaspbase"` result payload (§19.2).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnalysisResult {
    /// The jaspResults tree — **opaque to the orchestrator**: forwarded verbatim, never
    /// interpreted or mutated. On failure this is the error tree `{error, errorMessage, title}`.
    pub results: Value,
    /// Absolute path of the revision dir holding this result's file artifacts
    /// (`<dir_root>/<session>/<work_id>/results_<rev>`). **Orchestrator-filled, wire-only**
    /// bootstrap for asset resolution — never persisted (§19.2, the asset-path rule: artifact
    /// references inside `results` stay relative to this dir on disk).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub results_dir: Option<String>,
    /// Optional artifact manifest, paths relative to `results_dir` (the runner computes it from
    /// its keep-list; for save/archive/GC). Shape reserved; not filled in the alpha.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub images: Option<Vec<String>>,
}

/// The `kind:"data"` result payload (§19.2) — the terminal result of a dataset-open work.
/// Schema is content, not routing metadata: it flows lane → frontend here; the dataset
/// registry does not cache it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DataResult {
    /// The minted identity the frontend references in later work (`dataset_ids`).
    /// **Orchestrator-filled** at the terminal result; the lane leaves it empty.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    /// The dataset revision **at dispatch** — orchestrator-filled, stamped on every view
    /// result so the frontend drops stale chunks (data-view-design §6.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_revision: Option<u64>,
    /// TOTAL row count of the dataset at serve time (lane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
    /// The frontend's column view: `[{name, display_name, type, levels?, all_integer?}]` (§24).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    /// Present when `status` is a failure (lane).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,

    // ── data_view additions (the cells themselves ride the frame's binary part, §18.1) ──
    /// First row carried in the binary part (lane, `data_view`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_offset: Option<u64>,
    /// Rows carried in the binary part (lane, `data_view`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub row_count: Option<u64>,
    /// Stopped at `max_bytes` before `row_limit`/end (lane, `data_view`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,

    // ── data_edit additions (data-edit-design §2–§3) ──
    /// The lane-computed invalidation descriptor of a completed edit — what `data_changed`
    /// will tell every holder is stale (§6). Lane → orchestrator transport ONLY: the
    /// orchestrator moves it into the broadcast and strips it from the forwarded result
    /// (D6: results carry undo material; `data_changed` carries view-consistency material).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalidation: Option<Invalidation>,
    /// Structured detail iff `status` is `validationError` (§3): one entry per failing
    /// rule, ≤10 example row indices each. Filled by the lane for edit validation failures;
    /// by the orchestrator for the D11 `stale_edit` dispatch check.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validation: Option<Vec<ValidationIssue>>,
    /// The undo material of a completed edit (D1/D10): the BLACK BOX. The `(meta, bytes)`
    /// pair is ONE inseparable unit — this object plus the frame's Arrow-IPC binary tail.
    /// The frontend stores it verbatim and resubmits it verbatim via `apply_inverse`; it
    /// never interprets, merges, or serializes it. Only `format` may be observed (log/size).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub inverse: Option<InverseMeta>,
}

/// The meta half of an inverse blob (data-edit-design §2, §5, D10). Lane-filled on the
/// edit result; echoed verbatim by the frontend on `apply_inverse`. `base_revision` is the
/// revision of the state the edit was computed against — lane-embedded provenance, checked
/// defensively at apply time (the revision itself only ever climbs; §5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct InverseMeta {
    /// The only field the frontend may observe. v1: `"arrow_ipc_v1"` — old cells as
    /// Arrow-IPC (LZ4_FRAME) in the frame's binary part. Future encodings (lane-side
    /// tokens, overlay references) are new values in this slot — never a contract change.
    pub format: String,
    pub base_revision: u64,
    /// The restore program (op kinds, anchors, old schema fragment) — lane internals,
    /// non-contractual JSON.
    pub ops: Value,
}

/// The §18.3 envelope `format` naming an inverse tail: Arrow-IPC, LZ4_FRAME — the same
/// codec stack as the dataset caches (data-edit-design §10). Unused until the edit engine
/// answers results (d3+), like `RECV_MAX_SIZE` before its consumer landed.
#[allow(dead_code)]
pub const FORMAT_ARROW_IPC: &str = "arrow/ipc";

/// One validation failure on a `validationError` data result (data-edit-design §3). Codes:
/// `anchor` | `range` | `coercion` | `level_unknown` | `level_in_use` | `schema_mismatch` |
/// `stale_edit`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct ValidationIssue {
    /// Display name of the offending column; absent when the failure is not column-scoped
    /// (e.g. `stale_edit`, a bad anchor).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    /// One of the codes above.
    pub code: String,
    /// Human-readable detail (frontend may surface verbatim).
    pub message: String,
    /// How many cells/rows failed (when countable).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u64>,
    /// ≤10 example row indices.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<Vec<u64>>,
}

/// Which rows a `data_changed` invalidates (data-edit-design §6 — the descriptor is
/// normative; whole-buffer drop is just the v1 frontend implementation). Exactly one of:
///
/// | wire shape | meaning |
/// |---|---|
/// | `{"rows_from": r}` | rows `r..` to the end — includes growth |
/// | `{"rows_from": r, "rows_to": t}` | rows `r..t` (`rows_to` exclusive) |
/// | `{"all": true}` | every row, and/or any column-set/order/type/value-levels change |
/// | `{}` | schema-only (e.g. a rename): no row is stale, headers change via `schema` |
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Invalidation {
    /// Present — and `true` — only for whole-dataset invalidation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all: Option<bool>,
    /// First stale row (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_from: Option<u64>,
    /// End of the stale range (EXCLUSIVE); only meaningful alongside `rows_from`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows_to: Option<u64>,
}

/// Why a `data_changed` fired — diagnostics only, never load-bearing (data-edit-design §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    /// A user edit (the `data_edit` family, including `apply_inverse`).
    Edit,
    /// A derived-data recompute (`data_update`; future).
    Derived,
    /// External sync reached the cache (future; subsumes legacy DB-interval polling).
    External,
}

/// The `cause` block of a `data_changed` (data-edit-design §6).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DataChangedCause {
    pub kind: ChangeKind,
    /// The triggering work unit, when there was one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
}

/// Orchestrator → Frontend: unsolicited push on the data channel (like `modules`) telling
/// every holder of a dataset that its revision bumped (data-edit-design §6 — Increment 4,
/// the prerequisite return leg of the edit era). One uniform buffer-invalidation path for
/// every revision bump, whatever caused it. The lane computes the invalidation once; the
/// orchestrator stamps identity and broadcasts. Frontend rule: per-dataset,
/// `data_changed` arrives in revision order, after the triggering result, on the same
/// channel; `revision ≤ current` → ignore (idempotent).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DataChanged {
    pub dataset_id: String,
    /// The NEW revision post-apply (edits stamp post-apply; views stamp at dispatch —
    /// different semantics on `DataResult::dataset_revision`, documented there).
    pub dataset_revision: u64,
    /// The new row total — always carried in practice (the lane knows it post-apply).
    pub rows: Option<u64>,
    /// The canonical post-edit schema — present IFF the schema changed (the lane decides).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<Value>,
    /// Always present (possibly `{}` for a schema-only change).
    pub invalidation: Invalidation,
    pub cause: DataChangedCause,
}

/// Result lifecycle status (wire values are camelCase, matching the existing engine strings).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Running,
    Complete,
    FatalError,
    ValidationError,
    /// **v2-era (additive; `v` stays 1):** the runner unwound cooperatively at a checkpoint
    /// after an `abort` — a TERMINAL status (it returns a credit). Produced by the runner's
    /// checkpoint drain (orchestrator-v2 design §7); a raced normal completion instead
    /// carries whatever status the run actually produced, and the router discards it by
    /// revision.
    Aborted,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Abort {
    pub work_id: String,
}

/// Frontend → Orchestrator: discard a work unit (§19.1). The frontend is the authority on "this
/// `work_id` is consumed forever" (its analysis was closed). The orchestrator aborts any in-flight
/// work and reclaims the work's workspace. Idempotent: closing an unknown or already-closed work
/// is a no-op.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct WorkClose {
    pub work_id: String,
    /// Present → close only this revision (`results_<revision>`), aborting it if it is the
    /// in-flight one. Absent → close the whole work (every revision). Idempotent either way.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ErrorMsg {
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
}

/// Ingestion parameters carried by a data `work` (the frontend sends them on the open;
/// the orchestrator stores them on the dataset entry and they ride every data op, since
/// the lane is stateless).
/// They **change the result** — locale decides how numbers are parsed/canonicalized and
/// `threshold` decides ordinal-vs-scale — so they are baked into the cached Arrow at
/// conversion. Changing them is a re-open, not an edit. Maps 1:1 onto the `csv2arrow`
/// lane's knobs.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct IngestParams {
    /// Routing key: `"csv"` | `"spss"` | `"excel"` | `"arrow"` | …
    #[serde(default)]
    pub format: String,
    /// Locale decimal separator ('.' or ',').
    #[serde(default = "IngestParams::default_decimal_sep")]
    pub decimal_sep: char,
    /// Locale thousands separator (',' / '.' / ' ' / none).
    #[serde(default)]
    pub thousands_sep: Option<char>,
    /// Ordinal-vs-scale threshold (JASP "threshold for scale").
    #[serde(default = "IngestParams::default_threshold")]
    pub threshold: usize,
    /// Null spellings (the empty string is included so blank cells read as null).
    #[serde(default = "IngestParams::default_nulls")]
    pub nulls: Vec<String>,
    /// Value-sort dictionaries with ≤ this many distinct values.
    #[serde(default = "IngestParams::default_sort_limit")]
    pub sort_limit: usize,
}

impl IngestParams {
    fn default_decimal_sep() -> char {
        '.'
    }
    fn default_threshold() -> usize {
        10
    }
    fn default_nulls() -> Vec<String> {
        vec![String::new(), "NA".into(), "NaN".into()]
    }
    fn default_sort_limit() -> usize {
        2000
    }
}

impl Default for IngestParams {
    fn default() -> Self {
        IngestParams {
            format: String::new(),
            decimal_sep: Self::default_decimal_sep(),
            thousands_sep: None,
            threshold: Self::default_threshold(),
            nulls: Self::default_nulls(),
            sort_limit: Self::default_sort_limit(),
        }
    }
}

/// Runner → Orchestrator: capability advertisement (Section 9.3, Section 19.5).
///
/// A runner advertises one [`Capability`] entry per work `kind` it can satisfy. This is the
/// single umbrella for "what can this runner do" — analysis modules, data-plane operations,
/// and raw-R all live in the same `capabilities` list, each tagged by `kind`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Register {
    /// Optional *hint* id (e.g. a dev-machine label for pinning). The orchestrator is the
    /// authority: it assigns the canonical `runner_id` and returns it in `register_ack`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_id: Option<String>,
    /// The capabilities this runner offers — one entry per work `kind` it can satisfy (§9.3).
    #[serde(default)]
    pub capabilities: Vec<Capability>,
    /// Higher = preferred when multiple runners can satisfy the same work.
    #[serde(default)]
    pub priority: u32,
    /// **v2-era (additive; `v` stays 1):** the executor's credit window — how many work
    /// units it may hold at once (`register` = the boot credit grant). Local R runners → 1
    /// (R is single-threaded); the Rust data worker → 1 (one job at a time, as built);
    /// remote pools → N (reserve). Classic ignores the field (its runners default to 1 and
    /// it never checks) — the default keeps old peers wire-compatible.
    #[serde(default = "Register::default_slots")]
    pub slots: u32,
    /// Hardware/runtime environment (`r_version`, `gpu`, `high_memory`) for resource-matched
    /// routing (§9.5). Not a capability — these *qualify* how a capability runs.
    #[serde(default)]
    pub environment: Value,
}

impl Register {
    fn default_slots() -> u32 {
        1
    }
}

/// A capability a runner advertises — the routing-key projection of a work `kind` (§9.3).
///
/// Mirrors the work `kind` enum one-to-one: a work unit *requests* a computation; a capability
/// *advertises* the ability to satisfy such requests. Each variant carries only the **routing
/// key** for its kind (analysis name+version; data op+formats; rcode is unconstrained for now),
/// never the full request payload — a runner that can run Anova can run it with *any* options,
/// so it advertises `{name, version}`, not an `AnalysisWork`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Capability {
    /// Can run an analysis module — version-matched routing (§9.4). `{name, version}` is the
    /// routing key; `base_uri` is *discovery* metadata, NOT part of the key: the directory (a
    /// `file://` URI now, `http://` for the webapp later) the frontend lazy-loads this module's
    /// assets from (Description.qml, qml/, icons/, help/). A runner that computes a module but
    /// does not vouch for its assets omits it — it is still routed to, but left out of the
    /// module catalog handed to frontends.
    #[serde(rename = "analysis_r_classic_jaspbase")]
    AnalysisRClassicJaspbase {
        name: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        base_uri: Option<String>,
    },
    /// Can run raw R code. No routing key for now (arbitrary routing); future: `r_version`
    /// constraints. Empty object on the wire.
    Rcode {},
    /// Can perform a data-plane operation. `op` discriminates the operation; `formats` is
    /// meaningful **only** for `data_open` (the source formats this runner can open) and is
    /// ignored for the format-agnostic ops.
    Data {
        op: DataOp,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        formats: Option<Vec<String>>,
    },
}

/// The data-plane operations — the `op` of a `data` capability (and of `data` work, §19.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DataOp {
    #[serde(rename = "data_open")]
    Open,
    #[serde(rename = "data_edit")]
    Edit,
    #[serde(rename = "data_close")]
    Close,
    #[serde(rename = "data_update")]
    Update,
    /// A windowed view of a cached dataset (data-view-format.md): the lane slices the
    /// Feather and renders the window as escaped TSV in the frame's binary part.
    #[serde(rename = "data_view")]
    View,
}

/// Orchestrator → Runner: registration response (§19.5).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RegisterAck {
    pub ok: bool,
    /// The orchestrator-assigned canonical runner id (present when `ok`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub runner_id: Option<String>,
    /// Dedicated PAIR data-channel URL (present when `ok`). The runner dials this after
    /// receiving the ack; all work/result/abort flows on this channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_url: Option<String>,
    /// Suggested rate-limit (ms) for the runner's opportunistic `activity` pings (§19.4, §25.4).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity_min_interval_ms: Option<u64>,
    /// Reason for rejection (`ok=false` only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Frontend → Orchestrator: connection handshake on the control endpoint (§19.5).
///
/// The orchestrator is the authority on identity: it **assigns** the canonical `session_id`
/// (`s-N`) and returns it in `welcome`. The frontend's optional `client_id` is a stable hint
/// for reconnect idempotency (deferred).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Hello {
    /// Stable frontend id (reconnect hint; the orchestrator maps it to a session).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    /// Frontend/app version (diagnostics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

/// Orchestrator → Frontend: connection accepted (§19.5).
///
/// The assigned `session_id` is carried on the [`Envelope`](crate::Envelope)'s `session_id` (the
/// tenancy scope every message carries), NOT as a field here: a `session_id` on both the envelope
/// and this flattened body would collide on the wire (one JSON key), and serde would feed it to the
/// envelope, leaving this body's copy `None`. Wire format is unchanged — clients read the top-level
/// `session_id`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Welcome {
    pub ok: bool,
    /// Dedicated PAIR data-channel URL (present when `ok`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub channel_url: Option<String>,
    /// Reason when `ok` is false.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Runner → Orchestrator: opportunistic liveness signal (§19.4, §25.4).
///
/// A `result` is itself an activity signal; `activity` covers busy-but-quiet periods (e.g.
/// a long computation with no intermediate output). Rate-limited by `activity_min_interval_ms`.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Activity {
    /// If present, signals progress on this specific work unit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<String>,
}

/// Runner → Orchestrator: graceful leave (§19.5). Alternative to just closing the socket.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Deregister {
    pub runner_id: String,
}

// `Message::ListModules` is a unit variant (like `Ping`): a frontend's request for the current
// module catalog, answered with a [`ModulesMsg`] whose `reply_to` echoes the query's `id`.
// Subsequent changes to the available-module set are pushed as unsolicited `modules` messages
// (`reply_to` absent).

/// Orchestrator → Frontend: the module catalog. Sent three ways, all handled identically by
/// the frontend ("replace your menu"): unsolicited as the **first frame on the data channel**
/// (buffered at channel setup in `handle_hello`), unsolicited on every actual catalog change,
/// and as the correlated reply to `list_modules` (`reply_to` echoes the query's `id` — the
/// only case where `reply_to` is present). Deliberately lightweight: `{name, version,
/// base_uri}` per module; the frontend parses `Description.qml` at the base URI for ribbon
/// metadata (title, icon, menu placement) and lazy-loads everything else relative to it.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ModulesMsg {
    pub modules: Vec<ModuleInfo>,
}

/// One discoverable module. `base_uri` is the directory found by scanning (the orchestrator's
/// libset scan or a runner's advertisement), always trailing-slash so assets resolve by simple
/// concatenation. The wire carries nothing richer — no titles, icons, or menus.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, JsonSchema)]
pub struct ModuleInfo {
    pub name: String,
    pub version: String,
    pub base_uri: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The four normative invalidation shapes (data-edit-design §6) serialize EXACTLY —
    /// the frontend pattern-matches the descriptor's wire form, not just its semantics.
    #[test]
    fn invalidation_serializes_the_four_shapes() {
        let none = Invalidation {
            all: None,
            rows_from: None,
            rows_to: None,
        };
        assert_eq!(serde_json::to_string(&none).unwrap(), "{}");

        let tail = Invalidation {
            all: None,
            rows_from: Some(1234),
            rows_to: None,
        };
        assert_eq!(
            serde_json::to_string(&tail).unwrap(),
            r#"{"rows_from":1234}"#
        );

        let range = Invalidation {
            all: None,
            rows_from: Some(1234),
            rows_to: Some(1300),
        };
        assert_eq!(
            serde_json::to_string(&range).unwrap(),
            r#"{"rows_from":1234,"rows_to":1300}"#
        );

        let all = Invalidation {
            all: Some(true),
            rows_from: None,
            rows_to: None,
        };
        assert_eq!(serde_json::to_string(&all).unwrap(), r#"{"all":true}"#);

        // Round-trip: every shape parses back to itself.
        for inv in [none, tail, range, all] {
            let back: Invalidation =
                serde_json::from_str(&serde_json::to_string(&inv).unwrap()).unwrap();
            assert_eq!(back, inv);
        }
    }

    /// `data_changed` rides the flattened envelope with the `type:"data_changed"` tag
    /// (the internal tag on `Message`), and its fields land at the top level.
    #[test]
    fn data_changed_envelope_carries_the_wire_tag() {
        let env = Envelope {
            v: 1,
            id: "orch-dchg-ds-4-7".to_string(),
            reply_to: None,
            session_id: Some("s-1".to_string()),
            format: None,
            ts: None,
            body: Message::DataChanged(DataChanged {
                dataset_id: "ds-4".to_string(),
                dataset_revision: 7,
                rows: Some(50001),
                schema: None,
                invalidation: Invalidation {
                    all: None,
                    rows_from: Some(1234),
                    rows_to: None,
                },
                cause: DataChangedCause {
                    kind: ChangeKind::Edit,
                    work_id: Some("w-12".to_string()),
                },
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&env).unwrap();
        assert_eq!(v["type"], "data_changed");
        assert_eq!(v["dataset_id"], "ds-4");
        assert_eq!(v["dataset_revision"], 7);
        assert_eq!(v["rows"], 50001);
        assert_eq!(v["invalidation"], serde_json::json!({ "rows_from": 1234 }));
        assert_eq!(
            v["cause"],
            serde_json::json!({ "kind": "edit", "work_id": "w-12" })
        );
        assert!(v.get("schema").is_none(), "schema absent IFF unchanged");

        // Round-trip through the envelope: the frontend's parse path.
        let json = serde_json::to_string(&env).unwrap();
        let back: Envelope = serde_json::from_str(&json).unwrap();
        match back.body {
            Message::DataChanged(c) => {
                assert_eq!(c.dataset_id, "ds-4");
                assert_eq!(c.cause.kind, ChangeKind::Edit);
            }
            other => panic!("expected data_changed, got {other:?}"),
        }
    }

    /// The `data_edit` op family round-trips through the envelope with the frozen wire
    /// shape (data-edit-design §2–§3): `payload.edit` adjacently present, `op` tags
    /// snake_case, `target_schema` absent when null (D4), and defaults keep the
    /// open/view shapes valid (no `edit` key at all).
    #[test]
    fn edit_ops_round_trip_with_the_wire_shape() {
        let mk = |edit: EditOp| Envelope {
            v: 1,
            id: "w-1".into(),
            reply_to: None,
            session_id: None,
            format: None,
            ts: None,
            body: Message::Work(Work {
                work_id: "w-1".into(),
                revision: 6,
                base_revision: None,
                dataset_ids: vec!["ds-4".into()],
                payload: WorkPayload::Data(DataWork {
                    op: DataOp::Edit,
                    source: String::new(),
                    cache_path: String::new(),
                    format: String::new(),
                    ingest: Default::default(),
                    row_offset: 0,
                    row_limit: None,
                    columns: None,
                    max_bytes: VIEW_CHUNK_BYTES,
                    render: None,
                    edit: Some(Box::new(edit)),
                }),
            }),
        };

        // insert_block: anchor + absent target_schema (lane recomputes).
        let block = EditOp::InsertBlock {
            row: 1234,
            col: 2,
            target_schema: None,
        };
        let v: serde_json::Value = serde_json::to_value(mk(block.clone())).unwrap();
        assert_eq!(v["payload"]["edit"]["op"], "insert_block");
        assert_eq!(v["payload"]["edit"]["row"], 1234);
        assert_eq!(v["payload"]["edit"]["col"], 2);
        assert!(v["payload"]["edit"].get("target_schema").is_none());
        let back: Envelope = serde_json::from_value(v).unwrap();
        match back.body {
            Message::Work(Work {
                payload: WorkPayload::Data(d),
                ..
            }) => assert_eq!(d.edit, Some(Box::new(block))),
            other => panic!("expected work, got {other:?}"),
        }

        // Every variant round-trips. (One deliberate normalization: an explicit
        // `target_schema: null` parses as `None` — D4 defines absent ≡ null ≡ lane
        // recomputes, so the two never need distinguishing on the Rust side either;
        // the declared case is exercised by the engine tests' array schemas, d3+.)
        let variants = vec![
            EditOp::InsertBlock {
                row: 0,
                col: 0,
                target_schema: None,
            },
            EditOp::InsertRows { at: 10, count: 5 },
            EditOp::DeleteRows { at: 10, count: 5 },
            EditOp::InsertCols {
                at: 1,
                columns: vec![NewColumnSpec {
                    name: "score".into(),
                    display_name: None,
                    column_type: None, // D5: infer
                    levels: None,
                }],
            },
            EditOp::DeleteCols { at: 1, count: 2 },
            EditOp::SchemaChange {
                target_schema: Value::Null,
            },
            EditOp::ApplyInverse {
                inverse: InverseMeta {
                    format: "arrow_ipc_v1".into(),
                    base_revision: 6,
                    ops: Value::Null,
                },
            },
        ];
        for edit in variants {
            let json = serde_json::to_string(&mk(edit.clone())).unwrap();
            let back: Envelope = serde_json::from_str(&json).unwrap();
            match back.body {
                Message::Work(Work {
                    payload: WorkPayload::Data(d),
                    ..
                }) => assert_eq!(d.edit, Some(Box::new(edit)), "round-trip broke for {json}"),
                other => panic!("expected work, got {other:?}"),
            }
        }

        // Defaults keep the open/view shapes valid: no `edit` key parses as None.
        let open = r#"{"v":1,"id":"w-2","type":"work","work_id":"w-2","revision":0,
            "dataset_ids":[],"kind":"data","payload":{"op":"data_open","source":"/a.csv",
            "cache_path":"","format":"csv","ingest":{}}}"#;
        let back: Envelope = serde_json::from_str(open).unwrap();
        match back.body {
            Message::Work(Work {
                payload: WorkPayload::Data(d),
                ..
            }) => {
                assert_eq!(d.op, DataOp::Open);
                assert_eq!(d.edit, None, "absent edit must parse as None");
            }
            other => panic!("expected work, got {other:?}"),
        }
    }

    /// The edit result's undo material: `inverse` rides the payload, the IPC bytes ride the
    /// frame's binary tail (never JSON) — the meta alone must round-trip (D10).
    #[test]
    fn edit_result_carries_inverse_meta() {
        let env = Envelope {
            v: 1,
            id: "r-1".into(),
            reply_to: None,
            session_id: None,
            format: Some(FORMAT_ARROW_IPC.into()),
            ts: None,
            body: Message::Result(ResultMsg {
                work_id: "w-1".into(),
                revision: 6,
                status: Status::Complete,
                payload: ResultPayload::Data(DataResult {
                    dataset_id: Some("ds-4".into()),
                    dataset_revision: None,
                    rows: None,
                    schema: None,
                    error_message: None,
                    row_offset: None,
                    row_count: None,
                    truncated: None,
                    invalidation: None,
                    validation: None,
                    inverse: Some(InverseMeta {
                        format: "arrow_ipc_v1".into(),
                        base_revision: 6,
                        ops: serde_json::json!([{ "restore": "cells", "row": 1234 }]),
                    }),
                }),
                module_version: None,
                message: None,
            }),
        };
        let v: serde_json::Value = serde_json::to_value(&env).unwrap();
        assert_eq!(v["payload"]["inverse"]["format"], "arrow_ipc_v1");
        assert_eq!(v["payload"]["inverse"]["base_revision"], 6);
        assert_eq!(v["format"], FORMAT_ARROW_IPC);
        let back: Envelope = serde_json::from_value(v).unwrap();
        match back.body {
            Message::Result(ResultMsg {
                payload: ResultPayload::Data(d),
                ..
            }) => assert_eq!(d.inverse.unwrap().base_revision, 6),
            other => panic!("expected result, got {other:?}"),
        }
    }
}
