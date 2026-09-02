//! Orchestrator v2 — **the scheduler** (design: `refactor_design/orchestrator-v2-design.md`).
//!
//! Classic is a *pipe* ("forwards work frontend→runner"); v2 is a *scheduler*. The
//! transport, threading fundamental, and lifecycle machinery survive from classic
//! (via the shared `wire` crate — extract, don't fork); the router's policy core is new:
//!
//! * **P1 — single-writer router.** NNG `Aio` callbacks do the bare minimum — recv,
//!   re-arm, forward over mpsc — and one router thread owns every table and queue as
//!   plain `HashMap`s. No locks, never blocks, never touches the filesystem (the
//!   janitor does) or processes (the provisioner does).
//! * **P2 — dumb executors.** Executors register slots, signal readiness, run one
//!   thing at a time, report terminals. No queues, no supersession logic, no
//!   scheduling opinions on the executor side.
//! * **P3 — correctness never depends on hints.** Determinism + content addressing
//!   backstop every optimization; prefetch, credit windows, and pool queues are
//!   optimization only.
//!
//! **Works pull, aborts push.** A `work` is only ever sent as the answer to an
//! executor's ready signal (`register` / `activity` / `result`), bounded by its credit
//! window (`outstanding < slots`). `abort` is the one unsolicited message a busy
//! runner may see. Under pull discipline the classic failure class — works queueing
//! implicitly in the 64-deep PAIR buffer, unrecallable and invisible to supersession —
//! is deleted by construction.
//!
//! **One writer, many caches; a cache is always safe to drop.** Queues live in the
//! router, one shape per semantics: ready-queue (newest `(work_id, revision)` wins),
//! edit chains (per dataset, ordered, never superseded), parked (awaiting a
//! runner/worker; timeout-bounded).
//!
//! Death is op-aware (inherited from classic): analysis fails fast (resubmission is
//! retry), edit-chain heads drain to the respawned worker, opens refuse dangling
//! queued edits, views keep the dataset alive.
//!
//! Run: `cargo run --bin jasp-orchestrator-v2` — answers the same env/URL contract as
//! classic (`JASP_ORCH_URL`); the desktop picks by URL. Classic stays the runnable
//! default until v2's parity is boring.

mod router;

use std::path::PathBuf;
use std::sync::{Arc, mpsc};
use std::time::Duration;

use nng::{Protocol, Socket};
use wire::provisioner::{LaneKind, LaneSpec};
use wire::transport;

use router::RouterMsg;

// ─── Configuration (same env contract as classic, §8) ────────────────────────

struct Config {
    control_url: String,
    orchestrator_dir_root: String,
    hang_timeout_ms: u64,
    activity_min_ms: u64,
    /// Skip workspace reclamation on work_close / frontend drop (debugging). Off in prod.
    keep_workspaces: bool,
    /// §18.4 `max_inline_payload`: the recv ceiling every channel raises to (plus the
    /// envelope margin) — libnng's ~1 MiB default silently discards larger messages.
    max_inline_payload: usize,
    /// Runner provisioner config. `Some` enables on-demand spawning (parked work asks
    /// the provisioner); `None` keeps attach-only behavior (what tests use).
    provisioner: Option<ProvisionerConfig>,
    /// Data-plane workers the provisioner keeps alive (pinned, auto-restarted).
    lane_specs: Vec<LaneSpec>,
    /// View-cache disk budget (bytes) — the LRU sweep's ceiling (AV9: LRU + budget;
    /// the one hold rule of v2 §4 bounds what stays). `JASP_ORCH_VIEW_BUDGET_MB`, default 2 GiB.
    view_budget_bytes: u64,
}

/// Resolve the data-runner binary for the CSV lane: `JASP_ORCH_DATA_RUNNER` override
/// (`off` disables), else the `jasp-data-runner` sibling of this executable.
fn data_lane_binary() -> Option<PathBuf> {
    if let Some(p) = std::env::var("JASP_ORCH_DATA_RUNNER")
        .ok()
        .filter(|s| !s.is_empty())
    {
        return (p != "off").then(|| PathBuf::from(p));
    }
    let exe = std::env::current_exe().ok()?;
    let sibling = exe.with_file_name(format!("jasp-data-runner{}", std::env::consts::EXE_SUFFIX));
    sibling.exists().then_some(sibling)
}

/// Provisioner configuration, built from the environment only when `JASP_ORCH_LIBSET`
/// names at least one libpath.
#[derive(Clone)]
struct ProvisionerConfig {
    libset: Vec<PathBuf>,
    runner_script: PathBuf,
    rscript_bin: String,
    spawn_timeout_ms: u64,
    park_timeout_ms: u64,
}

impl ProvisionerConfig {
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

/// Default orchestrator directory root — identical to classic's (the same on-disk
/// contract; see `Config` path methods below).
fn default_dir_root() -> String {
    dirs::data_local_dir()
        .map(|d| {
            d.join("JASP")
                .join("orchestrator")
                .to_string_lossy()
                .into_owned()
        })
        .unwrap_or_else(|| {
            std::env::temp_dir()
                .join("jasp-orchestrator")
                .to_string_lossy()
                .into_owned()
        })
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
            orchestrator_dir_root: std::env::var("JASP_ORCH_DIR_ROOT")
                .unwrap_or_else(|_| default_dir_root()),
            hang_timeout_ms: parse("JASP_ORCH_HANG_TIMEOUT_MS", 30_000),
            activity_min_ms: parse("JASP_ORCH_ACTIVITY_MIN_MS", 1_000),
            keep_workspaces: std::env::var("JASP_ORCH_KEEP_WORKSPACES")
                .map(|v| v == "1" || v == "true")
                .unwrap_or(false),
            max_inline_payload: std::env::var("JASP_ORCH_MAX_INLINE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(wire::MAX_INLINE_PAYLOAD),
            provisioner: ProvisionerConfig::from_env(&parse),
            lane_specs: data_lane_binary()
                .map(|bin| {
                    vec![LaneSpec {
                        kind: LaneKind::RustData,
                        program: bin,
                        args: Vec::new(),
                    }]
                })
                .unwrap_or_default(),
            view_budget_bytes: parse("JASP_ORCH_VIEW_BUDGET_MB", 2048) * 1024 * 1024,
        }
    }

    fn scheme(&self) -> &str {
        self.control_url.split("://").next().unwrap_or("tcp")
    }

    fn tcp_host(&self) -> &str {
        self.control_url
            .trim_start_matches("tcp://")
            .split(':')
            .next()
            .filter(|h| !h.is_empty())
            .unwrap_or("127.0.0.1")
    }

    // ── Path layout — MUST match classic's exactly (same runner/frontend contract) ──

    /// Per-session workspace: `<root>/<session_id>`; reclaimed on session close + startup GC.
    fn session_workspace(&self, session_id: &str) -> std::path::PathBuf {
        std::path::PathBuf::from(&self.orchestrator_dir_root).join(session_id)
    }

    /// Per-work tree: `<root>/<session>/<work_id>` — the parent of every revision's
    /// `results_<rev>` dir.
    fn work_workspace(&self, session_id: &str, work_id: &str) -> std::path::PathBuf {
        self.session_workspace(session_id).join(work_id)
    }

    /// Per-revision results dir: `…/<work_id>/results_<rev>` — each revision
    /// self-contained; concurrent/out-of-order revisions never share a mutable workspace.
    fn revision_dir(&self, session_id: &str, work_id: &str, revision: u64) -> std::path::PathBuf {
        self.work_workspace(session_id, work_id)
            .join(format!("results_{revision}"))
    }

    /// Per-dataset cache file: `<root>/<session>/datasets/<dataset_id>_<revision>.arrow`.
    /// The router assigns the path (identity); the data worker writes the file (I/O).
    fn dataset_cache_path(&self, session_id: &str, dataset_id: &str, revision: u64) -> PathBuf {
        self.session_workspace(session_id)
            .join("datasets")
            .join(format!("{dataset_id}_{revision}.arrow"))
    }

    /// A view blob's content-addressed path (AV8): `<root>/<session>/views/<hash>.arrow`.
    /// Router-minted identity (the hash IS the name); the data worker writes the bytes.
    /// Dies with the session workspace (teardown wipe) — no per-view lifecycle on close.
    fn view_cache_path(&self, session_id: &str, view_id: &str) -> PathBuf {
        self.session_workspace(session_id)
            .join("views")
            .join(format!("{view_id}.arrow"))
    }
}

// ─── Broker wiring (transport + provisioner, shared via `wire`) ──────────────

/// Start the router thread + janitor (+ provisioner) and return the broker handle
/// (the router's mailbox sender — all real state lives on the router thread).
/// `router::Broker::start_inner` mirrors classic's shape so the stub-provisioner
/// test pattern carries over.
fn start_broker(config: Config) -> Arc<router::Broker> {
    // The libset scan is filesystem I/O — it runs HERE, before the router thread exists.
    let scan = config
        .provisioner
        .as_ref()
        .map(|pc| wire::provisioner::scan_libset(&pc.libset));
    if let Some(scan) = &scan {
        println!(
            "[v2] libset scan: {} module(s) discoverable",
            scan.modules.len()
        );
    }
    let (tx, rx) = mpsc::channel();
    let provisioner = router::spawn_provisioner(&config, scan, tx.clone());
    router::Broker::start_inner(config, tx, rx, provisioner)
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::from_env();
    let control_url = config.control_url.clone();

    let control = Socket::new(Protocol::Rep0)?;
    transport::listen_control(&control, &control_url)?;
    let control = Arc::new(control);

    let broker = start_broker(config);
    router::Broker::arm_control(&broker, Arc::clone(&control))?;
    transport::start_hang_detector(|| RouterMsg::Tick, broker.tx.clone());

    println!("[v2] control endpoint (REP) listening on {control_url}");
    println!("[v2] scheduler ready — pull dispatch, credit windows, supersession");

    // Park forever; Aio recv loops run on NNG's pool threads, routing on the router.
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}
