//! NEO JASP orchestrator — per-channel, transport-agnostic, event-driven broker.
//!
//! Implements the broker/router specified in `refactor_design/orchestrator-design.md`:
//!
//! * **One REQ/REP control endpoint** (`JASP_ORCH_URL`) — handshake *only*. Every peer phones in
//!   here (REQ dial), sends `hello`/`register`, and receives `welcome`/`register_ack` carrying a
//!   dedicated data-channel URL. REP auto-routes each reply to its requester.
//! * **Per-peer PAIR v1 data channels** — allocated at handshake, scheme-matched to the control
//!   URL (tcp → ephemeral port, ipc → unique path, inproc → tests). All `work`/`result`/`abort`
//!   flows here.
//! * **Concurrency — a single-threaded router.** NNG `Aio` callbacks do the bare minimum on NNG's
//!   pool threads: recv → re-arm → forward the message over an `mpsc` channel to one dedicated
//!   **router thread**. The router thread owns *all* broker state (registries, correlation table,
//!   channel handles) as plain `HashMap`s — no locks — and performs every registration, routing,
//!   eviction, and hang scan sequentially. Blocking filesystem reclamation (`rm -r` of a workspace)
//!   is offloaded to a single **janitor** thread so the router never blocks. This makes the routing
//!   order total and the deadlock class (a blocking send waiting on a busy recv callback)
//!   impossible: callbacks never route and are always re-armed. The pattern is the classic
//!   single-threaded event loop (redis/nginx/node) — right-sized for a desktop broker with a
//!   handful of peers.
//! * **Correlation** — `work_id` is session-scoped; the routing key is `(session_id, work_id)`.
//!   The orchestrator stamps `session_id` onto the work envelope so it round-trips through the
//!   runner and back.
//! * **Module discovery** — the libset is scanned once at startup (before the router thread
//!   exists — the scan is filesystem I/O) into `{name, version, base_uri}` module metadata,
//!   merged with live runners' advertisements into a catalog the router keeps in memory. The
//!   router answers `list_modules` queries and pushes the catalog to all frontends — but only
//!   when the set of available modules actually changes.
//!
//! Run:
//! * `cargo run`               — start the broker (control endpoint on `$JASP_ORCH_URL`).
//! * `cargo run -- --schema`   — print the JSON Schema generated from the Rust types, and exit.
//! * `cargo test`              — unit tests (inproc + tcp).
//!
//! Environment (§8):
//! * `JASP_ORCH_URL`             control endpoint; scheme selects transport (default `tcp://127.0.0.1:9555`).
//! * `JASP_ORCH_TEST_DATASET`    alpha dataset path injected into each work (default `test_data/debug.arrow`).
//! * `JASP_ORCH_DIR_ROOT`        orchestrator directory root (default `/tmp/jasp-orchestrator`).
//! * `JASP_ORCH_HANG_TIMEOUT_MS` busy hang timeout (default `30000`).
//! * `JASP_ORCH_ACTIVITY_MIN_MS` suggested `activity` rate-limit advertised to runners (default `1000`).
//! * `JASP_ORCH_LIBSET`          colon-separated libpaths; enables the runner provisioner (on-demand
//!   runner spawning). Unset → provisioner disabled (attach-only).
//! * `JASP_ORCH_RUNNER_SCRIPT`   runner entry script the provisioner spawns (default `refactor_design/runner_jaspbase.R`).
//! * `JASP_ORCH_RSCRIPT`         R interpreter the provisioner uses (default `Rscript`).
//! * `JASP_ORCH_SPAWN_TIMEOUT_MS` spawned-runner boot timeout (default `120000`).
//! * `JASP_ORCH_PARK_TIMEOUT_MS`  parked-work timeout (default `180000`).

mod messages;
mod provisioner;

use messages::{Capability, Envelope, Message, ModuleInfo, Status, WorkPayload};
use nng::options::{LocalAddr, Options, RecvBufferSize, SendBufferSize};
use nng::{Aio, AioResult, Listener, Pipe, PipeEvent, Protocol, Socket};
use provisioner::{ProvEvent, ProvReq, RunnerProvisioner};
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, AtomicUsize, Ordering},
    mpsc,
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

// ─── Framing (§18.1) ─────────────────────────────────────────────────────────

/// Frame raw JSON bytes into `[u32 BE length][json bytes]`.
fn frame_bytes(json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(json);
    out
}

/// Frame a typed [`Envelope`].
fn frame_envelope(env: &Envelope) -> Vec<u8> {
    frame_bytes(&serde_json::to_vec(env).expect("serialize envelope"))
}

/// Parse `[u32 BE length][json bytes][…]` into a typed [`Envelope`].
fn deframe(body: &[u8]) -> Option<Envelope> {
    if body.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + len {
        return None;
    }
    serde_json::from_slice(&body[4..4 + len]).ok()
}

/// Epoch milliseconds (saturates to 0 before the epoch).
fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ─── Configuration (§8) ──────────────────────────────────────────────────────

struct Config {
    control_url: String,
    dataset_path: String,
    orchestrator_dir_root: String,
    hang_timeout_ms: u64,
    activity_min_ms: u64,
    /// When set (JASP_ORCH_KEEP_WORKSPACES=1/true), skip workspace reclamation on work_close /
    /// frontend drop — for manual inspection/debugging. Off in production.
    keep_workspaces: bool,
    /// Runner provisioner config. `Some` enables on-demand runner spawning: a routing miss for an
    /// analysis module parks the work and asks the provisioner for a runner. `None` (the default,
    /// and what every existing test uses) keeps the original attach-only behaviour.
    provisioner: Option<ProvisionerConfig>,
}

/// Configuration for the runner provisioner (§9.3). Built from the environment only when
/// `JASP_ORCH_LIBSET` names at least one libpath; otherwise [`Config::provisioner`] is `None` and
/// the orchestrator behaves exactly as before (runners must self-attach).
#[derive(Clone)]
struct ProvisionerConfig {
    /// The libset: libpaths scanned for modules. Each libpath is a self-consistent merged library
    /// (jaspRunner + one or more modules + their co-resolved deps). The provisioner scans it at
    /// startup into its module→libpath / libpath→modules indexes.
    libset: Vec<PathBuf>,
    /// The runner entry script handed to the interpreter (default `refactor_design/runner_jaspbase.R`).
    runner_script: PathBuf,
    /// The R interpreter used to launch a runner (default `Rscript`).
    rscript_bin: String,
    /// How long to wait for a spawned runner to register before treating it as a boot failure.
    spawn_timeout_ms: u64,
    /// How long work may sit parked awaiting a runner before the router fails it (no-silent-loss
    /// backstop for a wedged provisioner).
    park_timeout_ms: u64,
}

impl ProvisionerConfig {
    /// Build the provisioner config from the environment, if provisioning is enabled.
    ///
    /// Enabled by `JASP_ORCH_LIBSET` (colon-separated libpaths). `JASP_ORCH_RUNNER_SCRIPT` selects
    /// the runner entry script (default `refactor_design/runner_jaspbase.R`), `JASP_ORCH_RSCRIPT`
    /// the interpreter (default `Rscript`), and `JASP_ORCH_SPAWN_TIMEOUT_MS` /
    /// `JASP_ORCH_PARK_TIMEOUT_MS` the boot and park windows. The libset is scanned by the
    /// provisioner at startup so a requested module maps to a libpath at provision time.
    fn from_env(parse: &impl Fn(&str, u64) -> u64) -> Option<Self> {
        let raw = std::env::var("JASP_ORCH_LIBSET").ok()?;
        let libset: Vec<PathBuf> = raw
            .split(':')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .collect();
        if libset.is_empty() {
            return None;
        }
        Some(ProvisionerConfig {
            libset,
            runner_script: std::env::var("JASP_ORCH_RUNNER_SCRIPT")
                .unwrap_or_else(|_| "refactor_design/runner_jaspbase.R".into())
                .into(),
            rscript_bin: std::env::var("JASP_ORCH_RSCRIPT").unwrap_or_else(|_| "Rscript".into()),
            spawn_timeout_ms: parse("JASP_ORCH_SPAWN_TIMEOUT_MS", 120_000),
            park_timeout_ms: parse("JASP_ORCH_PARK_TIMEOUT_MS", 180_000),
        })
    }
}

impl Config {
    fn from_env() -> Self {
        let parse = |key: &str, default: u64| -> u64 {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        Config {
            control_url: std::env::var("JASP_ORCH_URL")
                .unwrap_or_else(|_| "tcp://127.0.0.1:9555".into()),
            dataset_path: alpha_dataset_path(),
            orchestrator_dir_root: std::env::var("JASP_ORCH_DIR_ROOT")
                .unwrap_or_else(|_| "/tmp/jasp-orchestrator".into()),
            hang_timeout_ms: parse("JASP_ORCH_HANG_TIMEOUT_MS", 30_000),
            activity_min_ms: parse("JASP_ORCH_ACTIVITY_MIN_MS", 1_000),
            keep_workspaces: std::env::var("JASP_ORCH_KEEP_WORKSPACES")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false),
            provisioner: ProvisionerConfig::from_env(&parse),
        }
    }

    fn scheme(&self) -> &str {
        self.control_url.split("://").next().unwrap_or("tcp")
    }

    /// The host part of a `tcp://` control URL (used to construct tcp channel candidates).
    fn tcp_host(&self) -> &str {
        self.control_url
            .trim_start_matches("tcp://")
            .split(':')
            .next()
            .filter(|h| !h.is_empty())
            .unwrap_or("127.0.0.1")
    }

    /// Per-session workspace dir: `<orchestrator_dir_root>/<session_id>`. Reclaimed wholesale when
    /// the session closes (`drop_frontend`) and at startup GC.
    fn session_workspace(&self, session_id: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.orchestrator_dir_root).join(session_id)
    }

    /// Per-work workspace tree: `<orchestrator_dir_root>/<session_id>/<work_id>` — the parent of
    /// every revision's `results_<rev>` dir. Reclaimed wholesale (all revisions) when the frontend
    /// closes the whole work, on session-close, and at startup GC.
    fn work_workspace(&self, session_id: &str, work_id: &str) -> std::path::PathBuf {
        self.session_workspace(session_id).join(work_id)
    }

    /// Per-revision results dir: `<orchestrator_dir_root>/<session_id>/<work_id>/results_<rev>`.
    /// Each revision is self-contained (working state mid-run, published output after), so
    /// concurrent or out-of-order revisions never share a mutable workspace — reuse of prior
    /// computation is by *copying* the base revision's finished dir (copy-on-seed), never by
    /// sharing. Injected into the work as `output_dir`; reclaimed per-revision by `work_close(rev)`.
    fn revision_dir(&self, session_id: &str, work_id: &str, revision: u64) -> std::path::PathBuf {
        self.work_workspace(session_id, work_id)
            .join(format!("results_{revision}"))
    }
}

/// Resolve the alpha test dataset to an absolute path so the runner never depends on the
/// orchestrator's cwd. Overridable via `JASP_ORCH_TEST_DATASET`. Phase 3 replaces this entirely
/// with real per-dataset paths handed out by the data plane (§8).
fn alpha_dataset_path() -> String {
    let raw =
        std::env::var("JASP_ORCH_TEST_DATASET").unwrap_or_else(|_| "test_data/debug.arrow".into());
    let path = std::path::PathBuf::from(&raw);
    if path.is_absolute() {
        return raw;
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path).to_string_lossy().into_owned())
        .unwrap_or(raw)
}

/// Read back the address a listener actually bound, as a dialable URL — the single source of truth
/// for "what do peers dial?" For inproc/ipc this is the (unique) address we asked for; for tcp it is
/// the OS-assigned ephemeral `:0` port.
///
/// Corrects a one-byte quirk in nng 1.0.1 (probe-verified): its `From<nng_sockaddr>` converts the
/// address from network order but passes the port through raw, whereas NNG stores `sa_port` in
/// network order and `SocketAddrV4/V6::new` expect host order — so the readback port is endian-
/// flipped on little-endian hosts. `u16::from_be` is the portable network→host fix (a no-op on
/// big-endian, a swap on little-endian).
fn readback_url(listener: &Listener) -> Result<String, nng::Error> {
    Ok(match listener.get_opt::<LocalAddr>()? {
        nng::SocketAddr::Inet(v4) => format!("tcp://{}:{}", v4.ip(), u16::from_be(v4.port())),
        nng::SocketAddr::Inet6(v6) => format!("tcp://[{}]:{}", v6.ip(), u16::from_be(v6.port())),
        // inproc / ipc: the crate's Display already renders a dialable url.
        other => format!("{other}"),
    })
}

// ─── Runtimes (§4) ───────────────────────────────────────────────────────────

/// A registered runner and its dedicated data channel.
///
/// Hot state (`last_activity`, `outstanding`) lives in atomics so any thread (the router, the
/// hang scan) can read them cheaply. `channel` is the routing target (the PAIR socket the
/// orchestrator listened on and the runner dialed); the Aio recv loop reads it and outbound frames
/// are sent on it directly (probe-verified: a PAIR send buffers and returns immediately while the
/// peer drains).
struct RunnerRuntime {
    runner_id: String,
    channel: Arc<Socket>,
    capabilities: Vec<Capability>,
    priority: u32,
    seq: u64,
    /// true = orchestrator-spawned (killable on hang); false = self-attached. All runners are
    /// attached in this increment (the orchestrator does not yet spawn runners).
    #[allow(dead_code)]
    managed: bool,
    last_activity: AtomicU64,
    outstanding: AtomicUsize,
}

/// A connected frontend and its dedicated data channel. `session_id` is the orchestrator-assigned
/// scope (`s-N`) that namespaces this client's work/datasets.
struct FrontendRuntime {
    channel: Arc<Socket>,
    session_id: String,
    #[allow(dead_code)]
    client_id: Option<String>,
}

/// Orchestrator-side outbound channel buffer (messages, orch → peer). This is the cushion that
/// absorbs a slow or bursty peer — most importantly a frontend whose UI thread has momentarily
/// stalled — before `try_send` returns `TryAgain`. Deeper = more tolerance for transient peer
/// silence; the backpressure policy (runner → evict, frontend → drop) decides what a full buffer
/// *means*. Runners/clients set 64 on their side.
const ORCH_SEND_BUF: i32 = 256;
/// Orchestrator-side inbound channel buffer (messages, peer → orch). The recv `Aio` drains this
/// near-instantly (it only re-arms and enqueues to the router), so a shallow depth is ample.
const ORCH_RECV_BUF: i32 = 128;

impl RunnerRuntime {
    /// Non-blocking send on this channel (the socket carries `SENDBUF=ORCH_SEND_BUF`). Returns
    /// `Err(TryAgain)` when the peer's buffer is full (peer hung) or `Err(Closed)` when the peer is
    /// gone; callers evict the peer on `Err`. Never blocks the router thread.
    fn send(&self, frame: Vec<u8>) -> Result<(), nng::Error> {
        self.channel.try_send(frame.as_slice()).map_err(|(_m, e)| e)
    }
}

impl FrontendRuntime {
    /// Non-blocking send on this channel (see `RunnerRuntime::send`). Callers drop the frontend on
    /// `Err`.
    fn send(&self, frame: Vec<u8>) -> Result<(), nng::Error> {
        self.channel.try_send(frame.as_slice()).map_err(|(_m, e)| e)
    }
}

/// One in-flight work unit's routing record (§6). Keyed in [`Router::work`] by
/// `(session_id, work_id)`. `Clone` is cheap (two `Arc` bumps + a `u64`).
#[derive(Clone)]
struct WorkRoute {
    frontend: Arc<FrontendRuntime>,
    runner: Arc<RunnerRuntime>,
    revision: u64,
}

/// A freshly-allocated data channel: the shared PAIR socket (used by both the Aio recv loop and
/// direct sends) and the dialable URL handed to the peer.
type Channel = (Arc<Socket>, String);

// ─── Router mailbox (§5) ─────────────────────────────────────────────────────

/// A one-shot reply channel: send exactly one value, the requester reads it once.
type Reply<T> = mpsc::Sender<T>;

/// The provisioner handoff bundle: the router's request sender, the park timeout, and the
/// libset's discovery metadata (the catalog seed). Built by [`Broker::spawn_provisioner`] (or a
/// test stub) and consumed by [`Broker::start_inner`]. Events in the other direction
/// (provisioner → router) are NOT part of this bundle: the provisioner sends them itself,
/// directly onto the router's mailbox, via the `on_event` closure injected at spawn time.
type ProvisionerHandles = (mpsc::Sender<ProvReq>, u64, Vec<ModuleInfo>);

/// Everything the single router thread can be asked to do. Producers are the NNG `Aio` callbacks
/// (control handshake, per-peer data channels), the `pipe_notify` disconnect callbacks, the
/// hang-detector thread, and (in tests) state queries. The router is the sole consumer, so all of
/// these are serialized into one total order — there is no concurrent mutation of broker state.
enum RouterMsg {
    /// Handshake forwarded from the control REP callback; the router allocates + arms a channel,
    /// registers the peer, and replies with the `welcome`/`register_ack` envelope.
    Handshake {
        env: Envelope,
        reply: Reply<Envelope>,
    },
    /// A deframed message arrived on a runner's data channel.
    RunnerData {
        runner: Arc<RunnerRuntime>,
        env: Envelope,
    },
    /// A deframed message arrived on a frontend's data channel.
    FrontendData {
        frontend: Arc<FrontendRuntime>,
        env: Envelope,
    },
    /// A runner's channel pipe was removed (disconnect) → evict.
    EvictRunner(String),
    /// A frontend's channel pipe was removed (disconnect) → drop.
    DropFrontend(String),
    /// The provisioner could not provide a module (not in the libset, spawn crashed, or never
    /// registered in time) — fail the work parked for it back to the frontend (no-silent-loss).
    ProvisionFailed { module: String, reason: String },
    /// Periodic hang scan (from the detector thread).
    Tick,
    /// Hand a strong `Aio` ref to the router so a recv loop stays alive (the trampoline holds only
    /// a weak ref; the router's map is the canonical keep-alive).
    KeepAlive { key: String, aio: Aio },
    /// Test-only: snapshot current runner ids.
    #[allow(dead_code)]
    QueryRunners(Reply<Vec<String>>),
    /// Test-only: snapshot currently-hung runner ids.
    #[allow(dead_code)]
    QueryHung(Reply<Vec<String>>),
    /// Test-only: has any runner ever registered?
    #[allow(dead_code)]
    QueryEverRegistered(Reply<bool>),
}

// ─── Janitor (off-router filesystem reclamation) ─────────────────────────────

/// Mailbox for the janitor: paths to delete recursively.
type JanitorTx = mpsc::Sender<std::path::PathBuf>;

/// Spawn the single janitor thread and return its mailbox. Recursive directory deletion is
/// blocking filesystem I/O of unbounded duration, so it must never run on the router thread (the
/// "router never blocks" invariant). The router only ever enqueues a path (non-blocking); the
/// janitor does the deletion sequentially. Deletion is best-effort — `NotFound` is treated as
/// success (already gone), so cleanup is idempotent and a failed or repeated delete is harmless;
/// startup GC is the backstop for anything leaked.
fn start_janitor() -> JanitorTx {
    let (tx, rx) = mpsc::channel::<std::path::PathBuf>();
    std::thread::Builder::new()
        .name("orch-janitor".into())
        .spawn(move || {
            while let Ok(path) = rx.recv() {
                match std::fs::remove_dir_all(&path) {
                    Ok(()) => println!("[orch] reclaimed {}", path.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => eprintln!("[orch] cleanup failed for {}: {e}", path.display()),
                }
            }
        })
        .expect("spawn janitor thread");
    tx
}

// ─── Router (single-threaded owner of all broker state, §4/§5) ───────────────

/// Per-process nonce source: concurrently-running brokers (e.g. parallel tests) each get a
/// distinct value so their channel addresses never collide. inproc names are process-global and a
/// pid-scoped ipc path is shared by every broker in the process, so the per-broker id alone is not
/// enough to disambiguate.
static BROKER_NONCE: AtomicU64 = AtomicU64::new(0);

/// A work unit parked while awaiting a runner that does not yet exist (§9.3). Dispatched by
/// `try_dispatch_parked` when a matching runner registers, or failed on park-timeout /
/// `ProvisionFailed`. Keyed in [`Router::parked`] by `(session_id, work_id)` — which also dedups a
/// frontend that retries an already-parked work.
struct ParkedWork {
    module: String,
    frontend: Arc<FrontendRuntime>,
    env: Envelope,
    work: messages::Work,
    parked_ms: u64,
}

/// The single-threaded state machine. Owns the registries, correlation table, and channel keep-
/// alives as plain `HashMap`s — safe because only the router thread ever touches them. Created once
/// by [`Broker::start`], moved into the router thread, and driven by [`Router::run`].
struct Router {
    runners: HashMap<String, Arc<RunnerRuntime>>,
    frontends: HashMap<String, Arc<FrontendRuntime>>,
    work: HashMap<(String, String), WorkRoute>,
    /// Work awaiting a runner that does not exist yet, keyed `(session_id, work_id)`. Only used when
    /// a provisioner is configured; empty otherwise.
    parked: HashMap<(String, String), ParkedWork>,
    /// Strong `Aio` refs keep the per-channel recv loops alive. Keyed by runner_id / session_id
    /// (and "control" for the REP socket).
    aios: HashMap<String, Aio>,
    counter: u64,
    /// Set on the first registration; read by `route_work` to decide whether unservable work is an
    /// error (a runner existed) or a silent drop (no runner yet). Router-thread-only, so a plain
    /// `bool`.
    ever_registered: bool,
    config: Config,
    nonce: u64,
    /// Clone handed to each newly-armed channel callback so it can feed messages back to the loop.
    /// Also keeps the loop's `Receiver` from ever disconnecting while the router runs.
    tx: mpsc::Sender<RouterMsg>,
    /// Mailbox for the janitor thread. The router enqueues workspace paths here (non-blocking) for
    /// off-thread deletion; it never touches the filesystem itself.
    janitor: JanitorTx,
    /// Mailbox for the provisioner thread (§9.3). `None` disables on-demand spawning (the original
    /// attach-only behaviour). The router sends `Provision` on a miss and `RunnerUp`/`RunnerGone` on
    /// lifecycle events; the provisioner replies via `RouterMsg::ProvisionFailed`.
    provisioner: Option<mpsc::Sender<ProvReq>>,
    /// How long work may sit parked before being failed (0 when no provisioner).
    park_timeout_ms: u64,
    /// Modules discovered by the startup libset scan (empty without a provisioner). The stable
    /// half of the catalog — filesystem I/O happened once, before this thread started; the router
    /// only ever reads this vector, never re-scans.
    libset_modules: Vec<ModuleInfo>,
    /// The current module catalog (libset modules ∪ live runners' advertisements), in canonical
    /// sorted order. Answered verbatim on `list_modules`; pushed to all frontends only when a
    /// recompute actually changes it.
    catalog: Vec<ModuleInfo>,
}

impl Router {
    /// The event loop: block on the mailbox, dispatch one message at a time, forever. All broker
    /// policy runs here, sequentially, on one thread.
    fn run(mut self, rx: mpsc::Receiver<RouterMsg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                RouterMsg::Handshake { env, reply } => {
                    let ack = match &env.body {
                        Message::Register(reg) => self.handle_register(&env, reg),
                        Message::Hello(h) => self.handle_hello(&env, h),
                        other => {
                            println!("[orch] control ignoring {other:?}");
                            orch_err(
                                Some(&env.id),
                                "unexpected_on_control",
                                "control endpoint carries only the handshake",
                            )
                        }
                    };
                    let _ = reply.send(ack);
                }
                RouterMsg::RunnerData { runner, env } => self.on_runner_message(&runner, env),
                RouterMsg::FrontendData { frontend, env } => {
                    self.on_frontend_message(&frontend, env)
                }
                RouterMsg::EvictRunner(id) => self.evict_runner(&id),
                RouterMsg::DropFrontend(id) => self.drop_frontend(&id),
                RouterMsg::ProvisionFailed { module, reason } => {
                    self.fail_parked_module(&module, &reason)
                }
                RouterMsg::Tick => {
                    self.scan_hung();
                    self.scan_parked();
                }
                RouterMsg::KeepAlive { key, aio } => {
                    self.aios.insert(key, aio);
                }
                RouterMsg::QueryRunners(reply) => {
                    let ids = self.runners.keys().cloned().collect();
                    let _ = reply.send(ids);
                }
                RouterMsg::QueryHung(reply) => {
                    let _ = reply.send(self.hung_runners());
                }
                RouterMsg::QueryEverRegistered(reply) => {
                    let _ = reply.send(self.ever_registered);
                }
            }
        }
    }

    /// Mint a unique id (`r-N` / `s-N`), returning `(id, seq)`.
    fn next_id(&mut self, prefix: &str) -> (String, u64) {
        self.counter += 1;
        (format!("{}-{}", prefix, self.counter), self.counter)
    }

    // ── Channel allocation (§2.2, scheme-aware) ──────────────────────────────

    /// Allocate a dedicated PAIR data channel and return its `(socket, dialable_url)`.
    ///
    /// Uniform across transports: bind a candidate address with `Listener::new`, then use the
    /// address the OS *actually* bound as the dialable URL. For inproc/ipc the candidate is a unique
    /// name/path we construct (the per-broker `nonce` keeps it collision-free across brokers in one
    /// process, since inproc names are process-global) and the dialable URL is the candidate itself.
    /// For tcp the candidate is `host:0`, so the OS assigns a guaranteed-free ephemeral port which we
    /// read back via `readback_url`. No port ranges, no retry loops, and no transport-specific code
    /// beyond the candidate-address syntax (inherent to each transport's addressing).
    fn allocate_channel(&self, id: &str) -> Result<Channel, nng::Error> {
        let sock = Socket::new(Protocol::Pair1)?;
        let url = match self.config.scheme() {
            "inproc" => {
                let url = format!("inproc://jasp-ch-{}-{id}", self.nonce);
                let listener = Listener::new(&sock, &url)?;
                let _ = listener; // socket keeps the listener alive; handle is Copy
                url
            }
            "ipc" => {
                let url = format!(
                    "ipc:///tmp/jasp-ch-{}-{id}-{}.sock",
                    self.nonce,
                    std::process::id()
                );
                let listener = Listener::new(&sock, &url)?;
                let _ = listener;
                url
            }
            // tcp: delegate the port to the OS (`:0`); the readback tells us which port we got.
            _ => {
                let listener =
                    Listener::new(&sock, &format!("tcp://{}:0", self.config.tcp_host()))?;
                readback_url(&listener)?
            }
        };
        // Share the socket between the Aio recv loop and direct sends.
        // Orchestrator-side buffers: the outbound SENDBUF is the backpressure cushion for a slow
        // peer (256 — tolerates a stalled frontend / bursty results); the inbound RECVBUF stays
        // shallow (128) because the recv Aio drains it immediately. When SENDBUF fills, `try_send`
        // returns `TryAgain` and the per-peer policy applies (runner → evict, frontend → drop).
        sock.set_opt::<SendBufferSize>(ORCH_SEND_BUF)?;
        sock.set_opt::<RecvBufferSize>(ORCH_RECV_BUF)?;
        let channel = Arc::new(sock);
        Ok((channel, url))
    }

    // ── Handshake (control endpoint) ─────────────────────────────────────────

    fn handle_register(&mut self, env: &Envelope, reg: &messages::Register) -> Envelope {
        if env.v != 1 {
            return Self::register_ack(false, None, None, "unsupported_version", &env.id, None);
        }
        let (runner_id, seq) = self.next_id("r");
        let (channel, channel_url) = match self.allocate_channel(&runner_id) {
            Ok(x) => x,
            Err(e) => {
                return Self::register_ack(
                    false,
                    None,
                    None,
                    &format!("channel allocation failed: {e}"),
                    &env.id,
                    None,
                );
            }
        };
        let runner = Arc::new(RunnerRuntime {
            runner_id: runner_id.clone(),
            channel,
            capabilities: reg.capabilities.clone(),
            priority: reg.priority,
            seq,
            managed: false,
            last_activity: AtomicU64::new(now_ms()),
            outstanding: AtomicUsize::new(0),
        });
        if let Err(e) = self.arm_runner_channel(Arc::clone(&runner)) {
            return Self::register_ack(
                false,
                None,
                None,
                &format!("channel arm failed: {e}"),
                &env.id,
                None,
            );
        }
        self.runners.insert(runner_id.clone(), Arc::clone(&runner));
        self.ever_registered = true;
        println!(
            "[orch] runner registered {runner_id} (hint {:?}) channel={channel_url} caps={:?}",
            reg.runner_id, reg.capabilities
        );
        // Reconcile the provisioner (its spawn succeeded) and drain any work that was parked
        // waiting for a module this runner advertises (§9.3).
        let modules: Vec<String> = runner
            .capabilities
            .iter()
            .filter_map(|c| match c {
                Capability::Analysis { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        if let Some(p) = &self.provisioner
            && !modules.is_empty()
        {
            let _ = p.send(ProvReq::RunnerUp {
                modules: modules.clone(),
            });
        }
        self.try_dispatch_parked(&modules);
        // Merge this runner's advertisements into the discovery catalog; push to frontends only
        // if the set of available modules actually changed. Pure in-memory work — the router
        // never touches the filesystem here.
        self.recompute_catalog();
        Self::register_ack(
            true,
            Some(runner_id),
            Some(channel_url),
            "",
            &env.id,
            Some(self.config.activity_min_ms),
        )
    }

    fn handle_hello(&mut self, env: &Envelope, hello: &messages::Hello) -> Envelope {
        if env.v != 1 {
            return Self::welcome(false, None, None, Some("unsupported_version"), &env.id);
        }
        let (session_id, _seq) = self.next_id("s");
        let (channel, channel_url) = match self.allocate_channel(&session_id) {
            Ok(x) => x,
            Err(e) => {
                return Self::welcome(
                    false,
                    None,
                    None,
                    Some(&format!("channel allocation failed: {e}")),
                    &env.id,
                );
            }
        };
        let frontend = Arc::new(FrontendRuntime {
            channel,
            session_id: session_id.clone(),
            client_id: hello.client_id.clone(),
        });
        if let Err(e) = self.arm_frontend_channel(Arc::clone(&frontend)) {
            return Self::welcome(
                false,
                None,
                None,
                Some(&format!("channel arm failed: {e}")),
                &env.id,
            );
        }
        self.frontends
            .insert(session_id.clone(), Arc::clone(&frontend));
        // A connected frontend always holds the current catalog: send it on the fresh channel
        // now — the frontend has not dialed yet, so the frame sits in the listening PAIR's
        // buffer (preconnect buffering, the same mechanism as dispatch-on-registration) and is
        // delivered as the FIRST frame on the channel. Reconnects re-run this handler, so a
        // re-handshaking frontend re-receives the current catalog with no query of its own.
        let catalog_env = Self::modules_envelope(&self.catalog, None, Some(session_id.clone()));
        if let Err(e) = frontend.send(frame_envelope(&catalog_env)) {
            eprintln!("[orch] initial catalog to frontend {session_id} failed: {e}");
        }
        println!(
            "[orch] frontend registered {session_id} (client_id {:?}) channel={channel_url} catalog={} module(s)",
            hello.client_id,
            self.catalog.len()
        );
        Self::welcome(true, Some(session_id), Some(channel_url), None, &env.id)
    }

    fn register_ack(
        ok: bool,
        runner_id: Option<String>,
        channel_url: Option<String>,
        reason: &str,
        reply_to: &str,
        activity_min_interval_ms: Option<u64>,
    ) -> Envelope {
        Envelope {
            v: 1,
            id: format!(
                "orch-ack-{}",
                runner_id.clone().unwrap_or_else(|| "err".into())
            ),
            reply_to: Some(reply_to.to_string()),
            session_id: None,
            ts: None,
            body: Message::RegisterAck(messages::RegisterAck {
                ok,
                runner_id,
                channel_url,
                activity_min_interval_ms,
                reason: if ok || reason.is_empty() {
                    None
                } else {
                    Some(reason.to_string())
                },
            }),
        }
    }

    fn welcome(
        ok: bool,
        session_id: Option<String>,
        channel_url: Option<String>,
        error: Option<&str>,
        reply_to: &str,
    ) -> Envelope {
        let id = format!(
            "orch-welcome-{}",
            session_id.clone().unwrap_or_else(|| "err".into())
        );
        Envelope {
            v: 1,
            id,
            reply_to: Some(reply_to.to_string()),
            // The assigned session lives on the envelope (the tenancy scope every message carries);
            // `Welcome` has no separate `session_id` field, which would collide on the flattened wire.
            session_id,
            ts: None,
            body: Message::Welcome(messages::Welcome {
                ok,
                channel_url,
                error: error.map(|s| s.to_string()),
            }),
        }
    }

    // ── Channel Aio loops (§5) ───────────────────────────────────────────────
    //
    // Callbacks run on NNG's pool threads and do NO routing. They re-arm the recv immediately (so
    // the socket is always receptive and a peer's blocking send can always land in RECVBUF), then
    // forward the message to the router over the mailbox. Disconnects surface via `pipe_notify`'s
    // `RemovePost` (a *listening* PAIR's recv does NOT return `Closed` when the peer leaves).

    fn arm_runner_channel(&mut self, runner: Arc<RunnerRuntime>) -> Result<(), nng::Error> {
        let tx_pn = self.tx.clone();
        let rid = runner.runner_id.clone();
        runner
            .channel
            .pipe_notify(move |_pipe: Pipe, event: PipeEvent| {
                if matches!(event, PipeEvent::RemovePost) {
                    let _ = tx_pn.send(RouterMsg::EvictRunner(rid.clone()));
                }
            })?;
        let tx = self.tx.clone();
        let rt = Arc::clone(&runner);
        let aio = Aio::new(move |aio: Aio, res: AioResult| match res {
            AioResult::Recv(Ok(msg)) => {
                rt.last_activity.store(now_ms(), Ordering::Relaxed);
                // Re-arm BEFORE forwarding so the recv is always armed.
                if let Err(e) = rt.channel.recv_async(&aio) {
                    eprintln!("[orch] runner {} re-arm failed: {e}", rt.runner_id);
                }
                if let Some(env) = deframe(&msg[..]) {
                    let _ = tx.send(RouterMsg::RunnerData {
                        runner: Arc::clone(&rt),
                        env,
                    });
                }
            }
            AioResult::Recv(Err(e)) => {
                println!("[orch] runner {} channel closed: {e}", rt.runner_id);
                let _ = tx.send(RouterMsg::EvictRunner(rt.runner_id.clone()));
            }
            _ => {}
        })?;
        runner.channel.recv_async(&aio)?;
        self.aios.insert(runner.runner_id.clone(), aio);
        Ok(())
    }

    fn arm_frontend_channel(&mut self, frontend: Arc<FrontendRuntime>) -> Result<(), nng::Error> {
        let tx_pn = self.tx.clone();
        let sid = frontend.session_id.clone();
        frontend
            .channel
            .pipe_notify(move |_pipe: Pipe, event: PipeEvent| {
                if matches!(event, PipeEvent::RemovePost) {
                    let _ = tx_pn.send(RouterMsg::DropFrontend(sid.clone()));
                }
            })?;
        let tx = self.tx.clone();
        let fe = Arc::clone(&frontend);
        let aio = Aio::new(move |aio: Aio, res: AioResult| match res {
            AioResult::Recv(Ok(msg)) => {
                // Re-arm BEFORE forwarding so the recv is always armed.
                if let Err(e) = fe.channel.recv_async(&aio) {
                    eprintln!("[orch] frontend {} re-arm failed: {e}", fe.session_id);
                }
                if let Some(env) = deframe(&msg[..]) {
                    let _ = tx.send(RouterMsg::FrontendData {
                        frontend: Arc::clone(&fe),
                        env,
                    });
                }
            }
            AioResult::Recv(Err(e)) => {
                println!("[orch] frontend {} channel closed: {e}", fe.session_id);
                let _ = tx.send(RouterMsg::DropFrontend(fe.session_id.clone()));
            }
            _ => {}
        })?;
        frontend.channel.recv_async(&aio)?;
        self.aios.insert(frontend.session_id.clone(), aio);
        Ok(())
    }

    // ── Message handling & routing (§6) ──────────────────────────────────────

    fn on_runner_message(&mut self, runner: &RunnerRuntime, env: Envelope) {
        match &env.body {
            Message::Result(r) => {
                let terminal = matches!(
                    r.status,
                    Status::Complete | Status::FatalError | Status::ValidationError
                );
                if terminal {
                    let _ = runner.outstanding.fetch_update(
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                        |v| v.checked_sub(1),
                    );
                }
                self.route_result(env, terminal);
            }
            Message::Activity(_) => { /* last_activity already bumped on recv */ }
            other => println!("[orch] runner {} -> {other:?}", runner.runner_id),
        }
    }

    fn on_frontend_message(&mut self, fe: &Arc<FrontendRuntime>, env: Envelope) {
        // Decide the action with data cloned out of `env` first, so the match that moves `env`
        // (into `route_work`) does not also borrow `env.body`.
        enum Action {
            Work(messages::Work),
            Abort(String),
            WorkClose(String, Option<u64>),
            ListModules,
            Ping,
            Other,
        }
        let action = match &env.body {
            Message::Work(w) => Action::Work(w.clone()),
            Message::Abort(a) => Action::Abort(a.work_id.clone()),
            Message::WorkClose(wc) => Action::WorkClose(wc.work_id.clone(), wc.revision),
            Message::ListModules => Action::ListModules,
            Message::Ping => Action::Ping,
            _ => Action::Other,
        };
        match action {
            Action::Work(w) => self.route_work(Arc::clone(fe), env, &w),
            Action::Abort(work_id) => self.route_abort(fe, &work_id),
            Action::WorkClose(work_id, revision) => self.close_work(fe, work_id, revision),
            Action::ListModules => {
                // Answer with the current catalog (no recompute — it is maintained on every
                // register/evict, so the stored catalog is authoritative).
                let reply = Self::modules_envelope(
                    &self.catalog,
                    Some(env.id.clone()),
                    Some(fe.session_id.clone()),
                );
                if let Err(e) = fe.send(frame_envelope(&reply)) {
                    eprintln!(
                        "[orch] modules reply to frontend {} failed: {e}",
                        fe.session_id
                    );
                }
            }
            Action::Ping => {
                let pong = Envelope {
                    v: 1,
                    id: format!("orch-pong-{}", env.id),
                    reply_to: Some(env.id.clone()),
                    session_id: Some(fe.session_id.clone()),
                    ts: None,
                    body: Message::Pong,
                };
                if let Err(e) = fe.send(frame_envelope(&pong)) {
                    eprintln!("[orch] pong to frontend {} failed: {e}", fe.session_id);
                }
            }
            Action::Other => println!("[orch] frontend {} -> {:?}", fe.session_id, env.body),
        }
    }

    /// Work (frontend → runner) — §6.1.
    fn route_work(&mut self, fe: Arc<FrontendRuntime>, env: Envelope, w: &messages::Work) {
        // 1. Select a runner (returns an owned Arc; no borrow of `self` escapes this line).
        match select_runner(&self.runners, &w.payload) {
            Some(runner) => self.dispatch_work(runner, fe, env, w),
            None => self.miss_work(fe, env, w),
        }
    }

    /// Dispatch a work unit to a selected runner: stamp the session, inject the runner-facing
    /// fields (§19.3), record the route, and forward. Steps 2–4 of the routing path, shared by the
    /// fresh-route case and the parked-work drain (`try_dispatch_parked`).
    fn dispatch_work(
        &mut self,
        runner: Arc<RunnerRuntime>,
        fe: Arc<FrontendRuntime>,
        mut env: Envelope,
        w: &messages::Work,
    ) {
        // 2. Stamp session_id so (session_id, work_id) round-trips through the runner.
        env.session_id = Some(fe.session_id.clone());

        // 3. Inject runner-facing fields (§19.3): absolute dataset path + this revision's
        // self-contained workspace. Per-revision isolation: `output_dir` is `results_<revision>`,
        // so concurrent/out-of-order revisions never share a mutable workspace. The orchestrator
        // owns workspace *lifecycle* (naming + reclamation via the janitor) but never touches the
        // filesystem on the router thread — the runner creates `output_dir` and seeds it.
        let output_dir = self
            .config
            .revision_dir(&fe.session_id, &w.work_id, w.revision);
        let mut value = serde_json::to_value(&env).expect("work envelope to value");
        value["dataset_paths"] = json!({ "ds-001": self.config.dataset_path });
        value["output_dir"] = json!(output_dir.to_string_lossy());
        // Base revision to seed incremental recompute from (frontend-declared): inject the concrete
        // path so the runner can copy it into its own dir; absent → full recompute (no seed).
        if let Some(base) = w.base_revision {
            let base_dir = self.config.revision_dir(&fe.session_id, &w.work_id, base);
            value["base_results_dir"] = json!(base_dir.to_string_lossy());
        }
        let frame = frame_bytes(&serde_json::to_vec(&value).expect("re-serialize work"));

        // 4. Record the route, bump outstanding, forward.
        let key = (fe.session_id.clone(), w.work_id.clone());
        println!(
            "[orch] fe->rn work work_id={} revision={} session={} runner={}",
            w.work_id, w.revision, fe.session_id, runner.runner_id
        );
        self.work.insert(
            key,
            WorkRoute {
                frontend: fe,
                runner: Arc::clone(&runner),
                revision: w.revision,
            },
        );
        runner.outstanding.fetch_add(1, Ordering::Relaxed);
        if let Err(e) = runner.send(frame) {
            // Backpressure (buffer full) or a closed peer: the runner is hung/gone → evict it.
            // Eviction fails this just-recorded work back to the frontend (no silent loss).
            eprintln!(
                "[orch] work send to runner {} failed ({e}); evicting",
                runner.runner_id
            );
            self.evict_runner(&runner.runner_id);
        }
    }

    /// No live runner can serve this work. With a provisioner configured, **park** an analysis work
    /// unit and request a runner (it is dispatched on registration via `try_dispatch_parked`); the
    /// frontend is sent a `running` marker so it knows the work is pending, not lost. Without a
    /// provisioner (or for non-analysis work), keep the original no-silent-loss behaviour: a visible
    /// error once a runner has existed, else a silent drop.
    fn miss_work(&mut self, fe: Arc<FrontendRuntime>, env: Envelope, w: &messages::Work) {
        let prov = self.provisioner.clone();
        if let (Some(prov), Some((module, version))) = (prov, analysis_module(&w.payload)) {
            self.park_work(prov, fe, env, w, module.to_string(), version.to_string());
            return;
        }
        // No provisioner (or not an analysis): original no-silent-loss behaviour.
        if self.ever_registered {
            let detail = match &w.payload {
                WorkPayload::Analysis(a) => {
                    format!(
                        "No runner is available to run analyses from module '{}'.",
                        a.module
                    )
                }
                WorkPayload::Rcode(_) => "No runner is available to run R code.".to_string(),
            };
            eprintln!(
                "[orch] no live runner for work_id={} — returning error",
                w.work_id
            );
            let err = no_runner_result(&w.work_id, w.revision, &fe.session_id, &detail);
            // Frontend send is best-effort: a backpressured frontend is tolerated (its results
            // are dropped); a truly dead frontend is reaped by its channel's pipe_notify.
            if let Err(e) = fe.send(err) {
                eprintln!(
                    "[orch] no-runner error to frontend {} failed: {e}",
                    fe.session_id
                );
            }
        } else {
            println!(
                "[orch] work {} dropped (no runner ever registered)",
                w.work_id
            );
        }
    }

    /// Park a work unit awaiting a runner, tell the frontend it is pending, and ask the provisioner
    /// for a runner. Keyed by `(session_id, work_id)`:
    /// - new key → park + request a runner;
    /// - parked, higher revision → **supersede** the parked envelope (the frontend is the revision
    ///   authority, §23: newer wins); the in-flight provision request covers the new revision;
    /// - parked, same/lower revision → a retry — re-ack with the running marker, keep what's parked.
    ///
    /// Without the supersede, a resubmit while provisioning (user editing options fast) would leave
    /// the stale revision parked; the runner would run it and the client would drop its result as
    /// stale (§23) — leaving the analysis spinning forever.
    fn park_work(
        &mut self,
        prov: mpsc::Sender<ProvReq>,
        fe: Arc<FrontendRuntime>,
        env: Envelope,
        w: &messages::Work,
        module: String,
        version: String,
    ) {
        let key = (fe.session_id.clone(), w.work_id.clone());
        // Running marker: the work is accepted and pending a runner, not lost (§25.5).
        if let Err(e) = fe.send(running_result(&w.work_id, w.revision, &fe.session_id)) {
            eprintln!(
                "[orch] running-marker to frontend {} failed: {e}",
                fe.session_id
            );
        }
        match self.parked.entry(key) {
            std::collections::hash_map::Entry::Occupied(mut slot) => {
                if w.revision > slot.get().work.revision {
                    println!(
                        "[orch] parked work_id={} superseded: revision {} -> {}",
                        w.work_id,
                        slot.get().work.revision,
                        w.revision
                    );
                    let pw = slot.get_mut();
                    pw.env = env;
                    pw.work = w.clone();
                    // parked_ms is kept: the provision request started at the original park time.
                }
                // else: stale/equal retry — already re-acked with the running marker above.
            }
            std::collections::hash_map::Entry::Vacant(slot) => {
                println!(
                    "[orch] parking work_id={} revision={} (module {}) awaiting a runner",
                    w.work_id, w.revision, module
                );
                slot.insert(ParkedWork {
                    module: module.clone(),
                    frontend: fe,
                    env,
                    work: w.clone(),
                    parked_ms: now_ms(),
                });
                let _ = prov.send(ProvReq::Provision { module, version });
            }
        }
    }

    /// Drain parked work that a newly-registered runner can serve (called from `handle_register`).
    fn try_dispatch_parked(&mut self, modules: &[String]) {
        if self.parked.is_empty() || modules.is_empty() {
            return;
        }
        let ready: Vec<(String, String)> = self
            .parked
            .iter()
            .filter(|(_, pw)| modules.contains(&pw.module))
            .map(|(k, _)| k.clone())
            .collect();
        for key in ready {
            // Take the parked work out (owned) before borrowing `self` mutably for dispatch.
            let Some(pw) = self.parked.remove(&key) else {
                continue;
            };
            let Some(runner) = select_runner(&self.runners, &pw.work.payload) else {
                // No eligible runner after all — put it back and keep waiting.
                self.parked.insert(key, pw);
                continue;
            };
            println!(
                "[orch] dispatching parked work_id={} (module {}) to runner {}",
                pw.work.work_id, pw.module, runner.runner_id
            );
            self.dispatch_work(runner, pw.frontend, pw.env, &pw.work);
        }
    }

    /// Fail every parked work for a module the provisioner could not provide (no-silent-loss).
    fn fail_parked_module(&mut self, module: &str, reason: &str) {
        let doomed: Vec<(String, String)> = self
            .parked
            .iter()
            .filter(|(_, pw)| pw.module == module)
            .map(|(k, _)| k.clone())
            .collect();
        for key in doomed {
            if let Some(pw) = self.parked.remove(&key) {
                eprintln!(
                    "[orch] cannot provision module '{module}' for work_id={}: {reason}",
                    pw.work.work_id
                );
                let err = no_runner_result(
                    &pw.work.work_id,
                    pw.work.revision,
                    &key.0,
                    &format!("Could not start a runner for module '{module}': {reason}"),
                );
                let _ = pw.frontend.send(err);
            }
        }
    }

    /// Park-timeout backstop (the `Tick` handler's parked half): fail work parked longer than the
    /// configured window, so a wedged provisioner never strands work silently.
    fn scan_parked(&mut self) {
        if self.park_timeout_ms == 0 || self.parked.is_empty() {
            return;
        }
        let now = now_ms();
        let expired: Vec<(String, String)> = self
            .parked
            .iter()
            .filter(|(_, pw)| now.saturating_sub(pw.parked_ms) > self.park_timeout_ms)
            .map(|(k, _)| k.clone())
            .collect();
        for key in expired {
            if let Some(pw) = self.parked.remove(&key) {
                eprintln!(
                    "[orch] parked work_id={} (module {}) timed out after {}ms",
                    pw.work.work_id, pw.module, self.park_timeout_ms
                );
                let err = no_runner_result(
                    &pw.work.work_id,
                    pw.work.revision,
                    &key.0,
                    &format!("Timed out waiting for a runner for module '{}'.", pw.module),
                );
                let _ = pw.frontend.send(err);
            }
        }
    }

    /// Result (runner → frontend) — §6.2.
    fn route_result(&mut self, env: Envelope, terminal: bool) {
        let (work_id, revision) = match &env.body {
            Message::Result(r) => (r.work_id.clone(), r.revision),
            _ => return,
        };
        let session_id = env.session_id.clone().unwrap_or_default();
        let key = (session_id.clone(), work_id.clone());

        let Some(route) = self.work.get(&key).cloned() else {
            println!("[orch] result for unknown work ({session_id},{work_id}) — dropping");
            return;
        };
        // Stale guard (§23).
        if revision < route.revision {
            println!(
                "[orch] stale result ({session_id},{work_id}) rev {revision} < {} — dropping",
                route.revision
            );
            return;
        }
        // Frontend send is best-effort (a user session is tolerated, not evicted, on a transient
        // full buffer). The route is still removed on terminal so the table doesn't leak.
        if let Err(e) = route.frontend.send(frame_envelope(&env)) {
            eprintln!(
                "[orch] result to frontend {} failed ({e}); dropping result",
                route.frontend.session_id
            );
        }
        if terminal {
            self.work.remove(&key);
        }
    }

    /// Work discard (frontend → orchestrator). The frontend is the authority on "this `work_id` is
    /// consumed forever" (its analysis was closed). Revision-granular: with `Some(rev)` only that
    /// revision's `results_<rev>` is reclaimed (and it is aborted only if it is the in-flight one);
    /// with `None` the whole work tree (every revision) is reclaimed and the in-flight work
    /// aborted. Idempotent — closing an unknown or already-closed work/revision is a no-op.
    /// Deletion is offloaded to the janitor so the router never blocks on filesystem I/O.
    fn close_work(&mut self, fe: &FrontendRuntime, work_id: String, revision: Option<u64>) {
        let key = (fe.session_id.clone(), work_id.clone());
        match revision {
            // Close one revision: prune results_<rev>; abort only if it is the in-flight revision
            // (the route tracks the latest revision — a different one may be running).
            Some(rev) => {
                let inflight = self
                    .work
                    .get(&key)
                    .filter(|route| route.revision == rev)
                    .cloned();
                if let Some(route) = inflight {
                    self.work.remove(&key);
                    self.abort_and_release(&route, &fe.session_id, &work_id);
                }
                let dir = self.config.revision_dir(&fe.session_id, &work_id, rev);
                if !self.config.keep_workspaces {
                    let _ = self.janitor.send(dir);
                }
                println!(
                    "[orch] work {work_id} rev {rev} (session {}) closed; results_{rev} reclaimed",
                    fe.session_id
                );
            }
            // Close the whole work: abort the in-flight work + prune the entire work tree.
            None => {
                if let Some(route) = self.work.remove(&key) {
                    self.abort_and_release(&route, &fe.session_id, &work_id);
                }
                let dir = self.config.work_workspace(&fe.session_id, &work_id);
                if !self.config.keep_workspaces {
                    let _ = self.janitor.send(dir);
                }
                println!(
                    "[orch] work {work_id} (session {}) closed; workspace reclaimed",
                    fe.session_id
                );
            }
        }
    }

    /// Best-effort: tell a runner to stop an in-flight work and release its outstanding slot (used
    /// by `close_work`). Does not evict the runner — a failed abort means it is already gone or
    /// backpressured, which its own pipe_notify/eviction handles.
    fn abort_and_release(&self, route: &WorkRoute, session_id: &str, work_id: &str) {
        let _ = route
            .runner
            .outstanding
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |v| v.checked_sub(1));
        let abort = Envelope {
            v: 1,
            id: format!("orch-abort-{work_id}"),
            reply_to: None,
            session_id: Some(session_id.to_string()),
            ts: None,
            body: Message::Abort(messages::Abort {
                work_id: work_id.to_string(),
            }),
        };
        if let Err(e) = route.runner.send(frame_envelope(&abort)) {
            eprintln!(
                "[orch] abort-on-close to runner {} failed ({e})",
                route.runner.runner_id
            );
        }
    }

    /// Abort (frontend → runner) — §6.3.
    fn route_abort(&mut self, fe: &FrontendRuntime, work_id: &str) {
        let Some(runner) = self
            .work
            .get(&(fe.session_id.clone(), work_id.to_string()))
            .map(|r| Arc::clone(&r.runner))
        else {
            println!(
                "[orch] abort for unknown work ({},{work_id})",
                fe.session_id
            );
            return;
        };
        let abort = Envelope {
            v: 1,
            id: format!("orch-abort-{work_id}"),
            reply_to: None,
            session_id: Some(fe.session_id.clone()),
            ts: None,
            body: Message::Abort(messages::Abort {
                work_id: work_id.to_string(),
            }),
        };
        if let Err(e) = runner.send(frame_envelope(&abort)) {
            eprintln!(
                "[orch] abort to runner {} failed ({e}); evicting",
                runner.runner_id
            );
            self.evict_runner(&runner.runner_id);
        }
    }

    // ── Lifecycle / eviction (§3.4, §6.5, §7) ────────────────────────────────

    fn evict_runner(&mut self, runner_id: &str) {
        let removed = self.runners.remove(runner_id);
        self.aios.remove(runner_id);
        if removed.is_none() {
            return;
        }
        // Tell the provisioner the runner is gone so a later `Provision` may re-spawn (§9.3).
        if let Some(p) = &self.provisioner {
            let modules: Vec<String> = removed
                .as_ref()
                .unwrap()
                .capabilities
                .iter()
                .filter_map(|c| match c {
                    Capability::Analysis { name, .. } => Some(name.clone()),
                    _ => None,
                })
                .collect();
            if !modules.is_empty() {
                let _ = p.send(ProvReq::RunnerGone { modules });
            }
        }
        // Drop this runner's advertisements from the discovery catalog; push to frontends only
        // if the set of available modules actually changed (its modules may still be advertised
        // by other runners or come from the libset).
        self.recompute_catalog();
        // Collect the affected work (owned clones) so the sends run with no borrow of `self.work`
        // held, then remove the entries.
        let affected = self
            .work
            .iter()
            .filter(|(_, r)| r.runner.runner_id == runner_id)
            .map(|(k, r)| (k.clone(), Arc::clone(&r.frontend), r.revision))
            .collect::<Vec<_>>();
        for (key, frontend, revision) in &affected {
            let err = no_runner_result(
                &key.1,
                *revision,
                &key.0,
                "A fatal crash occurred while running the analysis (the runner stopped unexpectedly).",
            );
            // Best-effort: the frontend may itself be gone/backpressured; its own pipe_notify reaps
            // it if dead.
            if let Err(e) = frontend.send(err) {
                eprintln!(
                    "[orch] eviction-error to frontend {} failed: {e}",
                    frontend.session_id
                );
            }
        }
        for (key, _, _) in &affected {
            self.work.remove(key);
        }
        println!(
            "[orch] runner {runner_id} evicted ({} outstanding work unit(s) failed)",
            affected.len()
        );
    }

    fn drop_frontend(&mut self, session_id: &str) {
        self.frontends.remove(session_id);
        self.aios.remove(session_id);
        // The frontend is gone; drop its work entries (results would have nowhere to go).
        self.work.retain(|(sid, _), _| sid != session_id);
        // Reclaim the whole session workspace (best-effort, off the router thread) — unless
        // workspaces are being kept for inspection.
        if !self.config.keep_workspaces {
            let _ = self.janitor.send(self.config.session_workspace(session_id));
        }
        println!("[orch] frontend {session_id} dropped");
    }

    /// Runners with outstanding work and no activity within the hang timeout (§7).
    fn hung_runners(&self) -> Vec<String> {
        let now = now_ms();
        self.runners
            .values()
            .filter(|rt| {
                rt.outstanding.load(Ordering::Relaxed) > 0
                    && now.saturating_sub(rt.last_activity.load(Ordering::Relaxed))
                        > self.config.hang_timeout_ms
            })
            .map(|rt| rt.runner_id.clone())
            .collect()
    }

    /// Hang scan (the `Tick` handler): surface wedged runners. All runners are attached in this
    /// increment, so we only report (managed → SIGKILL+rerun is deferred, §7).
    fn scan_hung(&self) {
        for runner_id in self.hung_runners() {
            eprintln!(
                "[orch] runner {runner_id} appears hung (outstanding work, no activity past timeout)"
            );
        }
    }

    // ── Module discovery (catalog maintenance) ─────────────────────────────

    /// Recompute the discovery catalog — libset modules ∪ the `base_uri`-carrying analysis
    /// advertisements of all live runners — and push it to every frontend, but **only if it
    /// actually changed**. Called on runner register and evict; pure in-memory work.
    ///
    /// Dedup is by `(name, version)` with a deterministic winner, so identical availability
    /// never churns the wire:
    /// * libset beats runners — a stable, orchestrator-scanned source wins over a runner that
    ///   advertises the same module+version (a dev runner re-advertising a libset module causes
    ///   no push; to override assets, bump the version);
    /// * among runners, the earliest registration (lowest `seq`) wins — a redundant late
    ///   advertisement causes no push, while evicting the winner hands the module to the
    ///   next-oldest advertiser, a truthful change that IS pushed (the asset source moved).
    ///
    /// The final catalog is sorted into canonical order so the equality check (and the wire)
    /// is order-stable regardless of `HashMap` iteration order.
    fn recompute_catalog(&mut self) {
        let mut catalog = self.libset_modules.clone();
        let mut ads: Vec<(u64, ModuleInfo)> = self
            .runners
            .values()
            .flat_map(|rt| {
                rt.capabilities.iter().filter_map(move |c| match c {
                    // Advertisements without a base_uri route work but are not discoverable
                    // (the frontend could not load assets from them).
                    Capability::Analysis {
                        name,
                        version,
                        base_uri: Some(base_uri),
                    } => Some((
                        rt.seq,
                        ModuleInfo {
                            name: name.clone(),
                            version: version.clone(),
                            base_uri: base_uri.clone(),
                        },
                    )),
                    _ => None,
                })
            })
            .collect();
        // Registration order — `seq` is unique per runner, so this order is total and stable.
        ads.sort_by_key(|(seq, _)| *seq);
        for (_seq, info) in ads {
            if !catalog
                .iter()
                .any(|m| m.name == info.name && m.version == info.version)
            {
                catalog.push(info);
            }
        }
        catalog.sort();
        if catalog == self.catalog {
            return; // same set of available modules — no churn on the wire
        }
        self.catalog = catalog;
        self.broadcast_modules();
    }

    /// Push the current catalog to every connected frontend (best-effort per peer; a dead or
    /// backpressured frontend is reaped by its own `pipe_notify`). Called only from
    /// `recompute_catalog`, and only when the catalog changed.
    fn broadcast_modules(&self) {
        let frame = frame_envelope(&Self::modules_envelope(&self.catalog, None, None));
        for fe in self.frontends.values() {
            if let Err(e) = fe.send(frame.clone()) {
                eprintln!(
                    "[orch] modules push to frontend {} failed: {e}",
                    fe.session_id
                );
            }
        }
        println!(
            "[orch] module catalog changed: {} module(s), pushed to {} frontend(s)",
            self.catalog.len(),
            self.frontends.len()
        );
    }

    /// A `modules` envelope carrying `catalog`. `reply_to` = `Some(id)` answers a `list_modules`
    /// query (session-scoped); `None` is an unsolicited change push.
    fn modules_envelope(
        catalog: &[ModuleInfo],
        reply_to: Option<String>,
        session_id: Option<String>,
    ) -> Envelope {
        Envelope {
            v: 1,
            id: format!("orch-modules-{}", now_ms()),
            reply_to,
            session_id,
            ts: None,
            body: Message::Modules(messages::ModulesMsg {
                modules: catalog.to_vec(),
            }),
        }
    }
}

// ─── Broker handle (shared, thread-safe) ─────────────────────────────────────

/// The shareable handle to the broker. Holds only the router mailbox sender; all real state lives
/// on the router thread. Clones of this are handed to the control-endpoint callback and the
/// hang-detector.
struct Broker {
    tx: mpsc::Sender<RouterMsg>,
}

impl Broker {
    /// Spawn the single router thread and return a handle. The router runs for the life of the
    /// process (it holds its own mailbox sender, so its receiver never disconnects).
    fn start(config: Config) -> Arc<Broker> {
        // Scan the libset once, here — before the router thread exists. The scan is filesystem
        // I/O, which the router must never do; its results are shared: the routing indexes seed
        // the provisioner, the module metadata seeds the discovery catalog.
        let scan = config
            .provisioner
            .as_ref()
            .map(|pc| provisioner::scan_libset(&pc.libset));
        if let Some(scan) = &scan {
            println!(
                "[orch] libset scan: {} module(s) discoverable{}",
                scan.modules.len(),
                if scan.modules.is_empty() {
                    String::new()
                } else {
                    format!(
                        ": {}",
                        scan.modules
                            .iter()
                            .map(|m| format!("{}@{}", m.name, m.version))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                }
            );
        }
        // The router's mailbox is created here (not in `start_inner`) so `spawn_provisioner` can
        // hand the provisioner a direct send path onto it: provisioner outcomes are ordinary
        // mailbox messages, enqueued by the provisioner thread itself.
        let (tx, rx) = mpsc::channel();
        let provisioner = Self::spawn_provisioner(&config, scan, tx.clone());
        Self::start_inner(config, tx, rx, provisioner)
    }

    /// Start the provisioner thread from an already-completed libset scan (§9.3). The provisioner
    /// reports outcomes **directly** onto the router's mailbox via the injected `on_event`
    /// closure — a non-blocking mpsc send it makes from its own thread — so there is no relay
    /// thread and no second queue between the two. Returns the router's request sender, the park
    /// timeout, and the libset's discovery metadata (the router's catalog seed). `None` when
    /// provisioning is disabled.
    fn spawn_provisioner(
        config: &Config,
        scan: Option<provisioner::LibsetScan>,
        router_tx: mpsc::Sender<RouterMsg>,
    ) -> Option<ProvisionerHandles> {
        let pcfg = config.provisioner.as_ref()?;
        let scan = scan?;
        let modules = scan.modules.clone();
        let (req_tx, req_rx) = mpsc::channel::<ProvReq>();
        // The entire "provisioner → router" adapter: re-wrap the provisioner's own event type
        // into the router's and enqueue it. Runs on the provisioner thread — safe precisely
        // because sending on the router's unbounded mailbox never blocks the sender.
        let on_event = Box::new(move |ev: ProvEvent| match ev {
            ProvEvent::ProvisionFailed { module, reason } => {
                let _ = router_tx.send(RouterMsg::ProvisionFailed { module, reason });
            }
        });
        let provisioner = RunnerProvisioner::new(
            scan,
            pcfg.runner_script.clone(),
            pcfg.rscript_bin.clone(),
            config.control_url.clone(),
            Duration::from_millis(pcfg.spawn_timeout_ms),
            req_rx,
            on_event,
        );
        std::thread::Builder::new()
            .name("orch-provisioner".into())
            .spawn(move || provisioner.run())
            .expect("spawn provisioner thread");
        Some((req_tx, pcfg.park_timeout_ms, modules))
    }

    /// Construct the router + broker with an already-built provisioner handle (production builds it
    /// via `spawn_provisioner`; tests inject their own channels to stub the provisioner). The
    /// handle's third element is the libset's discovery metadata — the catalog seed. The router's
    /// mailbox channel is created by the caller, so whoever spawns the provisioner can hand it a
    /// direct send path onto that mailbox.
    fn start_inner(
        config: Config,
        tx: mpsc::Sender<RouterMsg>,
        rx: mpsc::Receiver<RouterMsg>,
        provisioner: Option<ProvisionerHandles>,
    ) -> Arc<Broker> {
        // Start the janitor, then wipe the orchestrator dir root: at startup no sessions are live
        // by definition, so everything under it is stale from a previous run. Best-effort.
        let janitor = start_janitor();
        let _ = janitor.send(std::path::PathBuf::from(&config.orchestrator_dir_root));
        let (prov_tx, park_timeout_ms, libset_modules) = match provisioner {
            Some((req_tx, timeout, modules)) => (Some(req_tx), timeout, modules),
            None => (None, 0, Vec::new()),
        };
        let router = Router {
            runners: HashMap::new(),
            frontends: HashMap::new(),
            work: HashMap::new(),
            parked: HashMap::new(),
            aios: HashMap::new(),
            counter: 0,
            ever_registered: false,
            config,
            nonce: BROKER_NONCE.fetch_add(1, Ordering::Relaxed),
            tx: tx.clone(),
            janitor,
            provisioner: prov_tx,
            park_timeout_ms,
            libset_modules: libset_modules.clone(),
            // The initial catalog is the libset scan itself (no runners are registered yet).
            // Nothing is pushed at startup — no frontends are connected to receive it; they
            // query with `list_modules` on arrival.
            catalog: libset_modules,
        };
        std::thread::Builder::new()
            .name("orch-router".into())
            .spawn(move || router.run(rx))
            .expect("spawn router thread");
        Arc::new(Broker { tx })
    }

    /// Arm the control REP socket's recv loop. The callback forwards each handshake to the router
    /// and waits (briefly, on a one-shot channel) for the reply envelope, which it sends back on the
    /// REP socket. REP enforces recv→send alternation, so we send the reply *then* re-arm.
    fn arm_control(self: &Arc<Broker>, control: Arc<Socket>) -> Result<(), nng::Error> {
        let broker = Arc::clone(self);
        let ctl = Arc::clone(&control);
        let aio = Aio::new(move |aio: Aio, res: AioResult| match res {
            AioResult::Recv(Ok(msg)) => {
                // REP must reply to every request before the next recv, so always produce one.
                let reply = match deframe(&msg[..]) {
                    Some(env) => {
                        let reply_to = env.id.clone();
                        let (rtx, rrx) = mpsc::channel();
                        let _ = broker.tx.send(RouterMsg::Handshake { env, reply: rtx });
                        rrx.recv().unwrap_or_else(|_| {
                            orch_err(Some(&reply_to), "router_unavailable", "router stopped")
                        })
                    }
                    None => orch_err(None, "bad_frame", "undecodable handshake"),
                };
                if let Err((_m, e)) = ctl.try_send(frame_envelope(&reply).as_slice()) {
                    eprintln!("[orch] control reply failed: {e} (handshake peer gone?)");
                }
                if let Err(e) = ctl.recv_async(&aio) {
                    eprintln!("[orch] control re-arm failed: {e}");
                }
            }
            AioResult::Recv(Err(e)) => println!("[orch] control socket closed: {e}"),
            _ => {}
        })?;
        control.recv_async(&aio)?;
        // Hand the strong Aio ref to the router so the loop stays alive.
        let _ = self.tx.send(RouterMsg::KeepAlive {
            key: "control".to_string(),
            aio,
        });
        Ok(())
    }
}

#[cfg(test)]
impl Broker {
    /// Snapshot the router's current runner ids (round-trip through the mailbox).
    fn runners_snapshot(&self) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryRunners(tx));
        rx.recv().unwrap_or_default()
    }

    /// Snapshot the router's currently-hung runner ids.
    fn hung_snapshot(&self) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryHung(tx));
        rx.recv().unwrap_or_default()
    }

    fn is_ever_registered(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryEverRegistered(tx));
        rx.recv().unwrap_or(false)
    }
}

// ─── Runner selection (§4.1, §9.4, §25.6) ────────────────────────────────────

/// Select the best runner for this work: analysis matches by module name (exact version preferred,
/// any-version fallback for the alpha); rcode matches a `Rcode` capability. Among matches, highest
/// `priority` wins; ties break to the most recent registration (`seq`). Returns `None` if no live
/// runner can serve the work.
fn select_runner(
    runners: &HashMap<String, Arc<RunnerRuntime>>,
    payload: &WorkPayload,
) -> Option<Arc<RunnerRuntime>> {
    let can_serve = |rt: &RunnerRuntime| match payload {
        WorkPayload::Analysis(a) => rt
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::Analysis { name, .. } if name == &a.module)),
        WorkPayload::Rcode(_) => rt
            .capabilities
            .iter()
            .any(|c| matches!(c, Capability::Rcode {})),
    };
    runners
        .values()
        .filter(|rt| can_serve(rt))
        .max_by_key(|rt| (rt.priority, rt.seq))
        .cloned()
}

/// The `(module, module_version)` an analysis work unit targets, or `None` for non-analysis work.
fn analysis_module(payload: &WorkPayload) -> Option<(&str, &str)> {
    match payload {
        WorkPayload::Analysis(a) => Some((a.module.as_str(), a.module_version.as_str())),
        WorkPayload::Rcode(_) => None,
    }
}

/// Build a `running` result acknowledging a parked work unit — so the frontend knows the work is
/// accepted and pending a runner, not lost (§25.5), while the provisioner spins one up.
fn running_result(work_id: &str, revision: u64, session_id: &str) -> Vec<u8> {
    let env = Envelope {
        v: 1,
        id: format!("orch-running-{work_id}"),
        reply_to: None,
        session_id: Some(session_id.to_string()),
        ts: None,
        body: Message::Result(messages::ResultMsg {
            work_id: work_id.to_string(),
            revision,
            status: Status::Running,
            payload: messages::ResultPayload {
                results: json!({ "title": "provisioning a runner" }),
            },
        }),
    };
    frame_envelope(&env)
}

/// Build a `fatalError` result for a work unit that no live runner can serve — so dead/evicted
/// runners surface as a visible error instead of the work vanishing (§25.5). `detail` is the full
/// user-facing sentence shown as the error message.
fn no_runner_result(work_id: &str, revision: u64, session_id: &str, detail: &str) -> Vec<u8> {
    let env = Envelope {
        v: 1,
        id: format!("orch-norunner-{work_id}"),
        reply_to: None,
        session_id: Some(session_id.to_string()),
        ts: None,
        body: Message::Result(messages::ResultMsg {
            work_id: work_id.to_string(),
            revision,
            status: Status::FatalError,
            payload: messages::ResultPayload {
                results: json!({
                    "error": true,
                    "errorMessage": detail,
                    "title": "Analysis could not be completed",
                }),
            },
        }),
    };
    frame_envelope(&env)
}

/// Build a generic error envelope (control-endpoint failures).
fn orch_err(reply_to: Option<&str>, code: &str, message: &str) -> Envelope {
    Envelope {
        v: 1,
        id: "orch-err".into(),
        reply_to: reply_to.map(|s| s.to_string()),
        session_id: None,
        ts: None,
        body: Message::Error(messages::ErrorMsg {
            code: code.into(),
            message: message.into(),
            work_id: None,
        }),
    }
}

// ─── Control-endpoint listen (stale-socket handling, §2.2) ───────────────────

/// Listen on the control URL. For IPC, a hard crash can leave a stale socket file; on
/// `AddressInUse` we probe-connect, and if nothing answers, unlink the stale file and retry once.
/// TCP/inproc have no stale-file problem.
fn listen_control(sock: &Socket, url: &str) -> Result<(), nng::Error> {
    match sock.listen(url) {
        Ok(()) => Ok(()),
        Err(nng::Error::AddressInUse) if url.starts_with("ipc://") => {
            let path = url.trim_start_matches("ipc://");
            let answered = Socket::new(Protocol::Pair1)
                .and_then(|p| p.dial(url))
                .is_ok();
            if !answered {
                let _ = std::fs::remove_file(path);
                sock.listen(url)
            } else {
                Err(nng::Error::AddressInUse)
            }
        }
        Err(e) => Err(e),
    }
}

// ─── Hang detector (§7) ──────────────────────────────────────────────────────

/// A dedicated slow loop (design §7 explicitly permits this) that asks the router to scan for
/// wedged runners (outstanding work with no activity past the timeout). The scan itself runs on the
/// router thread; this thread only paces it.
fn start_hang_detector(tx: mpsc::Sender<RouterMsg>) {
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if tx.send(RouterMsg::Tick).is_err() {
                break; // router gone
            }
        }
    });
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    if std::env::args().any(|a| a == "--schema") {
        let schema = schemars::schema_for!(Envelope);
        println!("{}", serde_json::to_string_pretty(&schema)?);
        return Ok(());
    }

    let config = Config::from_env();
    let control_url = config.control_url.clone();

    let control = Socket::new(Protocol::Rep0)?;
    listen_control(&control, &control_url)?;
    let control = Arc::new(control);

    let broker = Broker::start(config);
    broker.arm_control(Arc::clone(&control))?;
    start_hang_detector(broker.tx.clone());

    println!("[orch] control endpoint (REP) listening on {control_url}");
    println!("[orch] broker ready — waiting for peers");

    // Park forever; the Aio recv loops run on NNG's internal thread pool and routing runs on the
    // router thread.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

// ─── tests (§11) ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use messages::{AnalysisWork, Register, ResultMsg, ResultPayload, Settings, Work};
    use nng::options::RecvTimeout;
    use serde_json::Value;
    use std::sync::atomic::AtomicU64 as SeqAtomic;

    static TEST_SEQ: SeqAtomic = SeqAtomic::new(0);
    fn unique() -> u64 {
        TEST_SEQ.fetch_add(1, Ordering::SeqCst)
    }

    fn test_config(control_url: String) -> Config {
        Config {
            control_url,
            dataset_path: alpha_dataset_path(),
            orchestrator_dir_root: format!(
                "/tmp/jasp-orchestrator-test-{}-{}",
                std::process::id(),
                unique()
            ),
            hang_timeout_ms: 30_000,
            activity_min_ms: 1_000,
            keep_workspaces: false,
            provisioner: None,
        }
    }

    /// Start a broker on a fresh control endpoint; returns `(broker, control_socket)`. The caller
    /// must keep the returned control socket alive for the broker to keep serving.
    fn start_broker(control_url: String) -> (Arc<Broker>, Arc<Socket>) {
        let control = Socket::new(Protocol::Rep0).unwrap();
        listen_control(&control, &control_url).unwrap();
        let control = Arc::new(control);
        let broker = Broker::start(test_config(control_url));
        broker.arm_control(Arc::clone(&control)).unwrap();
        (broker, control)
    }

    fn envelope(body: Message) -> Envelope {
        Envelope {
            v: 1,
            id: format!("t-{}", unique()),
            reply_to: None,
            session_id: None,
            ts: None,
            body,
        }
    }

    fn analysis_work(id: &str, module: &str) -> Envelope {
        analysis_work_rev(id, module, 0, None)
    }

    /// `analysis_work` with an explicit revision + optional base revision (to seed recompute).
    fn analysis_work_rev(
        id: &str,
        module: &str,
        revision: u64,
        base_revision: Option<u64>,
    ) -> Envelope {
        envelope(Message::Work(Work {
            work_id: id.to_string(),
            revision,
            base_revision,
            dataset_ids: vec!["ds-001".to_string()],
            payload: WorkPayload::Analysis(AnalysisWork {
                module: module.to_string(),
                module_version: "0.1".to_string(),
                analysis: "A".to_string(),
                options: Value::Null,
                settings: Settings {
                    ppi: 96,
                    num_decimals: 3,
                },
            }),
        }))
    }

    /// REQ-register a mock runner; returns its PAIR data channel and assigned runner_id.
    fn register_runner(control_url: &str, module: &str) -> (Socket, String) {
        register_runner_full(control_url, module, 0, None)
    }

    /// Register a runner that advertises a `base_uri` for its module (discovery metadata) — for
    /// catalog tests.
    fn register_runner_with_uri(
        control_url: &str,
        module: &str,
        base_uri: &str,
    ) -> (Socket, String) {
        register_runner_full(control_url, module, 0, Some(base_uri.to_string()))
    }

    fn register_runner_full(
        control_url: &str,
        module: &str,
        priority: u32,
        base_uri: Option<String>,
    ) -> (Socket, String) {
        let req = Socket::new(Protocol::Req0).unwrap();
        req.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        req.dial(control_url).unwrap();
        let reg = envelope(Message::Register(Register {
            runner_id: None,
            capabilities: vec![Capability::Analysis {
                name: module.to_string(),
                version: "0.1".to_string(),
                base_uri,
            }],
            priority,
            environment: Value::Null,
        }));
        req.send(frame_envelope(&reg).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let ack_raw = req.recv().expect("register_ack");
        let ack = deframe(&ack_raw[..]).expect("valid ack");
        let (runner_id, channel_url) = match ack.body {
            Message::RegisterAck(a) => {
                assert!(a.ok, "registration accepted: {:?}", a.reason);
                (
                    a.runner_id.unwrap(),
                    a.channel_url.expect("channel_url present"),
                )
            }
            other => panic!("expected register_ack, got {other:?}"),
        };
        drop(req);
        let ch = Socket::new(Protocol::Pair1).unwrap();
        ch.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        // Peer-side buffering (64 msgs): the broker's non-blocking try_send needs the peer to have
        // a recv buffer so the send can land even if the test hasn't posted recv yet.
        ch.set_opt::<SendBufferSize>(64).unwrap();
        ch.set_opt::<RecvBufferSize>(64).unwrap();
        ch.dial(&channel_url).unwrap();
        (ch, runner_id)
    }

    /// REQ-hello a mock frontend; returns its PAIR data channel and assigned session_id.
    /// The orchestrator pushes the current catalog as the FIRST frame on the channel (the
    /// connect-time push); this helper consumes it so callers' recvs see only what they ask
    /// for. Discovery tests that observe the push itself use [`hello_frontend_raw`].
    fn hello_frontend(control_url: &str) -> (Socket, String) {
        let (ch, session_id) = hello_frontend_raw(control_url);
        let env = deframe(&ch.recv().expect("initial catalog frame")[..]).expect("valid envelope");
        assert!(
            matches!(env.body, Message::Modules(_)),
            "first channel frame is the connect-time catalog push, got {:?}",
            env.body
        );
        assert!(env.reply_to.is_none(), "connect-time push is unsolicited");
        (ch, session_id)
    }

    /// `hello_frontend` WITHOUT consuming the connect-time catalog frame — for discovery tests.
    fn hello_frontend_raw(control_url: &str) -> (Socket, String) {
        let req = Socket::new(Protocol::Req0).unwrap();
        req.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        req.dial(control_url).unwrap();
        let hello = envelope(Message::Hello(messages::Hello {
            client_id: Some("test-client".to_string()),
            client_version: Some("0.0.0".to_string()),
        }));
        req.send(frame_envelope(&hello).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let welcome_raw = req.recv().expect("welcome");
        let welcome = deframe(&welcome_raw[..]).expect("valid welcome");
        // The assigned session rides on the envelope's session_id (see messages::Welcome).
        let session_id = welcome.session_id.clone().expect("session_id on envelope");
        let channel_url = match &welcome.body {
            Message::Welcome(w) => {
                assert!(w.ok, "hello accepted: {:?}", w.error);
                w.channel_url.clone().expect("channel_url present")
            }
            other => panic!("expected welcome, got {other:?}"),
        };
        drop(req);
        let ch = Socket::new(Protocol::Pair1).unwrap();
        ch.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        // Peer-side buffering (64 msgs): the broker's non-blocking try_send needs the peer to have
        // a recv buffer so the send can land even if the test hasn't posted recv yet.
        ch.set_opt::<SendBufferSize>(64).unwrap();
        ch.set_opt::<RecvBufferSize>(64).unwrap();
        ch.dial(&channel_url).unwrap();
        (ch, session_id)
    }

    /// Send a `result` on a runner channel, echoing the session_id/work_id/revision of the work it
    /// answers (as a real runner must, so the orchestrator can correlate).
    fn send_result(runner_ch: &Socket, session_id: &str, work_id: &str, revision: u64) {
        let result = Envelope {
            v: 1,
            id: format!("rn-{work_id}"),
            reply_to: None,
            session_id: Some(session_id.to_string()),
            ts: None,
            body: Message::Result(ResultMsg {
                work_id: work_id.to_string(),
                revision,
                status: Status::Complete,
                payload: ResultPayload {
                    results: json!({"title": "ok"}),
                },
            }),
        };
        runner_ch
            .send(frame_envelope(&result).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
    }

    // 1. Registration → channel. Registration is synchronous (the control callback waits for the
    //    router to insert before replying), so the registry already contains the runner once the
    //    ack has landed — no polling needed.
    #[test]
    fn registration_yields_a_usable_channel() {
        let url = format!("inproc://orch-reg-{}", unique());
        let (broker, _ctl) = start_broker(url.clone());
        let (_ch, runner_id) = register_runner(&url, "jaspTTests");
        assert!(
            runner_id.starts_with("r-"),
            "assigned id is r-N: {runner_id}"
        );
        assert!(
            broker.runners_snapshot().contains(&runner_id),
            "runner registered"
        );
    }

    // 2. Two-runner targeted routing.
    #[test]
    fn work_routes_only_to_the_matching_runner() {
        let url = format!("inproc://orch-route-{}", unique());
        let (_broker, _ctl) = start_broker(url.clone());
        let (runner_x, _id_x) = register_runner(&url, "moduleX");
        let (runner_y, _id_y) = register_runner(&url, "moduleY");
        let (frontend, _session) = hello_frontend(&url);

        // Shorten runner_y's timeout so the negative assertion is quick.
        runner_y
            .set_opt::<RecvTimeout>(Some(Duration::from_millis(300)))
            .unwrap();

        frontend
            .send(frame_envelope(&analysis_work("w-x", "moduleX")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();

        let got = runner_x
            .recv()
            .expect("runner_x receives its module's work");
        match deframe(&got[..]).unwrap().body {
            Message::Work(w) => assert_eq!(w.work_id, "w-x"),
            other => panic!("expected work, got {other:?}"),
        }
        assert!(
            runner_y.recv().is_err(),
            "runner_y must NOT receive work for a module it does not advertise"
        );
    }

    // 3. Frontend channel + correlation round trip.
    #[test]
    fn frontend_round_trips_with_correlation() {
        let url = format!("inproc://orch-corr-{}", unique());
        let (_broker, _ctl) = start_broker(url.clone());
        let (runner, _rid) = register_runner(&url, "jaspDescriptives");
        let (frontend, session) = hello_frontend(&url);

        frontend
            .send(frame_envelope(&analysis_work("w-1", "jaspDescriptives")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();

        // Runner receives work with session_id stamped + dataset_paths/output_dir injected.
        let raw = runner.recv().expect("runner recv work");
        let env = deframe(&raw[..]).unwrap();
        assert_eq!(
            env.session_id.as_deref(),
            Some(session.as_str()),
            "session_id stamped"
        );
        let value: Value = serde_json::from_slice(&raw[4..]).unwrap();
        assert!(
            value.get("dataset_paths").is_some(),
            "dataset_paths injected"
        );
        assert!(value.get("output_dir").is_some(), "output_dir injected");
        match &env.body {
            Message::Work(w) => assert_eq!(w.work_id, "w-1"),
            other => panic!("expected work, got {other:?}"),
        }

        // Runner sends a result echoing session_id/work_id; frontend must receive it.
        send_result(&runner, &session, "w-1", 0);
        let res_raw = frontend.recv().expect("frontend recv result");
        let res = deframe(&res_raw[..]).unwrap();
        match res.body {
            Message::Result(r) => {
                assert_eq!(r.work_id, "w-1");
                assert_eq!(r.revision, 0);
                assert!(matches!(r.status, Status::Complete));
            }
            other => panic!("expected result, got {other:?}"),
        }
    }

    // 3b. work_close aborts in-flight work and is idempotent (§19.1).
    #[test]
    fn work_close_aborts_outstanding_and_is_idempotent() {
        let url = format!("inproc://orch-close-{}", unique());
        let (_broker, _ctl) = start_broker(url.clone());
        let (runner, _rid) = register_runner(&url, "jaspTTests");
        let (frontend, _session) = hello_frontend(&url);

        // Submit work; the runner receives it (now outstanding on the orchestrator).
        frontend
            .send(frame_envelope(&analysis_work("w-close", "jaspTTests")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let got = runner.recv().expect("runner recv work");
        match deframe(&got[..]).unwrap().body {
            Message::Work(w) => assert_eq!(w.work_id, "w-close"),
            other => panic!("expected work, got {other:?}"),
        }

        // Frontend discards the work -> orchestrator aborts the in-flight work on the runner.
        let close = envelope(Message::WorkClose(messages::WorkClose {
            work_id: "w-close".into(),
            revision: None,
        }));
        frontend
            .send(frame_envelope(&close).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let abort_raw = runner.recv().expect("runner recv abort");
        match deframe(&abort_raw[..]).unwrap().body {
            Message::Abort(a) => assert_eq!(a.work_id, "w-close"),
            other => panic!("expected abort, got {other:?}"),
        }

        // Closing again is a no-op: the route is already gone, so no second abort is sent.
        frontend
            .send(frame_envelope(&close).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        runner
            .set_opt::<RecvTimeout>(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(
            runner.recv().is_err(),
            "idempotent close must not send a second abort"
        );
    }

    // 3c. Per-revision isolation: output_dir is results_<rev>, base_revision injects
    //     base_results_dir to seed recompute, and work_close is revision-granular (prunes one
    //     revision; aborts only the in-flight one).
    #[test]
    fn per_revision_workspace_and_granular_close() {
        let url = format!("inproc://orch-rev-{}", unique());
        let (_broker, _ctl) = start_broker(url.clone());
        let (runner, _rid) = register_runner(&url, "jaspTTests");
        let (frontend, _session) = hello_frontend(&url);

        // rev 0, no base: output_dir ends in results_0; no base_results_dir.
        frontend
            .send(frame_envelope(&analysis_work_rev("w-rev", "jaspTTests", 0, None)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let raw0 = runner.recv().expect("runner recv work rev0");
        let v0: Value = serde_json::from_slice(&raw0[4..]).unwrap();
        assert!(
            v0["output_dir"].as_str().unwrap().ends_with("results_0"),
            "output_dir is per-revision: {}",
            v0["output_dir"]
        );
        assert!(v0.get("base_results_dir").is_none(), "no base for rev 0");

        // rev 1, base = 0: output_dir ends in results_1; base_results_dir ends in results_0.
        frontend
            .send(frame_envelope(&analysis_work_rev("w-rev", "jaspTTests", 1, Some(0))).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let raw1 = runner.recv().expect("runner recv work rev1");
        let v1: Value = serde_json::from_slice(&raw1[4..]).unwrap();
        assert!(
            v1["output_dir"].as_str().unwrap().ends_with("results_1"),
            "output_dir is per-revision: {}",
            v1["output_dir"]
        );
        assert!(
            v1["base_results_dir"]
                .as_str()
                .unwrap()
                .ends_with("results_0"),
            "base_results_dir seeds from the base revision: {}",
            v1["base_results_dir"]
        );

        // The route now tracks rev 1 (latest wins). Close rev 0 (NOT in flight): no abort, prune
        // only results_0 — the runner must NOT receive an abort.
        let close0 = envelope(Message::WorkClose(messages::WorkClose {
            work_id: "w-rev".into(),
            revision: Some(0),
        }));
        frontend
            .send(frame_envelope(&close0).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        runner
            .set_opt::<RecvTimeout>(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(
            runner.recv().is_err(),
            "closing a non-in-flight revision must not abort the runner"
        );

        // Close rev 1 (the in-flight revision): abort sent.
        runner
            .set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        let close1 = envelope(Message::WorkClose(messages::WorkClose {
            work_id: "w-rev".into(),
            revision: Some(1),
        }));
        frontend
            .send(frame_envelope(&close1).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let abort_raw = runner.recv().expect("runner recv abort for in-flight rev1");
        match deframe(&abort_raw[..]).unwrap().body {
            Message::Abort(a) => assert_eq!(a.work_id, "w-rev"),
            other => panic!("expected abort, got {other:?}"),
        }
    }

    // 4. Disconnect eviction → outstanding work fails to the frontend (no silent loss).
    #[test]
    fn runner_disconnect_evicts_and_fails_outstanding_work() {
        let url = format!("inproc://orch-evict-{}", unique());
        let (broker, _ctl) = start_broker(url.clone());
        let (runner, rid) = register_runner(&url, "jaspTTests");
        let (frontend, _session) = hello_frontend(&url);

        frontend
            .send(frame_envelope(&analysis_work("w-orphan", "jaspTTests")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let _ = runner.recv().expect("runner recv work"); // now outstanding == 1

        drop(runner); // ungraceful disconnect → channel close → evict

        // The frontend must receive a fatalError for the orphaned work.
        let raw = frontend.recv().expect("frontend recv fatalError");
        let env = deframe(&raw[..]).unwrap();
        match env.body {
            Message::Result(r) => {
                assert_eq!(r.work_id, "w-orphan");
                assert!(
                    matches!(r.status, Status::FatalError),
                    "status is fatalError"
                );
            }
            other => panic!("expected fatalError result, got {other:?}"),
        }
        // Registry evicted.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !broker.runners_snapshot().contains(&rid) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "runner evicted");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    // No-silent-loss: after a runner disconnects, fresh work for its module errors.
    #[test]
    fn work_errors_when_no_live_runner() {
        let url = format!("inproc://orch-norunner-{}", unique());
        let (broker, _ctl) = start_broker(url.clone());
        let (runner, rid) = register_runner(&url, "jaspTTests");
        drop(runner);
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while broker.runners_snapshot().contains(&rid) {
            assert!(std::time::Instant::now() < deadline, "evicted");
            std::thread::sleep(Duration::from_millis(10));
        }

        let (frontend, _session) = hello_frontend(&url);
        frontend
            .send(frame_envelope(&analysis_work("w-dead", "jaspTTests")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let raw = frontend.recv().expect("frontend recv error result");
        match deframe(&raw[..]).unwrap().body {
            Message::Result(r) => assert!(matches!(r.status, Status::FatalError)),
            other => panic!("expected fatalError, got {other:?}"),
        }
    }

    // 5. Liveness: outstanding + stale last_activity → the router's hang scan flags the runner.
    //    Black-box: route a real work unit, let the runner go silent, and query the scan.
    #[test]
    fn hang_detection_flags_a_wedged_runner() {
        let url = format!("inproc://orch-hang-{}", unique());
        let mut cfg = test_config(url.clone());
        cfg.hang_timeout_ms = 50; // short, for the test
        let control = Socket::new(Protocol::Rep0).unwrap();
        listen_control(&control, &url).unwrap();
        let control = Arc::new(control);
        let broker = Broker::start(cfg);
        broker.arm_control(Arc::clone(&control)).unwrap();

        let (runner, rid) = register_runner(&url, "jaspTTests");
        let (frontend, _session) = hello_frontend(&url);
        // No outstanding work yet → not hung.
        assert!(broker.hung_snapshot().is_empty());

        // Route a real work unit; the runner receives it but never answers, so outstanding stays 1
        // and last_activity (bumped only on runner→orchestrator traffic) goes stale. The runner
        // socket is kept alive (not dropped) so the runner stays registered but silent.
        frontend
            .send(frame_envelope(&analysis_work("w-hang", "jaspTTests")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let _ = runner.recv().expect("runner recv work"); // outstanding == 1; runner now silent

        // Wait past the hang timeout; the router's scan flags the wedged runner.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if broker.hung_snapshot() == vec![rid.clone()] {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "wedged runner flagged"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = runner; // keep the (silent) runner socket alive to the end of the test
    }

    /// Start a broker on an OS-assigned ephemeral TCP control endpoint — exercises the real
    /// `tcp://host:0` + readback path (and proves that dropping the `Listener` handle does not stop
    /// the listener; the socket keeps it alive).
    fn start_broker_tcp() -> (Arc<Broker>, String, Arc<Socket>) {
        let control = Socket::new(Protocol::Rep0).unwrap();
        let listener = Listener::new(&control, "tcp://127.0.0.1:0").unwrap();
        let url = readback_url(&listener).unwrap();
        let control = Arc::new(control);
        let broker = Broker::start(test_config(url.clone()));
        broker.arm_control(Arc::clone(&control)).unwrap();
        (broker, url, control)
    }

    // 6. Transport-agnostic over tcp:// (OS-assigned ephemeral control + channel ports).
    #[test]
    fn routing_over_tcp() {
        let (broker, url, _ctl) = start_broker_tcp();
        let (runner, _rid) = register_runner(&url, "jaspTTests");
        let (frontend, session) = hello_frontend(&url);

        frontend
            .send(frame_envelope(&analysis_work("w-tcp", "jaspTTests")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let raw = runner.recv().expect("runner recv work over tcp");
        match deframe(&raw[..]).unwrap().body {
            Message::Work(w) => assert_eq!(w.work_id, "w-tcp"),
            other => panic!("expected work, got {other:?}"),
        }
        send_result(&runner, &session, "w-tcp", 0);
        let res_raw = frontend.recv().expect("frontend recv result over tcp");
        match deframe(&res_raw[..]).unwrap().body {
            Message::Result(r) => assert_eq!(r.work_id, "w-tcp"),
            other => panic!("expected result, got {other:?}"),
        }
        let _ = broker; // keep the broker (and its channels) alive to the end of the test
    }

    // 7. Transport-agnostic over ipc:// (unique-path channels).
    #[test]
    fn routing_over_ipc() {
        // A unique filesystem ipc control endpoint (the production transport on Linux).
        let path = format!(
            "/tmp/jasp-orch-test-{}-{}.sock",
            std::process::id(),
            unique()
        );
        let _ = std::fs::remove_file(&path);
        let url = format!("ipc://{path}");
        let (broker, _ctl) = start_broker(url.clone());

        let (runner, _rid) = register_runner(&url, "jaspTTests");
        let (frontend, session) = hello_frontend(&url);

        frontend
            .send(frame_envelope(&analysis_work("w-ipc", "jaspTTests")).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let raw = runner.recv().expect("runner recv work over ipc");
        match deframe(&raw[..]).unwrap().body {
            Message::Work(w) => assert_eq!(w.work_id, "w-ipc"),
            other => panic!("expected work, got {other:?}"),
        }
        send_result(&runner, &session, "w-ipc", 0);
        let res_raw = frontend.recv().expect("frontend recv result over ipc");
        match deframe(&res_raw[..]).unwrap().body {
            Message::Result(r) => assert_eq!(r.work_id, "w-ipc"),
            other => panic!("expected result, got {other:?}"),
        }
        let _ = broker; // keep the broker (and its channels) alive to the end of the test
    }

    // Cross-language Rust↔R disconnect (requires Rscript + nanonext), adapted to the handshake.
    #[test]
    #[ignore = "requires Rscript + nanonext (cross-language Rust↔R)"]
    fn r_runner_disconnect_is_detected() {
        let url = format!("tcp://127.0.0.1:{}", 18000 + (std::process::id() % 1000));
        let (broker, _ctl) = start_broker(url.clone());

        let probe = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../refactor_design/probe_register_disconnect.R");
        let status = match std::process::Command::new("Rscript")
            .arg(&probe)
            .arg(&url)
            .status()
        {
            Ok(s) => s,
            Err(e) => {
                eprintln!("SKIPPING r_runner_disconnect_is_detected: cannot spawn Rscript ({e})");
                return;
            }
        };
        assert!(status.success(), "R probe exited non-zero");

        // The probe registered then disconnected; the registry must end empty.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        loop {
            if broker.runners_snapshot().is_empty() && broker.is_ever_registered() {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "R runner registered then evicted on disconnect"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    // ── Provisioner (parking / dispatch / failure) tests (§9.3) ──────────────
    //
    // These drive the router's provisioning logic with a STUB provisioner: the router's outbound
    // `ProvReq`s are observed on a channel the test owns, and the test injects `ProvEvent`s that
    // a tiny test-only pump carries onto the router's mailbox (the send the real provisioner
    // makes itself, via its `on_event` closure). No runner process is spawned, so these run
    // without Rscript.

    /// Start a broker with a test-controlled stub provisioner. Returns the broker handle, the
    /// control socket (keep alive), the receiver that observes the router's `ProvReq`s, and the
    /// sender the test uses to inject `ProvEvent`s. `libset_modules` seeds the discovery catalog,
    /// as a real startup libset scan would.
    ///
    /// In production the provisioner enqueues its own events onto the router's mailbox via the
    /// injected `on_event` closure; here the test plays the provisioner, so a small test-only
    /// pump carries the injected events the same way (production has no such pump).
    fn start_broker_with_stub_provisioner(
        control_url: String,
        park_timeout_ms: u64,
        libset_modules: Vec<messages::ModuleInfo>,
    ) -> (
        Arc<Broker>,
        Arc<Socket>,
        std::sync::mpsc::Receiver<crate::provisioner::ProvReq>,
        std::sync::mpsc::Sender<crate::provisioner::ProvEvent>,
    ) {
        let control = Socket::new(Protocol::Rep0).unwrap();
        listen_control(&control, &control_url).unwrap();
        let control = Arc::new(control);
        let (req_tx, req_rx) = std::sync::mpsc::channel::<crate::provisioner::ProvReq>();
        let (event_tx, event_rx) = std::sync::mpsc::channel::<crate::provisioner::ProvEvent>();
        let (tx, rx) = std::sync::mpsc::channel::<RouterMsg>();
        let pump = tx.clone();
        std::thread::spawn(move || {
            while let Ok(ev) = event_rx.recv() {
                match ev {
                    crate::provisioner::ProvEvent::ProvisionFailed { module, reason } => {
                        let _ = pump.send(RouterMsg::ProvisionFailed { module, reason });
                    }
                }
            }
        });
        let broker = Broker::start_inner(
            test_config(control_url),
            tx,
            rx,
            Some((req_tx, park_timeout_ms, libset_modules)),
        );
        broker.arm_control(Arc::clone(&control)).unwrap();
        (broker, control, req_rx, event_tx)
    }

    /// A [`messages::ModuleInfo`] triple, for catalog assertions.
    fn module_info(name: &str, version: &str, base_uri: &str) -> messages::ModuleInfo {
        messages::ModuleInfo {
            name: name.to_string(),
            version: version.to_string(),
            base_uri: base_uri.to_string(),
        }
    }

    // 8. A routing miss with a provisioner parks the work (frontend gets a `running` marker, the
    //    provisioner gets a `Provision` request); when a matching runner registers, the parked work
    //    is dispatched to it and the result round-trips to the frontend.
    #[test]
    fn parked_work_is_dispatched_when_a_runner_registers() {
        let url = format!("inproc://orch-prov-park-{}", unique());
        let (broker, _ctl, req_rx, _event_tx) =
            start_broker_with_stub_provisioner(url.clone(), 60_000, vec![]);

        // Frontend sends analysis work for module "M"; no runner exists yet.
        let (fe_ch, session_id) = hello_frontend(&url);
        let work = analysis_work("w-park", "M");
        fe_ch
            .send(frame_envelope(&work).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();

        // 1. Frontend gets a `running` marker (work accepted, pending a runner).
        let running = deframe(&fe_ch.recv().expect("running marker")[..]).expect("valid marker");
        match &running.body {
            Message::Result(r) => {
                assert_eq!(r.work_id, "w-park");
                assert!(
                    matches!(r.status, Status::Running),
                    "expected Running, got {:?}",
                    r.status
                );
            }
            other => panic!("expected running result, got {other:?}"),
        }

        // 2. The router asked the provisioner for module "M".
        match req_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("provision req")
        {
            crate::provisioner::ProvReq::Provision { module, .. } => assert_eq!(module, "M"),
            other => panic!("expected Provision, got {other:?}"),
        }

        // 3. A runner advertising "M" registers → the parked work is dispatched to it.
        let (runner_ch, runner_id) = register_runner(&url, "M");
        let dispatched =
            deframe(&runner_ch.recv().expect("dispatched work")[..]).expect("valid work");
        match &dispatched.body {
            Message::Work(w) => {
                assert_eq!(w.work_id, "w-park");
                assert_eq!(dispatched.session_id.as_deref(), Some(session_id.as_str()));
            }
            other => panic!("expected work, got {other:?}"),
        }

        // 4. Runner completes; the frontend gets the complete result (full round-trip).
        send_result(&runner_ch, &session_id, "w-park", 0);
        let result = deframe(&fe_ch.recv().expect("complete result")[..]).expect("valid result");
        match &result.body {
            Message::Result(r) => {
                assert_eq!(r.work_id, "w-park");
                assert!(
                    matches!(r.status, Status::Complete),
                    "expected Complete, got {:?}",
                    r.status
                );
            }
            other => panic!("expected complete result, got {other:?}"),
        }

        // The runner must NOT have been evicted by the dispatch (it is still registered).
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !broker.runners_snapshot().is_empty(),
            "runner {runner_id} should still be registered after dispatch"
        );
    }

    // 9. A provisioner failure (`ProvisionFailed`) fails the work parked for that module back to the
    //    frontend (no-silent-loss).
    #[test]
    fn provision_failure_fails_parked_work() {
        let url = format!("inproc://orch-prov-fail-{}", unique());
        let (_broker, _ctl, req_rx, event_tx) =
            start_broker_with_stub_provisioner(url.clone(), 60_000, vec![]);

        let (fe_ch, _session_id) = hello_frontend(&url);
        let work = analysis_work("w-fail", "M");
        fe_ch
            .send(frame_envelope(&work).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();

        // Frontend gets the `running` marker; the router requested a runner for "M".
        let running = deframe(&fe_ch.recv().expect("running marker")[..]).expect("valid marker");
        assert!(matches!(&running.body, Message::Result(r) if matches!(r.status, Status::Running)));
        let _ = req_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("provision req");

        // The provisioner reports it cannot provide "M" → onto the router's mailbox → the router
        // fails the parked work.
        event_tx
            .send(crate::provisioner::ProvEvent::ProvisionFailed {
                module: "M".to_string(),
                reason: "not in libset".to_string(),
            })
            .unwrap();

        let err = deframe(&fe_ch.recv().expect("fatal error")[..]).expect("valid result");
        match &err.body {
            Message::Result(r) => {
                assert_eq!(r.work_id, "w-fail");
                assert!(
                    matches!(r.status, Status::FatalError),
                    "expected FatalError, got {:?}",
                    r.status
                );
            }
            other => panic!("expected fatal error result, got {other:?}"),
        }
    }

    // 9b. A parked work superseded by a newer revision before its runner arrives: the runner is
    //     dispatched the NEW revision — otherwise the client would drop the stale result (§23)
    //     and the analysis would spin forever. The resubmit must not re-request a runner either.
    #[test]
    fn parked_work_is_superseded_by_a_newer_revision() {
        let url = format!("inproc://orch-prov-supersede-{}", unique());
        let (_broker, _ctl, req_rx, _event_tx) =
            start_broker_with_stub_provisioner(url.clone(), 60_000, vec![]);

        let (fe_ch, session_id) = hello_frontend(&url);

        // rev 0 parks; rev 1 (an options change) supersedes the parked envelope.
        for rev in [0u64, 1] {
            let work = analysis_work_rev("w-sup", "M", rev, None);
            fe_ch
                .send(frame_envelope(&work).as_slice())
                .map_err(|(_, e)| e)
                .unwrap();
            let marker = deframe(&fe_ch.recv().expect("running marker")[..]).expect("valid marker");
            assert!(
                matches!(&marker.body, Message::Result(r) if matches!(r.status, Status::Running))
            );
        }

        // Exactly one Provision request (the rev-1 resubmit must not re-request).
        match req_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("provision req")
        {
            crate::provisioner::ProvReq::Provision { module, .. } => assert_eq!(module, "M"),
            other => panic!("expected Provision, got {other:?}"),
        }
        assert!(
            req_rx.recv_timeout(Duration::from_millis(300)).is_err(),
            "no duplicate Provision for a superseding revision"
        );

        // Runner arrives → the dispatched work carries revision 1 (the superseding one).
        let (runner_ch, _rid) = register_runner(&url, "M");
        let dispatched =
            deframe(&runner_ch.recv().expect("dispatched work")[..]).expect("valid work");
        match &dispatched.body {
            Message::Work(w) => {
                assert_eq!(w.work_id, "w-sup");
                assert_eq!(w.revision, 1, "the superseding revision must be dispatched");
                assert_eq!(dispatched.session_id.as_deref(), Some(session_id.as_str()));
            }
            other => panic!("expected work, got {other:?}"),
        }
    }

    // 10. `scan_libset` discovers modules by the `jasp*` + `Description.qml` rule (and `is_module_dir`
    //     agrees), ignoring non-module and non-jasp directories. Layout is two-level, mirroring the
    //     real renv libset: a libset base holds one libpath (an R library) per module, named after
    //     it; each libpath holds the module plus its framework and dependencies as package
    //     subdirectories. Analysis modules are the `jasp*` packages shipping a Description.qml;
    //     framework (jaspBase, no Description.qml) and deps (ggplot2, not jasp-prefixed) are excluded.
    #[test]
    fn scan_libset_finds_jasp_modules() {
        let base = std::env::temp_dir().join(format!(
            "jasp-libset-test-{}-{}",
            std::process::id(),
            unique()
        ));
        // libpath "jaspTTests": module jaspTTests (qml) + framework jaspBase (no qml) + dep ggplot2.
        let lib_ttests = base.join("jaspTTests");
        let mod_ttests = lib_ttests.join("jaspTTests");
        let framework = lib_ttests.join("jaspBase");
        let dep = lib_ttests.join("ggplot2");
        // libpath "jaspAnova": module jaspAnova (qml).
        let lib_anova = base.join("jaspAnova");
        let mod_anova = lib_anova.join("jaspAnova");
        for d in [&mod_ttests, &framework, &dep, &mod_anova] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(mod_ttests.join("Description.qml"), "// qml").unwrap();
        std::fs::write(mod_anova.join("Description.qml"), "// qml").unwrap();
        std::fs::write(dep.join("Description.qml"), "// qml").unwrap(); // dep has qml but isn't jasp*
        // DESCRIPTION files (DCF) — name/version come from here, not the directory name. The
        // jaspTTests one carries a multi-line field to prove continuation lines are tolerated.
        std::fs::write(
            mod_ttests.join("DESCRIPTION"),
            "Package: jaspTTests\nVersion: 1.2.3\nDescription: something\n long and continued\n",
        )
        .unwrap();
        std::fs::write(
            mod_anova.join("DESCRIPTION"),
            "Package: jaspAnova\nVersion: 4.5.6\n",
        )
        .unwrap();

        let scan = crate::provisioner::scan_libset(std::slice::from_ref(&base));
        let module_lib = &scan.module_lib;
        let lib_modules = &scan.lib_modules;

        assert_eq!(
            module_lib.len(),
            2,
            "expected 2 modules, got {module_lib:?}"
        );
        assert_eq!(module_lib.get("jaspTTests"), Some(&lib_ttests));
        assert_eq!(module_lib.get("jaspAnova"), Some(&lib_anova));
        assert!(
            !module_lib.contains_key("jaspBase"),
            "framework: no Description.qml"
        );
        assert!(
            !module_lib.contains_key("ggplot2"),
            "dep: not jasp-prefixed"
        );

        assert_eq!(
            lib_modules.get(&lib_ttests).cloned().unwrap_or_default(),
            vec!["jaspTTests".to_string()]
        );
        assert_eq!(
            lib_modules.get(&lib_anova).cloned().unwrap_or_default(),
            vec!["jaspAnova".to_string()]
        );

        assert!(crate::provisioner::is_module_dir(&mod_ttests));
        assert!(!crate::provisioner::is_module_dir(&framework));
        assert!(!crate::provisioner::is_module_dir(&dep));

        // Discovery metadata: name/version from DESCRIPTION, base URI the scanned directory
        // itself (never constructed from the module name), sorted by name.
        assert_eq!(
            scan.modules,
            vec![
                module_info(
                    "jaspAnova",
                    "4.5.6",
                    &format!("file://{}/", mod_anova.display())
                ),
                module_info(
                    "jaspTTests",
                    "1.2.3",
                    &format!("file://{}/", mod_ttests.display())
                ),
            ],
            "catalog metadata from DESCRIPTION, canonically sorted"
        );

        std::fs::remove_dir_all(&base).ok();
    }

    // 10b. `file_uri` percent-encodes reserved bytes (spaces, etc.) so base URIs survive paths
    //      that are not URL-safe, and always yields a trailing-slash file:// URI.
    #[test]
    fn file_uri_percent_encodes_reserved_bytes() {
        let dir = std::env::temp_dir().join("jasp uri test").join("foo bar");
        let uri = crate::provisioner::file_uri(&dir);
        assert!(uri.starts_with("file://"), "{uri}");
        assert!(uri.ends_with('/'), "trailing slash: {uri}");
        assert!(!uri.contains(' '), "spaces encoded: {uri}");
        assert!(uri.contains("jasp%20uri%20test/foo%20bar/"), "{uri}");
    }

    // 11. A standard PAIR v1 socket buffers a `try_send` issued before any peer connects, delivering
    //     it once a peer dials. This is the load-bearing assumption behind dispatch-on-registration
    //     (`try_dispatch_parked` sends to a freshly-registered runner before it dials its data
    //     channel). The sleep makes the check non-temporal: a discarding (polyamorous) socket would
    //     drop the message regardless of the gap, so delivery here proves buffering, not a race.
    fn assert_preconnect_buffered(candidate_url: &str) {
        let srv = Socket::new(Protocol::Pair1).unwrap();
        srv.set_opt::<SendBufferSize>(256).unwrap();
        let listener = Listener::new(&srv, candidate_url).unwrap();
        // Mirror allocate_channel: inproc dials the candidate verbatim; only tcp needs the
        // ephemeral-port readback (readback_url does not round-trip to a dialable inproc address).
        let dial_url = if candidate_url.starts_with("inproc://") {
            candidate_url.to_string()
        } else {
            readback_url(&listener).unwrap()
        };

        let payload = b"dispatched-before-connect";
        srv.try_send(payload.as_slice())
            .map_err(|(_, e)| e)
            .unwrap(); // no peer yet
        std::thread::sleep(Duration::from_millis(100)); // send strictly precedes the connect

        let cli = Socket::new(Protocol::Pair1).unwrap();
        cli.set_opt::<RecvBufferSize>(64).unwrap();
        cli.set_opt::<RecvTimeout>(Some(Duration::from_secs(3)))
            .unwrap();
        cli.dial(&dial_url).unwrap();

        let msg = cli
            .recv()
            .expect("pre-connect send must be delivered once a peer connects");
        assert_eq!(msg.as_slice(), payload);
    }

    #[test]
    fn preconnect_send_is_buffered_until_a_peer_connects() {
        assert_preconnect_buffered(&format!("inproc://preconnect-{}", unique())); // unit-test transport
        assert_preconnect_buffered("tcp://127.0.0.1:0"); // production transport (ephemeral port)
    }

    // ── Module discovery (catalog) tests ────────────────────────────────────

    // 12. `list_modules` is answered with the current catalog, correlated by `reply_to`:
    //     uri-carrying runner advertisements are discoverable; an advertisement WITHOUT a
    //     base_uri (a pure compute runner) still routes work but is omitted from the catalog.
    #[test]
    fn list_modules_responds_with_the_current_catalog() {
        let url = format!("inproc://orch-mod-list-{}", unique());
        let (_broker, _ctl) = start_broker(url.clone());
        let (_rn, _rid) = register_runner_with_uri(&url, "jaspTTests", "file:///tmp/jaspTTests/");
        let (_rn2, _rid2) = register_runner(&url, "jaspAnova"); // no base_uri
        let (fe, session_id) = hello_frontend(&url);

        let query = envelope(Message::ListModules);
        let query_id = query.id.clone();
        fe.send(frame_envelope(&query).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();

        let env = deframe(&fe.recv().expect("modules reply")[..]).expect("valid modules");
        assert_eq!(
            env.reply_to.as_deref(),
            Some(query_id.as_str()),
            "reply correlated to the query"
        );
        assert_eq!(env.session_id.as_deref(), Some(session_id.as_str()));
        match env.body {
            Message::Modules(m) => assert_eq!(
                m.modules,
                vec![module_info("jaspTTests", "0.1", "file:///tmp/jaspTTests/")],
                "only the uri-carrying advertisement is discoverable"
            ),
            other => panic!("expected modules, got {other:?}"),
        }
    }

    // 13. Push-on-actual-change: the catalog is pushed exactly when the available-module set (or
    //     the asset source of a module) changes — never on a redundant advertisement. Winner rule:
    //     earliest registration wins; evicting the winner hands the module to the next-oldest
    //     advertiser (a truthful, pushed change).
    #[test]
    fn catalog_pushed_only_on_actual_change() {
        let url = format!("inproc://orch-mod-push-{}", unique());
        let (_broker, _ctl) = start_broker(url.clone());
        let (fe, _sid) = hello_frontend(&url);

        // 1. First advertiser of module M → catalog gains M → push.
        let (rn_a, _ra) = register_runner_with_uri(&url, "M", "file:///a/M/");
        let env = deframe(&fe.recv().expect("push on new module")[..]).expect("valid modules");
        assert!(env.reply_to.is_none(), "a push is unsolicited");
        match &env.body {
            Message::Modules(m) => {
                assert_eq!(m.modules, vec![module_info("M", "0.1", "file:///a/M/")])
            }
            other => panic!("expected modules push, got {other:?}"),
        }

        // 2. A second runner advertises the SAME module+version → the first registration keeps
        //    winning → catalog unchanged → NO push.
        let (rn_b, _rb) = register_runner_with_uri(&url, "M", "file:///b/M/");
        fe.set_opt::<RecvTimeout>(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(fe.recv().is_err(), "no push for a redundant advertisement");

        // 3. Evict the winner (A): B takes over the module → the asset source changed → push.
        drop(rn_a);
        fe.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        let env = deframe(&fe.recv().expect("push on takeover")[..]).expect("valid modules");
        match &env.body {
            Message::Modules(m) => {
                assert_eq!(m.modules, vec![module_info("M", "0.1", "file:///b/M/")])
            }
            other => panic!("expected modules push, got {other:?}"),
        }

        // 4. Evict the last advertiser → catalog empties → push.
        drop(rn_b);
        let env = deframe(&fe.recv().expect("push on empty catalog")[..]).expect("valid modules");
        match &env.body {
            Message::Modules(m) => {
                assert!(m.modules.is_empty(), "catalog drained: {:?}", m.modules)
            }
            other => panic!("expected modules push, got {other:?}"),
        }
    }

    // 14. Libset modules seed the catalog and beat runner advertisements of the same
    //     name+version — runner churn for a libset module causes no push; a genuinely new
    //     module (and its later removal) does.
    #[test]
    fn libset_modules_are_stable_across_runner_churn() {
        let url = format!("inproc://orch-mod-libset-{}", unique());
        let (_broker, _ctl, _req_rx, _event_tx) = start_broker_with_stub_provisioner(
            url.clone(),
            60_000,
            vec![module_info("jaspTTests", "0.1", "file:///lib/jaspTTests/")],
        );
        let (fe, _sid) = hello_frontend(&url);

        // The initial (libset-only) catalog answers a query — nothing was pushed at startup.
        let query = envelope(Message::ListModules);
        fe.send(frame_envelope(&query).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let env = deframe(&fe.recv().expect("modules reply")[..]).expect("valid modules");
        match &env.body {
            Message::Modules(m) => assert_eq!(
                m.modules,
                vec![module_info("jaspTTests", "0.1", "file:///lib/jaspTTests/")]
            ),
            other => panic!("expected modules, got {other:?}"),
        }

        // A runner advertising the SAME module+version (different uri) → libset wins → no push.
        let (_rn, _rid) = register_runner_with_uri(&url, "jaspTTests", "file:///dev/jaspTTests/");
        fe.set_opt::<RecvTimeout>(Some(Duration::from_millis(300)))
            .unwrap();
        assert!(
            fe.recv().is_err(),
            "libset entry is stable across runner churn"
        );

        // A NEW module → push with both entries (canonical name order).
        fe.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        let (rn_dev, _rdev) = register_runner_with_uri(&url, "jaspDev", "file:///dev/jaspDev/");
        let env = deframe(&fe.recv().expect("push on new module")[..]).expect("valid modules");
        match &env.body {
            Message::Modules(m) => assert_eq!(
                m.modules,
                vec![
                    module_info("jaspDev", "0.1", "file:///dev/jaspDev/"),
                    module_info("jaspTTests", "0.1", "file:///lib/jaspTTests/"),
                ]
            ),
            other => panic!("expected modules push, got {other:?}"),
        }

        // Dev runner leaves → the dev module disappears, the libset module remains → push.
        drop(rn_dev);
        let env =
            deframe(&fe.recv().expect("push on dev module removal")[..]).expect("valid modules");
        match &env.body {
            Message::Modules(m) => assert_eq!(
                m.modules,
                vec![module_info("jaspTTests", "0.1", "file:///lib/jaspTTests/")]
            ),
            other => panic!("expected modules push, got {other:?}"),
        }
    }

    // 15. Connect-time push: the FIRST frame a frontend reads on its data channel is the
    //     current catalog, unsolicited and session-scoped — no query needed.
    #[test]
    fn hello_receives_catalog_as_first_channel_frame() {
        let url = format!("inproc://orch-mod-hello-{}", unique());
        let (_broker, _ctl, _req_rx, _event_tx) = start_broker_with_stub_provisioner(
            url.clone(),
            60_000,
            vec![module_info("jaspTTests", "0.1", "file:///lib/jaspTTests/")],
        );
        let (fe, session_id) = hello_frontend_raw(&url);
        let env = deframe(&fe.recv().expect("initial catalog")[..]).expect("valid modules");
        assert!(env.reply_to.is_none(), "connect-time push is unsolicited");
        assert_eq!(env.session_id.as_deref(), Some(session_id.as_str()));
        match env.body {
            Message::Modules(m) => assert_eq!(
                m.modules,
                vec![module_info("jaspTTests", "0.1", "file:///lib/jaspTTests/")]
            ),
            other => panic!("expected modules, got {other:?}"),
        }
    }

    // 16. Reconnect re-receives the catalog — fresh state, not stale: a module that appeared
    //     between the two connections is in the second connect-time push.
    #[test]
    fn reconnect_receives_a_fresh_catalog() {
        let url = format!("inproc://orch-mod-reconnect-{}", unique());
        let (_broker, _ctl) = start_broker(url.clone());
        // First connection: empty catalog (no libset, no runners).
        let (fe1, _sid1) = hello_frontend_raw(&url);
        let env = deframe(&fe1.recv().expect("initial catalog")[..]).expect("valid modules");
        assert!(matches!(&env.body, Message::Modules(m) if m.modules.is_empty()));
        // A runner appears → push to fe1 (and into the catalog).
        let (_rn, _rid) = register_runner_with_uri(&url, "jaspDev", "file:///dev/jaspDev/");
        let env = deframe(&fe1.recv().expect("push on attach")[..]).expect("valid modules");
        assert!(matches!(&env.body, Message::Modules(m) if m.modules.len() == 1));
        // Reconnect: the new channel's first frame carries the CURRENT catalog.
        drop(fe1);
        let (fe2, _sid2) = hello_frontend_raw(&url);
        let env =
            deframe(&fe2.recv().expect("initial catalog on reconnect")[..]).expect("valid modules");
        match env.body {
            Message::Modules(m) => assert_eq!(
                m.modules,
                vec![module_info("jaspDev", "0.1", "file:///dev/jaspDev/")]
            ),
            other => panic!("expected modules, got {other:?}"),
        }
    }
}
