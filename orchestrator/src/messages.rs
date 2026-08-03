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
//! * `payload.results` is deliberately an opaque `serde_json::Value`: the orchestrator forwards
//!   the jaspResults tree verbatim and never interprets it.
//!
//! Only the messages the alpha needs are fleshed out; the rest of the catalog (data_edit,
//! dataset_close, data_changed, form_reload, …) follows the identical pattern — one variant +
//! one struct each.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Orchestrator-stamped epoch-ms.
    #[serde(skip_serializing_if = "Option::is_none")]
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
    #[serde(rename = "analysis")]
    Analysis(AnalysisWork),
    #[serde(rename = "rcode")]
    Rcode(RcodeWork),
    /// Synthesized data-plane work the orchestrator routes to a data lane (§5.4): a
    /// `dataset_open` from the frontend becomes a `kind:"data"` work unit carrying a
    /// `data_open` op. The lane writes the converted Arrow to the orchestrator-assigned
    /// `cache_path` and replies `{schema, rows}` — data never crosses the wire.
    #[serde(rename = "data")]
    Data(DataWork),
}

/// A data-plane work unit (frontend → orchestrator → data lane). The lane is **stateless**:
/// the orchestrator assigns identity at dispatch — for `data_open` it mints the `dataset_id`,
/// fills in the `cache_path` (the lane writes the converted `.arrow` there), and tracks the
/// work → dataset mapping; the lane's terminal result (with the `dataset_id` spliced in by
/// the orchestrator) is the frontend's "dataset ready". An open is just a work; the
/// orchestrator manages the dataset.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DataWork {
    /// The operation to perform (`data_open` in this increment).
    pub op: DataOp,
    /// The source file to read (`data_open`: the user's CSV/… file). The lane reads it
    /// directly — the orchestrator never touches the bytes.
    pub source: String,
    /// **Orchestrator-assigned at dispatch** (identity, not I/O — the lane writes the file);
    /// the sender leaves it empty: `<state_root>/<session>/datasets/<dataset_id>_<revision>.arrow`.
    pub cache_path: String,
    /// Routing key selecting the lane + parser (`"csv"`, later `"spss"`, `"arrow"`, …).
    /// Meaningful for `data_open`.
    pub format: String,
    /// Ingestion settings captured at open; forwarded verbatim on every op.
    pub ingest: IngestParams,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AnalysisWork {
    pub module: String,
    pub module_version: String,
    pub analysis: String,
    /// Opaque analysis options — the orchestrator never interprets them.
    pub options: Value,
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

/// Orchestrator → Frontend: a result (possibly one of a stream) for a work unit.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResultMsg {
    pub work_id: String,
    pub revision: u64,
    pub status: Status,
    pub payload: ResultPayload,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResultPayload {
    /// The jaspResults tree — opaque; forwarded verbatim, never interpreted.
    pub results: Value,
}

/// Result lifecycle status (wire values are camelCase, matching the existing engine strings).
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum Status {
    Running,
    Complete,
    FatalError,
    ValidationError,
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
    /// Hardware/runtime environment (`r_version`, `gpu`, `high_memory`) for resource-matched
    /// routing (§9.5). Not a capability — these *qualify* how a capability runs.
    #[serde(default)]
    pub environment: Value,
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
    Analysis {
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub enum DataOp {
    #[serde(rename = "data_open")]
    Open,
    #[serde(rename = "data_edit")]
    Edit,
    #[serde(rename = "data_close")]
    Close,
    #[serde(rename = "data_update")]
    Update,
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
