//! The v2 router — books, queues, machinery (orchestrator-v2-design §4–§6).
//!
//! One thread owns every table as plain `HashMap`s (P1). Executors are dumb (P2):
//! they register slots, signal readiness (register/activity/result), run one thing at
//! a time, and report terminals. Correctness never depends on hints (P3): stale
//! terminals die at the revision check, duplicate execution is safe by determinism.
//!
//! The tending table (§4):
//!
//! | Event | Action |
//! |---|---|
//! | work arrives | supersede queued older revisions; newer revision of a *running* work → abort push; ready-queue; pump |
//! | ready signal (register / activity / result) | update credits/liveness; pop ready-queue head if credits + capability match → dispatch |
//! | edit arrives | append to the dataset's chain (revision-gated); head dispatches to a free worker |
//! | result arrives | return credit (keyed by the executor's inflight record); discard if stale by revision; else forward; edit terminals drain the chain |
//! | pipe close | evict: fail outstanding (op-aware), drain/refuse chains, respawn via provisioner, release path refs |
//! | cache_filled | mark the view Ready/Failed; un-park waiters; offer the builder the next fill |
//! | tick | hang scan (recycle wedged via the provisioner), park timeouts, view LRU sweep (unheld Ready only) |
//!
//! THE ONE HOLD RULE (views phase, §8): a view referenced by a dispatched work is
//! never reclaimed. Everything else Ready is LRU food; the dispatch-time staple
//! guard re-parks a work whose hit became a miss (a cache is always safe to drop).

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, mpsc};

use nng::{Aio, Socket};
use wire::framing::{frame_envelope, frame_parts, now_ms};
use wire::janitor::{JanitorTx, Reclaim, start_janitor};
use wire::provisioner::{LaneKind, ProvReq};
use wire::transport;
use wire::{Capability, DataOp, Envelope, Message, ModuleInfo, Status, WorkKind, WorkPayload};

use crate::Config;

/// Per-process nonce source: concurrently-running brokers (parallel tests) each get a
/// distinct value so their channel addresses never collide.
static BROKER_NONCE: AtomicU64 = AtomicU64::new(0);

// ─── Peer handles (what the Aio callbacks hold) ─────────────────────────────

/// The immutable identity of an executor: its id and data channel. The Aio callback
/// holds one (an `Arc`); the router's `executors` book holds the rest of the state as
/// plain fields — single-writer, no atomics (P1).
pub(crate) struct ExecutorHandle {
    pub runner_id: String,
    pub channel: Arc<Socket>,
}

impl ExecutorHandle {
    /// Non-blocking send (the socket carries `SENDBUF=ORCH_SEND_BUF`). `Err(TryAgain)`
    /// when the peer's buffer is full (hung) or `Err(Closed)` when it is gone; callers
    /// evict on `Err`. Never blocks the router thread.
    fn send(&self, frame: &[u8]) -> Result<(), nng::Error> {
        self.channel.try_send(frame).map_err(|(_m, e)| e)
    }
}

/// A connected frontend and its dedicated data channel.
pub(crate) struct FrontendRuntime {
    pub channel: Arc<Socket>,
    pub session_id: String,
    #[allow(dead_code)]
    pub client_id: Option<String>,
}

impl FrontendRuntime {
    fn send(&self, frame: Vec<u8>) -> Result<(), nng::Error> {
        self.channel.try_send(frame.as_slice()).map_err(|(_m, e)| e)
    }
}

// ─── The books (§4 — router-owned plain maps) ────────────────────────────────

/// The credit ledger + liveness of one executor. All plain fields: only the router
/// thread touches them (P1 — the callbacks forward, they never touch state).
pub(crate) struct Executor {
    pub handle: Arc<ExecutorHandle>,
    pub capabilities: Vec<Capability>,
    pub priority: u32,
    pub seq: u64,
    /// The credit window (`register.slots`, default 1). R runners → 1 (single-threaded);
    /// the Rust data worker → 1; remote pools → N (reserve).
    pub slots: usize,
    /// Credits in use — dispatches-out minus terminals-back, computed, never trusted
    /// from the executor ("I'm free" claims are not a thing).
    pub outstanding: usize,
    /// One record per dispatched work holding a credit. Keyed `(session, work_id)` —
    /// the credit returns when THAT work's terminal arrives (any status, even stale).
    pub inflight: HashMap<(String, String), DispatchRecord>,
    /// Epoch-ms of the last inbound message (liveness for the hang detector).
    pub last_activity: u64,
    /// true = provisioner-spawned (killable via `Recycle`); false = self-attached.
    #[allow(dead_code)]
    pub managed: bool,
}

/// What one dispatched work holds: the revision that was sent, the dataset cache
/// paths it acquired refs on (released when its terminal arrives), and — for data
/// work — the op bookkeeping the terminal needs (op-aware state flips).
pub(crate) struct DispatchRecord {
    pub revision: u64,
    pub kind: WorkKind,
    pub dataset_paths: Vec<PathBuf>,
    pub data: Option<DataWorkEntry>,
    /// **Views-era:** the view ids this dispatch holds (the hold rule of §4: these are
    /// never LRU food while the dispatch lives). Fills are NOT dispatches — they hold
    /// no credit and no view hold; their source ref rides the book entry.
    pub view_ids: Vec<String>,
}

/// The *desired* state of one work id (what the submitter most recently wants).
/// The running dispatch (if any) is tracked by the executor's inflight record —
/// which may be an OLDER revision while a newer one sits in the ready-queue.
#[derive(Clone)]
pub(crate) struct WorkEntry {
    pub frontend: Arc<FrontendRuntime>,
    pub revision: u64,
    pub kind: WorkKind,
    /// The executor currently grinding the latest *dispatchplaced* revision of this
    /// work, if any (`None` while queued/parked — nothing dispatched for it now).
    pub runner: Option<String>,
    /// Supersession push dedup: exactly one `abort` is ever sent per running dispatch
    /// (churn A/6→A/7 must not re-abort). Cleared at the next dispatch.
    pub abort_sent: bool,
}

/// A work unit admitted to the ready-queue — the router-owned, recallable,
/// supersessible queue (analyses, opens, views, and drained edit-chain heads).
pub(crate) struct ReadyWork {
    pub frontend: Arc<FrontendRuntime>,
    pub env: Envelope,
    pub work: wire::Work,
    /// The incoming frame's binary tail (§18.1) — an edit's forward cells or an
    /// `apply_inverse`'s IPC bytes, forwarded verbatim at dispatch.
    pub tail: Vec<u8>,
}

impl ReadyWork {
    fn key(&self) -> (String, String) {
        (self.frontend.session_id.clone(), self.work.work_id.clone())
    }
}

/// What a parked work unit is waiting for: an analysis-module runner or a data lane.
#[derive(Debug, Clone)]
pub(crate) enum Awaiting {
    AnalysisRClassicJaspbase {
        module: String,
        version: String,
    },
    Lane {
        lane: LaneKind,
        op: DataOp,
        format: String,
    },
    /// **Views-era:** the work's stapled specs are not all in the view book yet — an
    /// implied build was ordered (or awaits a free builder). NOT an executor wait:
    /// registering any executor never un-parks it (only `cache_filled` / a re-staple
    /// can). Deliberately carries NO view ids — the missing set is re-derived from the
    /// work's CURRENT stapled spec at every check, so an in-place supersession (new
    /// revision, different spec) can never wait on stale ids.
    ViewBuild,
}

/// A work unit parked while awaiting an executor that does not exist yet (§9.3):
/// no capable executor ever registered, so there is nothing to queue FOR. Converted
/// to a ready-queue entry when a matching executor registers; failed on park-timeout
/// / ProvisionFailed / LaneFailed.
pub(crate) struct ParkedWork {
    pub awaiting: Awaiting,
    pub frontend: Arc<FrontendRuntime>,
    pub env: Envelope,
    pub work: wire::Work,
    pub tail: Vec<u8>,
    pub parked_ms: u64,
}

/// THE EDIT CHAIN — one data edit queued behind its dataset's in-flight edit. Each
/// edit's SOURCE is the previous edit's OUTPUT, so two edits on one dataset can never
/// run concurrently. Extras wait in a per-dataset FIFO; the terminal edit result
/// drains the chain (re-stamped to the realized revision) into the ready-queue.
pub(crate) struct QueuedEdit {
    pub frontend: Arc<FrontendRuntime>,
    pub env: Envelope,
    pub work: wire::Work,
    pub tail: Vec<u8>,
}

/// Lifecycle of a dataset in the index (a failed open removes the entry).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DatasetState {
    Opening,
    Ready,
}

/// One dataset the router owns the identity of: mints ids, tracks `id → current
/// cache file`, routes conversion/edits to workers, reclaims retired files (janitor).
pub(crate) struct DatasetEntry {
    pub id: String,
    pub session: String,
    pub current_path: PathBuf,
    pub revision: u64,
    pub state: DatasetState,
    /// Ingestion settings captured at open; forwarded on every routed data op (the
    /// worker is stateless). Changing them is a re-open, not an edit.
    #[allow(dead_code)]
    pub ingest: wire::IngestParams,
}

/// One dispatched data work's op bookkeeping (classic's `DataWorkEntry`): which
/// dataset it serves, which op, and the revision *at dispatch* (view stamp) or the
/// NEW revision minted at dispatch (edit, stamped post-apply).
pub(crate) struct DataWorkEntry {
    pub dataset_id: String,
    pub op: DataOp,
    pub revision: u64,
}

// ── The view book (v2 §4: hash → state) ─────────────────────────────────────

/// A stapled spec resolved to its fetch ref at dispatch (AV10: local = path, v1).
#[derive(Debug, Clone)]
pub(crate) struct ResolvedView {
    pub dataset_id: String,
    /// `None` = pass-through (the ref IS the base cache file — zero copy).
    pub view_id: Option<String>,
    pub path: PathBuf,
}

/// The staple check's outcome (see `Router::staple`).
#[derive(Debug)]
pub(crate) enum Staple {
    Ready(Vec<ResolvedView>),
    Park,
    Refused,
}

/// One content-addressed view blob the router tracks. Identity only — the router
/// never stats the filesystem (P1); `bytes` is the worker's report from its
/// `cache_filled` confirm.
#[derive(Debug)]
pub(crate) struct ViewEntry {
    /// The stapled spec this view materializes (kept for re-ordering fills).
    pub spec: wire::ViewSpec,
    /// Owning session — the blob lives under its workspace; the book entry dies with it.
    pub session: String,
    /// The base dataset (its CURRENT path is the fill's `source`; staleness is
    /// unreachable-by-hash — an edit bumps the revision ⇒ a new view_id).
    pub dataset_id: String,
    /// The base revision the `view_id` was hashed against.
    pub base_revision: u64,
    /// Router-minted blob path (`<session>/views/<hash>.arrow`).
    pub target: PathBuf,
    pub state: ViewState,
    /// LRU clock — stamped on Ready and on every dispatch-time hit.
    pub touch_ms: u64,
    /// The fill's source path ref (acquired at order; released at confirm/reset).
    pub source: Option<PathBuf>,
}

#[derive(Debug)]
pub(crate) enum ViewState {
    /// No builder has been sent the order yet (no free capable worker, or a reset
    /// after worker death — the re-order story of §8: "waiters were never dispatched,
    /// so worker death just re-orders").
    Wanted,
    /// The fill was ordered to this executor; the confirm (or its death) resolves it.
    Ordered(String),
    /// The blob exists (worker-confirmed); LRU food unless held by a dispatch.
    Ready { bytes: u64 },
    /// A deterministic build failure (unknown column, filter, bad stored type…).
    /// Retrying an unbuildable spec can never succeed — stapling works fail visibly
    /// with this reason forever (terminal memory, not an error loop).
    Failed(String),
}

/// The lane's view-consistency fields captured off a terminal edit result (D6).
pub(crate) struct ViewConsistency {
    pub rows: Option<u64>,
    pub schema: Option<serde_json::Value>,
    pub invalidation: Option<wire::Invalidation>,
}

// ─── The mailbox ─────────────────────────────────────────────────────────────

/// A one-shot reply channel for handshake/query round-trips through the router.
type Reply<T> = mpsc::Sender<T>;

/// Everything the single router thread can be asked to do. Producers are the NNG
/// `Aio` callbacks (via `wire::transport`), the `pipe_notify` disconnect callbacks,
/// the hang-detector thread, the provisioner, and (in tests) state queries.
pub(crate) enum RouterMsg {
    Handshake {
        env: Envelope,
        reply: Reply<Envelope>,
    },
    ExecutorData {
        executor: Arc<ExecutorHandle>,
        env: Envelope,
        binary: Vec<u8>,
    },
    FrontendData {
        frontend: Arc<FrontendRuntime>,
        env: Envelope,
        binary: Vec<u8>,
    },
    EvictExecutor(String),
    DropFrontend(String),
    ProvisionFailed {
        module: String,
        reason: String,
    },
    LaneFailed {
        lane: LaneKind,
        reason: String,
    },
    Tick,
    KeepAlive {
        key: String,
        aio: Aio,
    },
    #[allow(dead_code)]
    QueryRunners(Reply<Vec<String>>),
    #[allow(dead_code)]
    QueryHung(Reply<Vec<String>>),
    #[allow(dead_code)]
    QueryEverRegistered(Reply<bool>),
    #[allow(dead_code)]
    QueryDatasets(Reply<Vec<(String, String, PathBuf, usize)>>),
    /// Test-only: `(executor_id, outstanding, slots)` — the credit ledger.
    #[allow(dead_code)]
    QueryCredits(Reply<Vec<(String, usize, usize)>>),
    /// Test-only: the ready-queue as `(session, work_id, revision)` in FIFO order.
    #[allow(dead_code)]
    QueryReady(Reply<Vec<(String, String, u64)>>),
    /// Test-only: the view book as `(view_id, state, bytes, held)` — state as a string
    /// ("wanted" | "ordered" | "ready" | "failed"), bytes 0 unless ready, held = a
    /// live dispatch references it (the hold rule).
    #[allow(dead_code)]
    QueryViews(Reply<Vec<(String, String, u64, bool)>>),
}

// ─── The router (single-threaded owner of all state) ─────────────────────────

pub(crate) struct Router {
    /// The executors book (the credit ledger lives here).
    pub executors: HashMap<String, Executor>,
    pub frontends: HashMap<String, Arc<FrontendRuntime>>,
    /// Desired state per work id: `(session, work_id) → WorkEntry`.
    pub works: HashMap<(String, String), WorkEntry>,
    /// THE READY-QUEUE (analyses, opens, views, drained edits). FIFO by arrival;
    /// supersession: newest `(work_id, revision)` wins.
    pub ready: VecDeque<ReadyWork>,
    /// Work awaiting an executor that does not exist yet (no capable provider).
    pub parked: HashMap<(String, String), ParkedWork>,
    /// THE EDIT CHAINS: queued edits per dataset.
    pub queued_edits: HashMap<String, VecDeque<QueuedEdit>>,
    pub datasets: HashMap<String, DatasetEntry>,
    /// **Views-era — THE VIEW BOOK** (§4): `view_id → entry`. Books-only lookups at
    /// staple/dispatch time (the router never stats the FS, P1); the worker confirms
    /// existence; the tick's LRU sweep reclaims (janitor executes).
    pub views: HashMap<String, ViewEntry>,
    /// Outstanding dispatch references per cache file (acquire at dispatch, release
    /// at the dispatch's terminal). Decision state only — the janitor deletes.
    pub path_refs: HashMap<PathBuf, usize>,
    /// Strong `Aio` refs keep the per-channel recv loops alive.
    pub aios: HashMap<String, Aio>,
    pub counter: u64,
    pub ever_registered: bool,
    pub config: Config,
    pub nonce: u64,
    pub tx: mpsc::Sender<RouterMsg>,
    pub janitor: JanitorTx,
    pub provisioner: Option<mpsc::Sender<ProvReq>>,
    pub park_timeout_ms: u64,
    pub libset_modules: Vec<ModuleInfo>,
    pub catalog: Vec<ModuleInfo>,
}

impl Router {
    /// The event loop: block on the mailbox, dispatch one message at a time, forever.
    /// All policy runs here, sequentially, on one thread (P1).
    pub(crate) fn run(mut self, rx: mpsc::Receiver<RouterMsg>) {
        while let Ok(msg) = rx.recv() {
            match msg {
                RouterMsg::Handshake { env, reply } => {
                    let ack = match &env.body {
                        Message::Register(reg) => self.handle_register(&env, reg),
                        Message::Hello(h) => self.handle_hello(&env, h),
                        other => {
                            println!("[v2] control ignoring {other:?}");
                            orch_err(
                                Some(&env.id),
                                "unexpected_on_control",
                                "control endpoint carries only the handshake",
                            )
                        }
                    };
                    let _ = reply.send(ack);
                }
                RouterMsg::ExecutorData {
                    executor,
                    env,
                    binary,
                } => self.on_executor_message(&executor, env, binary),
                RouterMsg::FrontendData {
                    frontend,
                    env,
                    binary,
                } => self.on_frontend_message(&frontend, env, binary),
                RouterMsg::EvictExecutor(id) => self.evict_executor(&id),
                RouterMsg::DropFrontend(id) => self.drop_frontend(&id),
                RouterMsg::ProvisionFailed { module, reason } => {
                    self.fail_parked_module(&module, &reason)
                }
                RouterMsg::LaneFailed { lane, reason } => self.fail_parked_lane(lane, &reason),
                RouterMsg::Tick => {
                    self.scan_hung();
                    self.scan_parked();
                    self.scan_views();
                }
                RouterMsg::KeepAlive { key, aio } => {
                    self.aios.insert(key, aio);
                }
                RouterMsg::QueryRunners(reply) => {
                    let ids = self.executors.keys().cloned().collect();
                    let _ = reply.send(ids);
                }
                RouterMsg::QueryHung(reply) => {
                    let _ = reply.send(self.hung_executors());
                }
                RouterMsg::QueryEverRegistered(reply) => {
                    let _ = reply.send(self.ever_registered);
                }
                RouterMsg::QueryDatasets(reply) => {
                    let snap: Vec<(String, String, PathBuf, usize)> = self
                        .datasets
                        .values()
                        .map(|e| {
                            let state = match e.state {
                                DatasetState::Opening => "opening",
                                DatasetState::Ready => "ready",
                            };
                            let refs = self.path_refs.get(&e.current_path).copied().unwrap_or(0);
                            (
                                e.id.clone(),
                                state.to_string(),
                                e.current_path.clone(),
                                refs,
                            )
                        })
                        .collect();
                    let _ = reply.send(snap);
                }
                RouterMsg::QueryCredits(reply) => {
                    let snap: Vec<(String, usize, usize)> = self
                        .executors
                        .values()
                        .map(|e| (e.handle.runner_id.clone(), e.outstanding, e.slots))
                        .collect();
                    let _ = reply.send(snap);
                }
                RouterMsg::QueryReady(reply) => {
                    let snap: Vec<(String, String, u64)> = self
                        .ready
                        .iter()
                        .map(|rw| {
                            (
                                rw.frontend.session_id.clone(),
                                rw.work.work_id.clone(),
                                rw.work.revision,
                            )
                        })
                        .collect();
                    let _ = reply.send(snap);
                }
                RouterMsg::QueryViews(reply) => {
                    let mut held: Vec<String> = Vec::new();
                    for exec in self.executors.values() {
                        for rec in exec.inflight.values() {
                            held.extend(rec.view_ids.iter().cloned());
                        }
                    }
                    let snap: Vec<(String, String, u64, bool)> = self
                        .views
                        .iter()
                        .map(|(id, e)| {
                            let (state, bytes) = match &e.state {
                                ViewState::Wanted => ("wanted".to_string(), 0),
                                ViewState::Ordered(_) => ("ordered".to_string(), 0),
                                ViewState::Ready { bytes } => ("ready".to_string(), *bytes),
                                ViewState::Failed(_) => ("failed".to_string(), 0),
                            };
                            (id.clone(), state, bytes, held.contains(id))
                        })
                        .collect();
                    let _ = reply.send(snap);
                }
            }
        }
    }

    fn next_id(&mut self, prefix: &str) -> (String, u64) {
        self.counter += 1;
        (format!("{}-{}", prefix, self.counter), self.counter)
    }

    // ── Handshake (control endpoint) ─────────────────────────────────────────

    fn handle_register(&mut self, env: &Envelope, reg: &wire::Register) -> Envelope {
        if env.v != 1 {
            return register_ack(false, None, None, "unsupported_version", &env.id, None);
        }
        let (runner_id, seq) = self.next_id("r");
        let (channel, channel_url) = match transport::allocate_channel(
            self.config.scheme(),
            self.config.tcp_host(),
            self.nonce,
            &runner_id,
            self.config.max_inline_payload + wire::RECV_MARGIN,
        ) {
            Ok(x) => x,
            Err(e) => {
                return register_ack(
                    false,
                    None,
                    None,
                    &format!("channel allocation failed: {e}"),
                    &env.id,
                    None,
                );
            }
        };
        let handle = Arc::new(ExecutorHandle {
            runner_id: runner_id.clone(),
            channel,
        });
        if let Err(e) = self.arm_executor_channel(Arc::clone(&handle)) {
            return register_ack(
                false,
                None,
                None,
                &format!("channel arm failed: {e}"),
                &env.id,
                None,
            );
        }
        // Boot = the full credit window (§4: `register` carries slots, default 1).
        let slots = reg.slots.max(1) as usize;
        self.executors.insert(
            runner_id.clone(),
            Executor {
                handle: Arc::clone(&handle),
                capabilities: reg.capabilities.clone(),
                priority: reg.priority,
                seq,
                slots,
                outstanding: 0,
                inflight: HashMap::new(),
                last_activity: now_ms(),
                managed: self.provisioner.is_some(),
            },
        );
        self.ever_registered = true;
        println!(
            "[v2] executor registered {runner_id} (hint {:?}) channel={channel_url} slots={slots} caps={:?}",
            reg.runner_id, reg.capabilities
        );
        // Reconcile the provisioner (its spawn succeeded) and convert parked work the
        // new executor can serve into ready-queue entries, then pump.
        let (modules, lanes) = advertised(&reg.capabilities);
        if let Some(p) = &self.provisioner
            && (!modules.is_empty() || !lanes.is_empty())
        {
            let _ = p.send(ProvReq::RunnerUp {
                modules: modules.clone(),
                lanes,
            });
        }
        self.unpark_matching(&reg.capabilities);
        self.recompute_catalog();
        // A ViewBuild-capable worker may have just arrived — Wanted fills get their
        // builder (fills never un-park on register; they aren't executor waits).
        self.order_fills();
        self.pump();
        register_ack(
            true,
            Some(runner_id),
            Some(channel_url),
            "",
            &env.id,
            Some(self.config.activity_min_ms),
        )
    }

    fn handle_hello(&mut self, env: &Envelope, hello: &wire::Hello) -> Envelope {
        if env.v != 1 {
            return welcome(false, None, None, Some("unsupported_version"), &env.id);
        }
        let (session_id, _seq) = self.next_id("s");
        let (channel, channel_url) = match transport::allocate_channel(
            self.config.scheme(),
            self.config.tcp_host(),
            self.nonce,
            &session_id,
            self.config.max_inline_payload + wire::RECV_MARGIN,
        ) {
            Ok(x) => x,
            Err(e) => {
                return welcome(
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
            return welcome(
                false,
                None,
                None,
                Some(&format!("channel arm failed: {e}")),
                &env.id,
            );
        }
        self.frontends
            .insert(session_id.clone(), Arc::clone(&frontend));
        // A connected frontend always holds the current catalog: the frame sits in the
        // listening PAIR's buffer (pre-connect buffering) and is delivered FIRST.
        let catalog_env = modules_envelope(&self.catalog, None, Some(session_id.clone()));
        if let Err(e) = frontend.send(frame_envelope(&catalog_env)) {
            eprintln!("[v2] initial catalog to frontend {session_id} failed: {e}");
        }
        println!(
            "[v2] frontend registered {session_id} (client_id {:?}) channel={channel_url} catalog={} module(s)",
            hello.client_id,
            self.catalog.len()
        );
        welcome(true, Some(session_id), Some(channel_url), None, &env.id)
    }

    // ── Channel arming (loops live in wire::transport) ───────────────────────

    fn arm_executor_channel(&mut self, handle: Arc<ExecutorHandle>) -> Result<(), nng::Error> {
        let tx_msg = self.tx.clone();
        let tx_disc = self.tx.clone();
        let rid = handle.runner_id.clone();
        let h = Arc::clone(&handle);
        let aio = transport::arm_peer_channel(
            &handle.channel,
            Box::new(move |env, binary| {
                let _ = tx_msg.send(RouterMsg::ExecutorData {
                    executor: Arc::clone(&h),
                    env,
                    binary,
                });
            }),
            Box::new(move || {
                let _ = tx_disc.send(RouterMsg::EvictExecutor(rid.clone()));
            }),
        )?;
        self.aios.insert(handle.runner_id.clone(), aio);
        Ok(())
    }

    fn arm_frontend_channel(&mut self, frontend: Arc<FrontendRuntime>) -> Result<(), nng::Error> {
        let tx_msg = self.tx.clone();
        let tx_disc = self.tx.clone();
        let sid = frontend.session_id.clone();
        let fe = Arc::clone(&frontend);
        let aio = transport::arm_peer_channel(
            &frontend.channel,
            Box::new(move |env, binary| {
                let _ = tx_msg.send(RouterMsg::FrontendData {
                    frontend: Arc::clone(&fe),
                    env,
                    binary,
                });
            }),
            Box::new(move || {
                let _ = tx_disc.send(RouterMsg::DropFrontend(sid.clone()));
            }),
        )?;
        self.aios.insert(frontend.session_id.clone(), aio);
        Ok(())
    }

    // ── Executor messages ────────────────────────────────────────────────────

    fn on_executor_message(&mut self, handle: &ExecutorHandle, env: Envelope, binary: Vec<u8>) {
        // Liveness is stamped router-side (P1: the callbacks forward, never touch state).
        if let Some(exec) = self.executors.get_mut(&handle.runner_id) {
            exec.last_activity = now_ms();
        }
        match &env.body {
            Message::Result(r) => {
                let terminal = matches!(
                    r.status,
                    Status::Complete
                        | Status::FatalError
                        | Status::ValidationError
                        | Status::Aborted
                );
                self.on_result(&handle.runner_id, env, binary, terminal);
            }
            // A ready signal: the runner's loop reached loop-top (it is idle). Never
            // credit accounting — the books compute availability (P3).
            Message::Activity(_) => self.pump(),
            // The implied build's confirm (internal — never forwarded, never credited).
            Message::CacheFilled(c) => self.handle_cache_filled(&handle.runner_id, c),
            other => println!("[v2] executor {} -> {other:?}", handle.runner_id),
        }
    }

    /// A result from an executor. THE CREDIT RULE (§4): every terminal returns one
    /// credit, keyed by the executor's inflight record — never by the result's claim.
    /// Stale terminals (raced completions of an aborted revision) return their credit
    /// and die at the revision check; duplicate terminals find no record and no-op.
    fn on_result(&mut self, runner_id: &str, env: Envelope, binary: Vec<u8>, terminal: bool) {
        let (work_id, revision, status) = match &env.body {
            Message::Result(r) => (r.work_id.clone(), r.revision, r.status),
            _ => return,
        };
        let session_id = env.session_id.clone().unwrap_or_default();
        let key = (session_id.clone(), work_id.clone());

        // 1. The credit returns when the dispatched work's terminal arrives — any
        //    status. A terminal with no inflight record (duplicate / long-dead) no-ops.
        let record = if terminal {
            let Some(exec) = self.executors.get_mut(runner_id) else {
                return; // from an executor we already evicted
            };
            let rec = exec.inflight.remove(&key);
            if rec.is_some() {
                exec.outstanding = exec.outstanding.saturating_sub(1);
            }
            rec
        } else {
            None
        };

        // 2. Op-aware data side effects — keyed on the DISPATCH record, so a stale
        //    terminal still completes its dataset lifecycle (an orphaned Opening entry
        //    would leak otherwise), while the forwarding decision comes later.
        if let Some(rec) = &record
            && let Some(entry) = &rec.data
        {
            self.complete_data_work(&session_id, entry, &status, &env, &work_id);
        }

        // 3. Forward — only the result for the work's DESIRED revision. Stale (the
        //    raced completion of an aborted/superseded revision) dies here.
        let Some(wk) = self.works.get(&key).cloned() else {
            if terminal {
                // Nothing wanted this work anymore (closed, or the frontend left);
                // side effects above still ran. Edit chains still drain (below).
                if let Some(rec) = &record
                    && rec.data.as_ref().is_some_and(|d| d.op == DataOp::Edit)
                {
                    let dataset_id = rec.data.as_ref().unwrap().dataset_id.clone();
                    self.drain_queued_edit(&dataset_id);
                }
                self.pump();
            }
            return;
        };
        if revision < wk.revision {
            println!(
                "[v2] stale result ({session_id},{work_id}) rev {revision} < {} — discarded (raced completion)",
                wk.revision
            );
            // The aborted/superseded dispatch just ended: the queued newer revision is
            // dispatchable now (its predecessor's runner freed a credit).
            if wk.runner.as_deref() == Some(runner_id)
                && let Some(w) = self.works.get_mut(&key)
            {
                w.runner = None;
                w.abort_sent = false;
            }
            if terminal {
                if let Some(rec) = &record
                    && rec.data.as_ref().is_some_and(|d| d.op == DataOp::Edit)
                {
                    let dataset_id = rec.data.as_ref().unwrap().dataset_id.clone();
                    self.drain_queued_edit(&dataset_id);
                }
                self.pump();
            }
            return;
        }

        // Current revision: fill identity & location fields, forward verbatim (with
        // the binary tail), then tear the work's books down.
        let mut env = env;
        if let Message::Result(r) = &mut env.body {
            match &mut r.payload {
                wire::ResultPayload::Data(d) => {
                    if let Some(rec) = &record
                        && let Some(entry) = &rec.data
                    {
                        d.dataset_id = Some(entry.dataset_id.clone());
                        // The dataset revision AT DISPATCH for views; the NEW revision
                        // POST-APPLY for edits (different semantics, documented on both).
                        d.dataset_revision = Some(entry.revision);
                        if entry.op == DataOp::Edit {
                            // D6: view-consistency material rides `data_changed` only.
                            d.rows = None;
                            d.schema = None;
                            d.invalidation = None;
                        }
                    }
                }
                wire::ResultPayload::AnalysisRClassicJaspbase(a) => {
                    let results_dir = self.config.revision_dir(&session_id, &work_id, revision);
                    a.results_dir = Some(results_dir.to_string_lossy().into_owned());
                }
                wire::ResultPayload::Rcode(_) => {}
            }
        }
        let frame = frame_parts(
            &serde_json::to_vec(&env).expect("re-serialize result"),
            &binary,
        );
        if let Err(e) = wk.frontend.send(frame) {
            eprintln!(
                "[v2] result to frontend {} failed ({e}); dropping result",
                wk.frontend.session_id
            );
        }
        if terminal {
            // `data_changed` — after the triggering result, on the same channel (§6).
            if let Some(rec) = &record
                && let Some(entry) = &rec.data
                && entry.op == DataOp::Edit
                && matches!(status, Status::Complete)
            {
                let lane_fields = match &env.body {
                    Message::Result(r) => match &r.payload {
                        wire::ResultPayload::Data(d) => Some(ViewConsistency {
                            rows: d.rows,
                            schema: d.schema.clone(),
                            invalidation: d.invalidation,
                        }),
                        _ => None,
                    },
                    _ => None,
                };
                self.broadcast_data_changed(
                    &session_id,
                    &entry.dataset_id,
                    entry.revision,
                    &work_id,
                    lane_fields.unwrap_or(ViewConsistency {
                        rows: None,
                        schema: None,
                        invalidation: None,
                    }),
                );
            }
            self.works.remove(&key);
            if let Some(rec) = &record
                && rec.data.as_ref().is_some_and(|d| d.op == DataOp::Edit)
            {
                let dataset_id = rec.data.as_ref().unwrap().dataset_id.clone();
                self.drain_queued_edit(&dataset_id);
            }
            // A builder just freed (fills are worked one at a time like any job) —
            // offer it the next Wanted fill.
            self.order_fills();
            self.pump();
        }
    }

    /// The data-op side effects of a terminal (op-aware): an open flips its dataset
    /// (or drops it on failure + refuses queued edits); an edit bumps the revision at
    /// the swap point; a view is a pure read (nothing).
    fn complete_data_work(
        &mut self,
        session_id: &str,
        entry: &DataWorkEntry,
        status: &Status,
        env: &Envelope,
        work_id: &str,
    ) {
        match entry.op {
            DataOp::Open => {
                if matches!(status, Status::Complete) {
                    if let Some(ds) = self.datasets.get_mut(&entry.dataset_id) {
                        ds.state = DatasetState::Ready;
                    }
                    println!("[v2] dataset {} ready — {session_id}", entry.dataset_id);
                } else {
                    eprintln!("[v2] dataset {} open failed (lane error)", entry.dataset_id);
                    if let Some(ds) = self.datasets.remove(&entry.dataset_id) {
                        self.path_refs.remove(&ds.current_path);
                        let _ = self.janitor.send(Reclaim::File(ds.current_path));
                        // The dataset's identity is gone — refuse its queued edits.
                        self.flush_queued_edits(&entry.dataset_id, "the dataset failed to open");
                    }
                }
            }
            DataOp::Edit => {
                let new_path =
                    self.config
                        .dataset_cache_path(session_id, &entry.dataset_id, entry.revision);
                match status {
                    Status::Complete => {
                        if let Some(ds) = self.datasets.get_mut(&entry.dataset_id) {
                            ds.revision = entry.revision;
                            ds.current_path = new_path;
                        }
                        println!(
                            "[v2] dataset {} edited — revision {} ({session_id})",
                            entry.dataset_id, entry.revision
                        );
                    }
                    _ => {
                        eprintln!(
                            "[v2] dataset {} edit failed (lane error); revision unchanged",
                            entry.dataset_id
                        );
                        let _ = self.janitor.send(Reclaim::File(new_path));
                    }
                }
                let _ = (env, work_id); // diagnostics only
            }
            _ => {}
        }
    }

    // ── Frontend messages ────────────────────────────────────────────────────

    fn on_frontend_message(&mut self, fe: &Arc<FrontendRuntime>, env: Envelope, tail: Vec<u8>) {
        enum Action {
            Work(Box<wire::Work>),
            Abort(String),
            WorkClose(String, Option<u64>),
            ListModules,
            Ping,
            Other,
        }
        let action = match &env.body {
            Message::Work(w) => Action::Work(Box::new(w.clone())),
            Message::Abort(a) => Action::Abort(a.work_id.clone()),
            Message::WorkClose(wc) => Action::WorkClose(wc.work_id.clone(), wc.revision),
            Message::ListModules => Action::ListModules,
            Message::Ping => Action::Ping,
            _ => Action::Other,
        };
        match action {
            Action::Work(w) => self.on_work(Arc::clone(fe), env, &w, tail),
            Action::Abort(work_id) => self.route_abort(fe, &work_id),
            Action::WorkClose(work_id, revision) => self.close_work(fe, work_id, revision),
            Action::ListModules => {
                let reply = modules_envelope(
                    &self.catalog,
                    Some(env.id.clone()),
                    Some(fe.session_id.clone()),
                );
                if let Err(e) = fe.send(frame_envelope(&reply)) {
                    eprintln!(
                        "[v2] modules reply to frontend {} failed: {e}",
                        fe.session_id
                    );
                }
            }
            Action::Ping => {
                let pong = Envelope {
                    v: 1,
                    id: format!("v2-pong-{}", env.id),
                    reply_to: Some(env.id.clone()),
                    session_id: Some(fe.session_id.clone()),
                    format: None,
                    ts: None,
                    body: Message::Pong,
                };
                if let Err(e) = fe.send(frame_envelope(&pong)) {
                    eprintln!("[v2] pong to frontend {} failed: {e}", fe.session_id);
                }
            }
            Action::Other => println!("[v2] frontend {} -> {:?}", fe.session_id, env.body),
        }
    }

    /// Work arrives: supersession first (§3), then admission (ready-queue or park),
    /// then pump. This is the whole submission policy in one place.
    fn on_work(&mut self, fe: Arc<FrontendRuntime>, env: Envelope, w: &wire::Work, tail: Vec<u8>) {
        let key = (fe.session_id.clone(), w.work_id.clone());
        let kind = w.payload.kind();

        // ── Supersession / dedupe against what we already hold for this id ──
        if let Some(entry) = self.works.get_mut(&key) {
            if w.revision > entry.revision {
                // Newer revision wins. Update the desired state…
                entry.frontend = Arc::clone(&fe);
                entry.revision = w.revision;
                entry.kind = kind;
                let running = entry.runner.clone();
                let abort_sent = entry.abort_sent;
                // …replace any QUEUED older envelope (ready-queue, then edit chains —
                // a superseded edit's successor rides the same chain slot)…
                let queued_in_ready = self
                    .ready
                    .iter()
                    .position(|rw| rw.key() == key && rw.work.revision < w.revision);
                if let Some(idx) = queued_in_ready {
                    let mut rw = self.ready.remove(idx).unwrap();
                    rw.frontend = Arc::clone(&fe);
                    rw.env = env.clone();
                    rw.work = w.clone();
                    rw.tail = tail.clone();
                    self.ready.insert(idx, rw);
                } else {
                    for chain in self.queued_edits.values_mut() {
                        for qe in chain.iter_mut() {
                            if qe.frontend.session_id == fe.session_id
                                && qe.work.work_id == w.work_id
                            {
                                qe.frontend = Arc::clone(&fe);
                                qe.env = env.clone();
                                qe.work = w.clone();
                                qe.tail = tail.clone();
                            }
                        }
                    }
                }
                if let Some(rid) = running {
                    // …a RUNNING older revision gets the abort push — the ONE unsolicited
                    // message a busy executor may see — exactly once per dispatch.
                    println!(
                        "[v2] work {} superseded: revision bump — aborting the running dispatch on {rid}",
                        w.work_id
                    );
                    if !abort_sent && let Some(exec) = self.executors.get(&rid) {
                        let abort = abort_envelope(&fe.session_id, &w.work_id);
                        if let Err(e) = exec.handle.send(&abort) {
                            eprintln!("[v2] abort push to {rid} failed: {e}");
                        }
                        // Abort is credit-neutral: the credit returns when the aborted
                        // terminal arrives (checkpoint-gated, possibly slow).
                    }
                    if let Some(e2) = self.works.get_mut(&key) {
                        e2.abort_sent = true;
                    }
                }
                // Park it if no capable executor exists; queue it otherwise.
                self.admit(fe, env, w, tail);
                return;
            }
            if w.revision == entry.revision {
                // A retry of what we hold: re-ack (pending or running — never lost,
                // §25.5). Duplicate dispatch is impossible under pull (busy ⇒ queued).
                let marker = running_result(&w.work_id, w.revision, &fe.session_id, kind);
                let _ = fe.send(marker);
                return;
            }
            // Older than what we hold: immediate `superseded` reply (§3) — the
            // frontend drops it by revision client-side (§23), so a terminal-shaped
            // marker is safe.
            let reply =
                superseded_result(&w.work_id, w.revision, &fe.session_id, kind, entry.revision);
            let _ = fe.send(reply);
            return;
        }

        // Also supersede a PARKED older revision of this id (classic semantics).
        if let Some(pw) = self.parked.get_mut(&key)
            && w.revision > pw.work.revision
        {
            println!(
                "[v2] parked work {} superseded: revision {} -> {}",
                w.work_id, pw.work.revision, w.revision
            );
            pw.env = env;
            pw.work = w.clone();
            pw.tail = tail;
            let marker = running_result(&w.work_id, w.revision, &fe.session_id, kind);
            let _ = fe.send(marker);
            // The superseding revision's views may already all be Ready (or the wait
            // was for a spec nobody needs anymore) — re-staple every view waiter now.
            if kind == WorkKind::Data {
                // (data work never staples views; nothing to do)
            } else {
                self.try_unpark_view_waiters();
            }
            return;
        }
        if self.parked.contains_key(&key) {
            // Same/lower revision retry of a parked work — re-ack.
            let marker = running_result(&w.work_id, w.revision, &fe.session_id, kind);
            let _ = fe.send(marker);
            return;
        }

        self.admit(fe, env, w, tail);
    }

    // ── Views: the staple check + the implied build (§8) ─────────────────────

    /// Resolve a work's stapled specs against the view book (books-only — the router
    /// never stats the filesystem, P1). Every outcome:
    ///
    /// * `Ready(resolved)` — all specs hit (Ready blobs + pass-throughs); touch stamped.
    /// * `Park` — ≥ 1 miss: the missing entries are `Wanted` in the book and a fill is
    ///   ordered (`order_fills` — free capable builder only, no credit).
    /// * `Refused` — a visible error was already sent (filter present / dataset not
    ///   ready / the view is poisoned `Failed`); nothing is queued.
    ///
    /// Also the dispatch-time guard: called again at dispatch, a `Park` there means a
    /// hit became a miss after queueing (LRU eviction race) — the caller re-parks. A
    /// cache is always safe to drop; correctness never depends on the book (P3).
    fn staple(&mut self, fe: &FrontendRuntime, w: &wire::Work) -> Staple {
        let Some(specs) = w.views.as_ref() else {
            return Staple::Ready(Vec::new());
        };
        let mut resolved: Vec<ResolvedView> = Vec::with_capacity(specs.len());
        let mut missing = false;
        for spec in specs {
            if spec.filter.is_some() {
                self.send_bad_request(
                    fe,
                    w,
                    "view filters are not supported yet (awaiting the derived-columns design)",
                );
                return Staple::Refused;
            }
            let Some(ds) = self.datasets.get(&spec.dataset_id) else {
                self.send_dataset_not_ready(fe, w, &spec.dataset_id);
                return Staple::Refused;
            };
            if ds.state != DatasetState::Ready {
                self.send_dataset_not_ready(fe, w, &spec.dataset_id);
                return Staple::Refused;
            }
            // Pass-through (§8): all/no-columns + unfiltered IS the base file — zero
            // copy, zero build, never enters the book.
            if spec.is_pass_through() {
                resolved.push(ResolvedView {
                    dataset_id: spec.dataset_id.clone(),
                    view_id: None,
                    path: ds.current_path.clone(),
                });
                continue;
            }
            let id = wire::view_id(spec, ds.revision);
            match self.views.get(&id).map(|e| &e.state) {
                Some(ViewState::Ready { .. }) => {
                    if let Some(e) = self.views.get_mut(&id) {
                        e.touch_ms = now_ms();
                    }
                    let path = self.views.get(&id).map(|e| e.target.clone()).unwrap();
                    resolved.push(ResolvedView {
                        dataset_id: spec.dataset_id.clone(),
                        view_id: Some(id),
                        path,
                    });
                }
                Some(ViewState::Failed(reason)) => {
                    // Poisoned: a deterministic build failure never gets better. Fail
                    // this work NOW with the builder's own reason.
                    let err = no_executor_result(
                        &w.work_id,
                        w.revision,
                        &fe.session_id,
                        &format!("the view could not be built: {reason}"),
                        w.payload.kind(),
                    );
                    let _ = fe.send(err);
                    return Staple::Refused;
                }
                Some(ViewState::Wanted) | Some(ViewState::Ordered(_)) | None => {
                    if !self.views.contains_key(&id) {
                        self.views.insert(
                            id.clone(),
                            ViewEntry {
                                spec: spec.clone(),
                                session: fe.session_id.clone(),
                                dataset_id: spec.dataset_id.clone(),
                                base_revision: ds.revision,
                                target: self.config.view_cache_path(&fe.session_id, &id),
                                state: ViewState::Wanted,
                                touch_ms: now_ms(),
                                source: None,
                            },
                        );
                    }
                    missing = true;
                }
            }
        }
        if missing {
            self.order_fills();
            Staple::Park
        } else {
            Staple::Ready(resolved)
        }
    }

    /// Park a work whose stapled views are not all ready yet — the implied-build wait.
    /// Same shape as `park_work` (running marker + parked entry) but NO provisioner
    /// ask: the fill ordering is the router's own `order_fills`, not a spawn request.
    fn park_awaiting_view(
        &mut self,
        fe: Arc<FrontendRuntime>,
        env: Envelope,
        w: &wire::Work,
        tail: Vec<u8>,
    ) {
        let key = (fe.session_id.clone(), w.work_id.clone());
        if let Err(e) = fe.send(running_result(
            &w.work_id,
            w.revision,
            &fe.session_id,
            w.payload.kind(),
        )) {
            eprintln!(
                "[v2] running-marker to frontend {} failed: {e}",
                fe.session_id
            );
        }
        println!(
            "[v2] parking work_id={} revision={} (session {}) — awaiting view build",
            w.work_id, w.revision, fe.session_id
        );
        let _ = key; // (the parked map is keyed by the work; awaiting carries nothing)
        self.parked.insert(
            key,
            ParkedWork {
                awaiting: Awaiting::ViewBuild,
                frontend: fe,
                env,
                work: w.clone(),
                tail,
                parked_ms: now_ms(),
            },
        );
    }

    /// Order `Wanted` fills to free ViewBuild-capable workers (pull discipline for
    /// fills: no credit, but only a free builder takes one — a busy worker's socket
    /// holds at most aborts and ignorable hints). Called on: staple miss, register,
    /// every terminal (a builder freed), and after executor eviction (re-order).
    /// Fills are idempotent (deterministic rebuild) — a lost or duplicated order is
    /// harmless, which is exactly why this needs no protocol weight (§8).
    fn order_fills(&mut self) {
        let ids: Vec<String> = self
            .views
            .iter()
            .filter(|(_, e)| matches!(e.state, ViewState::Wanted))
            .map(|(id, _)| id.clone())
            .collect();
        if ids.is_empty() {
            return;
        }
        for id in ids {
            let Some(exec_id) = select_view_builder(&self.executors) else {
                break; // no free builder; re-tried on the next terminal/register
            };
            let Some(entry) = self.views.get_mut(&id) else {
                continue;
            };
            let Some(ds) = self.datasets.get(&entry.dataset_id) else {
                // The base dataset is gone (session dropped / open died) — the view is
                // unreachable; drop it (waiters fail at their own re-staple).
                self.views.remove(&id);
                continue;
            };
            let source = ds.current_path.clone();
            *self.path_refs.entry(source.clone()).or_insert(0) += 1;
            entry.source = Some(source.clone());
            entry.state = ViewState::Ordered(exec_id.clone());
            entry.touch_ms = now_ms();
            let fill = wire::CacheFill {
                view_id: id.clone(),
                spec: entry.spec.clone(),
                source: source.to_string_lossy().into_owned(),
                target: entry.target.to_string_lossy().into_owned(),
                base_revision: entry.base_revision,
            };
            let ncols = fill.spec.columns.as_ref().map(|c| c.len()).unwrap_or(0);
            let env = Envelope {
                v: 1,
                id: format!("v2-fill-{id}"),
                reply_to: None,
                session_id: Some(entry.session.clone()),
                format: None,
                ts: Some(now_ms()),
                body: Message::CacheFill(fill),
            };
            println!("[v2] view fill {id} ordered to {exec_id} ({ncols} column(s))");
            let send = self
                .executors
                .get(&exec_id)
                .map(|e| e.handle.send(&frame_envelope(&env)));
            match send {
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    eprintln!("[v2] fill send to {exec_id} failed: {e}; evicting");
                    self.reset_ordered_fills(&exec_id);
                    self.evict_executor(&exec_id);
                }
                None => {
                    // Raced away between select and send — reset and let the next
                    // order_fills pass retry.
                    self.reset_ordered_fills(&exec_id);
                }
            }
        }
    }

    /// Reset fills ordered to a dead/dying executor back to `Wanted` (releasing
    /// their source refs) — the §8 re-order story: waiters were never dispatched, so
    /// worker death only re-orders.
    fn reset_ordered_fills(&mut self, exec_id: &str) {
        let mut release: Vec<PathBuf> = Vec::new();
        for entry in self.views.values_mut() {
            if let ViewState::Ordered(to) = &entry.state
                && to == exec_id
                && let Some(src) = entry.source.take()
            {
                release.push(src);
                entry.state = ViewState::Wanted;
            }
        }
        self.release_paths(&release);
    }

    /// The `cache_filled` confirm: mark Ready (or poison on error), release the fill's
    /// source ref, un-park whatever can now run, and offer the freed builder the next
    /// fill. Unknown ids (LRU-evicted then filled, long-dead) are logged and dropped —
    /// a cache fill has no frontend-visible identity to fail (§8).
    fn handle_cache_filled(&mut self, runner_id: &str, c: &wire::CacheFilled) {
        let Some(entry) = self.views.get_mut(&c.view_id) else {
            println!(
                "[v2] cache_filled for unknown view {} (evicted/gone) — ignored",
                c.view_id
            );
            return;
        };
        // Only the executor we ordered accepts — a late duplicate from anyone else
        // still means the blob exists; take it (idempotent, deterministic bytes).
        let src = entry.source.take();
        match (&c.bytes, &c.error) {
            (Some(bytes), None) => {
                entry.state = ViewState::Ready { bytes: *bytes };
                entry.touch_ms = now_ms();
                println!(
                    "[v2] view {} ready on {runner_id} ({} bytes)",
                    c.view_id, bytes
                );
            }
            _ => {
                let reason = c.error.clone().unwrap_or_else(|| {
                    "the worker confirmed neither bytes nor an error".to_string()
                });
                eprintln!(
                    "[v2] view {} build failed on {runner_id}: {reason}",
                    c.view_id
                );
                entry.state = ViewState::Failed(reason);
            }
        }
        if let Some(src) = src {
            self.release_paths(&[src]);
        }
        self.try_unpark_view_waiters();
        self.order_fills();
        self.pump();
    }

    /// Scan parked implied-build waiters: any whose CURRENT stapled spec now resolves
    /// fully (re-derived — never stale ids) re-enters admission. Called on every
    /// `cache_filled` and after an in-place supersession of a parked work.
    fn try_unpark_view_waiters(&mut self) {
        if self.parked.is_empty() {
            return;
        }
        let keys: Vec<(String, String)> = self
            .parked
            .iter()
            .filter(|(_, pw)| matches!(pw.awaiting, Awaiting::ViewBuild))
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            let Some(pw) = self.parked.remove(&key) else {
                continue;
            };
            match self.staple(&pw.frontend, &pw.work) {
                Staple::Ready(_) => {
                    println!("[v2] un-parking work_id={} (views ready)", pw.work.work_id);
                    self.admit(pw.frontend, pw.env, &pw.work, pw.tail);
                }
                // Still missing (or refused — the error went out with the staple):
                // re-park. A Refused work must NOT linger in the book-less limbo.
                Staple::Park | Staple::Refused => {
                    self.parked.insert(key, pw);
                }
            }
        }
    }

    /// Admission: a capable executor EXISTS (busy or free) → ready-queue + works
    /// entry; none exists → the miss path (park via provisioner, or fail visibly).
    /// **Views-era:** stapled specs are checked FIRST (books-only — hit ⇒ continue,
    /// miss ⇒ park as an implied build + order the fill, §8).
    fn admit(&mut self, fe: Arc<FrontendRuntime>, env: Envelope, w: &wire::Work, tail: Vec<u8>) {
        let key = (fe.session_id.clone(), w.work_id.clone());
        if w.views.is_some() {
            match self.staple(&fe, w) {
                Staple::Ready(_) => { /* all views hit — continue into capability admission */ }
                Staple::Park => {
                    self.park_awaiting_view(fe, env, w, tail);
                    return;
                }
                Staple::Refused => return, // error already sent; nothing queued
            }
        }
        if capable_executor_exists(&self.executors, &w.payload).is_none() {
            self.miss_work(fe, env, w, tail);
            return;
        }
        println!(
            "[v2] admitted work {} rev {} (session {}) → ready-queue",
            w.work_id, w.revision, fe.session_id
        );
        self.works.insert(
            key.clone(),
            WorkEntry {
                frontend: Arc::clone(&fe),
                revision: w.revision,
                kind: w.payload.kind(),
                runner: None,
                abort_sent: false,
            },
        );
        self.ready.push_back(ReadyWork {
            frontend: fe,
            env,
            work: w.clone(),
            tail,
        });
        self.pump();
    }

    /// No live executor can EVER serve this work (no capability match among live
    /// executors): park it behind the provisioner, or fail it visibly.
    fn miss_work(
        &mut self,
        fe: Arc<FrontendRuntime>,
        env: Envelope,
        w: &wire::Work,
        tail: Vec<u8>,
    ) {
        let prov = self.provisioner.clone();

        if let WorkPayload::Data(d) = &w.payload {
            let lane = match d.op {
                DataOp::Open => match d.format.as_str() {
                    "csv" => Some(LaneKind::RustData),
                    _ => None,
                },
                DataOp::View | DataOp::Edit => Some(LaneKind::RustData),
                _ => None,
            };
            let configured =
                lane.is_some_and(|k| self.config.lane_specs.iter().any(|s| s.kind == k));
            if let (Some(prov), Some(lane), true) = (prov, lane, configured) {
                self.park_work(
                    prov,
                    fe,
                    env,
                    w,
                    tail,
                    Awaiting::Lane {
                        lane,
                        op: d.op,
                        format: d.format.clone(),
                    },
                );
            } else {
                let detail = format!(
                    "No data lane is available to perform {:?} (format '{}').",
                    d.op, d.format
                );
                eprintln!(
                    "[v2] no live lane for work_id={} — returning error",
                    w.work_id
                );
                let err = no_executor_result(
                    &w.work_id,
                    w.revision,
                    &fe.session_id,
                    &detail,
                    w.payload.kind(),
                );
                let _ = fe.send(err);
            }
            return;
        }

        let can_provision_modules = self.config.provisioner.is_some();
        if let (Some(prov), Some((module, version)), true) =
            (prov, analysis_module(&w.payload), can_provision_modules)
        {
            self.park_work(
                prov,
                fe,
                env,
                w,
                tail,
                Awaiting::AnalysisRClassicJaspbase {
                    module: module.to_string(),
                    version: version.to_string(),
                },
            );
            return;
        }
        if self.ever_registered {
            let detail = match &w.payload {
                WorkPayload::AnalysisRClassicJaspbase(a) => format!(
                    "No runner is available to run analyses from module '{}'.",
                    a.module
                ),
                WorkPayload::Rcode(_) => "No runner is available to run R code.".to_string(),
                WorkPayload::Data(_) => unreachable!("data work is handled above"),
            };
            eprintln!(
                "[v2] no live executor for work_id={} — returning error",
                w.work_id
            );
            let err = no_executor_result(
                &w.work_id,
                w.revision,
                &fe.session_id,
                &detail,
                w.payload.kind(),
            );
            let _ = fe.send(err);
        } else {
            println!(
                "[v2] work {} dropped (no executor ever registered)",
                w.work_id
            );
        }
    }

    /// Park a work unit awaiting an executor, ack with a running marker, and ask the
    /// provisioner. Keyed `(session, work_id)`: newer revision supersedes (the caller
    /// already did); same/lower is a retry (re-acked by the caller).
    fn park_work(
        &mut self,
        prov: mpsc::Sender<ProvReq>,
        fe: Arc<FrontendRuntime>,
        env: Envelope,
        w: &wire::Work,
        tail: Vec<u8>,
        awaiting: Awaiting,
    ) {
        let key = (fe.session_id.clone(), w.work_id.clone());
        if let Err(e) = fe.send(running_result(
            &w.work_id,
            w.revision,
            &fe.session_id,
            w.payload.kind(),
        )) {
            eprintln!(
                "[v2] running-marker to frontend {} failed: {e}",
                fe.session_id
            );
        }
        let what = match &awaiting {
            Awaiting::AnalysisRClassicJaspbase { module, .. } => format!("module {module}"),
            Awaiting::Lane {
                op: DataOp::View, ..
            } => "data lane for views".to_string(),
            Awaiting::Lane { format, .. } => format!("data lane for '{format}'"),
            // (ViewBuild works park via `park_awaiting_view` — no provisioner ask.)
            Awaiting::ViewBuild => unreachable!("ViewBuild parks carry no provisioner ask"),
        };
        println!(
            "[v2] parking work_id={} revision={} ({what}) awaiting an executor",
            w.work_id, w.revision
        );
        // Ask the provisioner BEFORE inserting (the request is derived from
        // `awaiting`, not read back out of the map).
        match &awaiting {
            Awaiting::AnalysisRClassicJaspbase { module, version } => {
                let _ = prov.send(ProvReq::Provision {
                    module: module.clone(),
                    version: version.clone(),
                });
            }
            Awaiting::Lane { lane, .. } => {
                let _ = prov.send(ProvReq::EnsureLane { lane: *lane });
            }
            // View waits order fills via `order_fills` (their own discipline — no
            // credit, no spawn); this path is never reached for them.
            Awaiting::ViewBuild => {}
        }
        self.parked.insert(
            key,
            ParkedWork {
                awaiting,
                frontend: fe,
                env,
                work: w.clone(),
                tail,
                parked_ms: now_ms(),
            },
        );
    }

    /// Convert parked work a newly-registered executor can serve into ready-queue
    /// entries (the park's whole purpose was "nothing to queue FOR").
    fn unpark_matching(&mut self, caps: &[Capability]) {
        if self.parked.is_empty() {
            return;
        }
        let ready_keys: Vec<(String, String)> = self
            .parked
            .iter()
            .filter(|(_, pw)| {
                match &pw.awaiting {
                Awaiting::AnalysisRClassicJaspbase { module, .. } => caps.iter().any(|c| {
                    matches!(c, Capability::AnalysisRClassicJaspbase { name, .. } if name == module)
                }),
                Awaiting::Lane { op, format, .. } => caps.iter().any(|c| {
                    matches!(c, Capability::Data { op: cop, formats }
                        if cop == op
                            && (*op != DataOp::Open
                                || formats
                                    .as_ref()
                                    .is_some_and(|fs| fs.iter().any(|f| f == format))))
                }),
                // A view wait is not an executor wait — registration never un-parks
                // it (only cache_filled / a supersession-triggered re-staple can).
                Awaiting::ViewBuild => false,
            }
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in ready_keys {
            if let Some(pw) = self.parked.remove(&key) {
                println!(
                    "[v2] un-parking work_id={} (executor registered)",
                    pw.work.work_id
                );
                self.works.insert(
                    key.clone(),
                    WorkEntry {
                        frontend: Arc::clone(&pw.frontend),
                        revision: pw.work.revision,
                        kind: pw.work.payload.kind(),
                        runner: None,
                        abort_sent: false,
                    },
                );
                self.ready.push_back(ReadyWork {
                    frontend: pw.frontend,
                    env: pw.env,
                    work: pw.work,
                    tail: pw.tail,
                });
            }
        }
    }

    /// Fail parked work for a module the provisioner could not provide.
    fn fail_parked_module(&mut self, module: &str, reason: &str) {
        let doomed: Vec<(String, String)> = self
            .parked
            .iter()
            .filter(|(_, pw)| {
                matches!(&pw.awaiting, Awaiting::AnalysisRClassicJaspbase { module: m, .. } if m == module)
            })
            .map(|(k, _)| k.clone())
            .collect();
        for key in doomed {
            if let Some(pw) = self.parked.remove(&key) {
                eprintln!(
                    "[v2] cannot provision module '{module}' for work_id={}: {reason}",
                    pw.work.work_id
                );
                let err = no_executor_result(
                    &pw.work.work_id,
                    pw.work.revision,
                    &key.0,
                    &format!("Could not start a runner for module '{module}': {reason}"),
                    pw.work.payload.kind(),
                );
                let _ = pw.frontend.send(err);
            }
        }
    }

    /// Fail parked data work for a lane the provisioner could not provide.
    fn fail_parked_lane(&mut self, lane: LaneKind, reason: &str) {
        let doomed: Vec<(String, String)> = self
            .parked
            .iter()
            .filter(|(_, pw)| matches!(&pw.awaiting, Awaiting::Lane { lane: l, .. } if *l == lane))
            .map(|(k, _)| k.clone())
            .collect();
        for key in doomed {
            if let Some(pw) = self.parked.remove(&key) {
                eprintln!(
                    "[v2] cannot provision lane {lane:?} for work_id={}: {reason}",
                    pw.work.work_id
                );
                let err = no_executor_result(
                    &pw.work.work_id,
                    pw.work.revision,
                    &key.0,
                    &format!("Could not start the data lane: {reason}"),
                    pw.work.payload.kind(),
                );
                let _ = pw.frontend.send(err);
            }
        }
    }
}

// ─── The pull machinery (§5): dispatch, pump ─────────────────────────────

impl Router {
    /// **PULL DISPATCH**: match ready work (FIFO) to executors with free credits
    /// (`outstanding < slots`) and a matching capability. A `work` message is only
    /// ever sent as the answer to a ready signal — and an idle executor IS ready
    /// (its last terminal/register signaled it). Called on: work arrival, every ready
    /// signal (register / activity / result), edit-chain drains, unparks.
    ///
    /// Loop until no match: one dispatch frees nothing, but a multi-slot executor's
    /// window may absorb several works in one pump.
    pub(crate) fn pump(&mut self) {
        loop {
            let mut matched: Option<(usize, String)> = None;
            'scan: for (idx, rw) in self.ready.iter().enumerate() {
                if let Some(exec) = select_free_executor(&self.executors, &rw.work.payload) {
                    matched = Some((idx, exec));
                    break 'scan;
                }
            }
            let Some((idx, exec_id)) = matched else { break };
            let Some(rw) = self.ready.remove(idx) else {
                break;
            };
            self.dispatch(&exec_id, rw);
        }
    }

    /// Send one admitted work to one executor with a free credit — the ONLY sender of
    /// `work` messages. Port of classic's `dispatch_work` (dataset identity + runner-
    /// facing injections, unchanged contract) with the credit bookkeeping of §4.
    fn dispatch(&mut self, exec_id: &str, rw: ReadyWork) {
        let ReadyWork {
            frontend: fe,
            mut env,
            work: w,
            tail,
        } = rw;
        let Some(handle) = self.executors.get(exec_id).map(|e| Arc::clone(&e.handle)) else {
            return;
        };

        // Stamp session_id so (session_id, work_id) round-trips through the executor.
        env.session_id = Some(fe.session_id.clone());

        // Resolve dataset_ids → current cache paths, ACQUIRING a `path_refs` reference
        // on each (the dispatch holds its refs until ITS terminal). Unknown / not-Ready
        // ids fail statelessly (the frontend is gated on the open's result).
        let mut resolved: Vec<(String, PathBuf)> = Vec::with_capacity(w.dataset_ids.len());
        for id in &w.dataset_ids {
            let path = match self.datasets.get(id) {
                Some(entry) if entry.state == DatasetState::Ready => entry.current_path.clone(),
                _ => {
                    let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                    self.release_paths(&acquired);
                    self.send_dataset_not_ready(&fe, &w, id);
                    return;
                }
            };
            *self.path_refs.entry(path.clone()).or_insert(0) += 1;
            resolved.push((id.clone(), path));
        }

        // Inject runner-facing fields (§19.3): resolved dataset paths + this revision's
        // self-contained workspace (`results_<rev>`). Base revision → `base_results_dir`
        // (copy-on-seed recompute; a completed revision can never be yanked by an abort).
        let output_dir = self
            .config
            .revision_dir(&fe.session_id, &w.work_id, w.revision);
        let mut dataset_paths = serde_json::Map::new();
        for (id, path) in &resolved {
            dataset_paths.insert(id.clone(), serde_json::json!(path.to_string_lossy()));
        }
        let mut value = serde_json::to_value(&env).expect("work envelope to value");
        value["dataset_paths"] = serde_json::Value::Object(dataset_paths);
        value["output_dir"] = serde_json::json!(output_dir.to_string_lossy());
        if let Some(base) = w.base_revision {
            let base_dir = self.config.revision_dir(&fe.session_id, &w.work_id, base);
            value["base_results_dir"] = serde_json::json!(base_dir.to_string_lossy());
        }

        // ── Views-era: resolve the stapled specs against the book (the guard — a hit
        // may have become a miss after queueing, e.g. LRU eviction; a cache is always
        // safe to drop, correctness never depends on it, P3). A miss re-parks the
        // work through the implied-build path; a refusal already told the frontend.
        let mut view_ids: Vec<String> = Vec::new();
        if w.views.is_some() {
            match self.staple(&fe, &w) {
                Staple::Ready(resolved) => {
                    // Stamp fetch refs (AV10) as an INJECTED envelope field — the exact
                    // precedent of `dataset_paths` (identity the router adds at dispatch,
                    // not a typed `Work` field). The stapled `views` specs ride along
                    // untouched; runners that don't speak views ignore `view_refs` and
                    // keep `dataset_paths` (the migration bridge).
                    let stamped: Vec<serde_json::Value> = resolved
                        .iter()
                        .map(|r| {
                            serde_json::json!({
                                "dataset_id": r.dataset_id,
                                "view_id": r.view_id,
                                "path": r.path.to_string_lossy(),
                            })
                        })
                        .collect();
                    view_ids = resolved.iter().filter_map(|r| r.view_id.clone()).collect();
                    value["view_refs"] = serde_json::Value::Array(stamped);
                }
                Staple::Park => {
                    let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                    self.release_paths(&acquired);
                    println!(
                        "[v2] work {} rev {} dispatch-time view miss — re-parking (implied build)",
                        w.work_id, w.revision
                    );
                    self.park_awaiting_view(fe, env, &w, tail.to_vec());
                    return;
                }
                Staple::Refused => {
                    let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                    self.release_paths(&acquired);
                    return;
                }
            }
        }

        // Op-specific identity, one arm per data op. `data_entry` is the op
        // bookkeeping the terminal will need (state flips / revision stamps).
        let mut data_entry: Option<DataWorkEntry> = None;

        // ── data_open: the router mints identity — dataset_id, revision-0 cache path,
        // Opening state — and injects the path (the worker writes the file).
        if let WorkPayload::Data(d) = &w.payload
            && d.op == DataOp::Open
        {
            let (dataset_id, _seq) = self.next_id("ds");
            let cache_path = self
                .config
                .dataset_cache_path(&fe.session_id, &dataset_id, 0);
            println!(
                "[v2] dataset_open {dataset_id} (session {}) format='{}' '{}' -> lane {exec_id}",
                fe.session_id, d.format, d.source
            );
            let mut ingest = d.ingest.clone();
            if ingest.format.is_empty() {
                ingest.format = d.format.clone();
            }
            self.datasets.insert(
                dataset_id.clone(),
                DatasetEntry {
                    id: dataset_id.clone(),
                    session: fe.session_id.clone(),
                    current_path: cache_path.clone(),
                    revision: 0,
                    state: DatasetState::Opening,
                    ingest,
                },
            );
            value["payload"]["cache_path"] = serde_json::json!(cache_path.to_string_lossy());
            if d.ingest.format.is_empty() {
                value["payload"]["ingest"]["format"] = serde_json::json!(d.format);
            }
            data_entry = Some(DataWorkEntry {
                dataset_id,
                op: DataOp::Open,
                revision: 0,
            });
        }

        // ── data_view: a pure read — inject the current cache path; record the revision
        // AT DISPATCH for the stale-chunk stamp (§6.3). No state flips, no minting.
        if let WorkPayload::Data(d) = &w.payload
            && d.op == DataOp::View
        {
            if resolved.len() != 1 {
                let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                self.release_paths(&acquired);
                self.send_bad_request(&fe, &w, "data_view requires exactly one dataset_id");
                return;
            }
            let (dataset_id, cache_path) = &resolved[0];
            let revision = self
                .datasets
                .get(dataset_id)
                .map(|e| e.revision)
                .unwrap_or(0);
            println!(
                "[v2] data_view {dataset_id} (session {}) offset={} -> lane {exec_id}",
                fe.session_id, d.row_offset
            );
            value["payload"]["cache_path"] = serde_json::json!(cache_path.to_string_lossy());
            data_entry = Some(DataWorkEntry {
                dataset_id: dataset_id.clone(),
                op: DataOp::View,
                revision,
            });
        }

        // ── data_edit: D11 (declared base == current, else stale-edit refusal), the
        // CHAIN (one edit in flight per dataset — extras queue), and the NEXT revision's
        // identity (source = pre-edit cache to read, cache_path = new file to write).
        if let WorkPayload::Data(d) = &w.payload
            && d.op == DataOp::Edit
        {
            if resolved.len() != 1 {
                let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                self.release_paths(&acquired);
                self.send_bad_request(&fe, &w, "data_edit requires exactly one dataset_id");
                return;
            }
            if d.edit.is_none() {
                let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                self.release_paths(&acquired);
                self.send_bad_request(
                    &fe,
                    &w,
                    "data_edit requires an edit (the op-family payload)",
                );
                return;
            }
            let (dataset_id, old_path) = &resolved[0];
            let current = self
                .datasets
                .get(dataset_id)
                .map(|e| e.revision)
                .unwrap_or(0);
            if w.revision != current {
                let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                self.release_paths(&acquired);
                self.send_stale_edit(&fe, &w, dataset_id, w.revision, current);
                return;
            }
            // THE EDIT CHAIN: is another edit for this dataset in flight (on ANY executor)?
            let edit_in_flight = self.executors.values().any(|e| {
                e.inflight.values().any(|rec| {
                    rec.data
                        .as_ref()
                        .is_some_and(|d| d.op == DataOp::Edit && &d.dataset_id == dataset_id)
                })
            });
            if edit_in_flight {
                let acquired: Vec<PathBuf> = resolved.iter().map(|(_, p)| p.clone()).collect();
                self.release_paths(&acquired);
                println!(
                    "[v2] data_edit {dataset_id} queued behind the in-flight edit (session {}, work_id={})",
                    fe.session_id, w.work_id
                );
                let _ = fe.send(running_result(
                    &w.work_id,
                    w.revision,
                    &fe.session_id,
                    w.payload.kind(),
                ));
                self.queued_edits
                    .entry(dataset_id.clone())
                    .or_default()
                    .push_back(QueuedEdit {
                        frontend: Arc::clone(&fe),
                        env: env.clone(),
                        work: w.clone(),
                        tail: tail.to_vec(),
                    });
                return;
            }
            let new_revision = current + 1;
            let cache_path =
                self.config
                    .dataset_cache_path(&fe.session_id, dataset_id, new_revision);
            println!(
                "[v2] data_edit {dataset_id} (session {}) base={current} -> rev {new_revision} -> lane {exec_id}",
                fe.session_id
            );
            value["payload"]["source"] = serde_json::json!(old_path.to_string_lossy());
            value["payload"]["cache_path"] = serde_json::json!(cache_path.to_string_lossy());
            data_entry = Some(DataWorkEntry {
                dataset_id: dataset_id.clone(),
                op: DataOp::Edit,
                revision: new_revision,
            });
        }

        // §18.1: the frame's binary tail forwards VERBATIM (edit cells, inverse IPC).
        let frame = frame_parts(
            &serde_json::to_vec(&value).expect("re-serialize work"),
            &tail,
        );

        // Record the dispatch (books BEFORE send: a send error evicts with full
        // op-aware teardown, so the just-recorded work fails visibly, not silently).
        let key = (fe.session_id.clone(), w.work_id.clone());
        let (outstanding, slots) = {
            let Some(exec) = self.executors.get_mut(exec_id) else {
                return;
            };
            exec.outstanding += 1;
            // The hang clock starts AT DISPATCH, not at the executor's last message.
            // An idle executor sends nothing (activity = work cycles), so without this
            // stamp `last_activity` carries the whole pre-dispatch idle window: any
            // work sent to an executor idle > hang_timeout is recycled on the next
            // tick, mid-flight, no matter how fast it actually runs. Stamping here
            // gives the work exactly one hang_timeout to produce a message — the §7
            // intent.
            exec.last_activity = now_ms();
            exec.inflight.insert(
                key.clone(),
                DispatchRecord {
                    revision: w.revision,
                    kind: w.payload.kind(),
                    dataset_paths: resolved.into_iter().map(|(_, p)| p).collect(),
                    data: data_entry,
                    view_ids,
                },
            );
            (exec.outstanding, exec.slots)
        };
        if let Some(entry) = self.works.get_mut(&key) {
            entry.runner = Some(exec_id.to_string());
            entry.abort_sent = false;
        }
        println!(
            "[v2] dispatch work {} rev {} session={} executor={exec_id} ({} of {} credits in use)",
            w.work_id, w.revision, fe.session_id, outstanding, slots
        );
        if let Err(e) = handle.send(&frame) {
            eprintln!("[v2] work send to executor {exec_id} failed ({e}); evicting");
            self.evict_executor(exec_id);
        }
    }

    // ── THE EDIT CHAIN (drain / flush) ──────────────────────────────────────

    /// The dataset's in-flight edit just ended (success AND failure — a failed edit
    /// changed nothing, §3): pop the next queued edit, RE-STAMP its base to the
    /// realized revision, and hand it to the ready-queue (it dispatches when a worker
    /// frees a credit — pull discipline applies to chains too). Whether it still makes
    /// sense against the newer data is the lane's state-relative validation.
    fn drain_queued_edit(&mut self, dataset_id: &str) {
        let Some(mut qe) = self
            .queued_edits
            .get_mut(dataset_id)
            .and_then(|q| q.pop_front())
        else {
            return;
        };
        if self
            .queued_edits
            .get(dataset_id)
            .is_some_and(|q| q.is_empty())
        {
            self.queued_edits.remove(dataset_id);
        }
        let current = self
            .datasets
            .get(dataset_id)
            .map(|e| e.revision)
            .unwrap_or(0);
        qe.work.revision = current;
        if let Message::Work(inner) = &mut qe.env.body {
            inner.revision = current;
        }
        let key = (qe.frontend.session_id.clone(), qe.work.work_id.clone());
        println!(
            "[v2] data_edit {dataset_id} dequeued (base re-stamped to {current}) → ready-queue"
        );
        // A newer revision of this work id may already be queued — supersede, don't dupe.
        if let Some(idx) = self.ready.iter().position(|rw| rw.key() == key) {
            let mut rw = self.ready.remove(idx).unwrap();
            if rw.work.revision < qe.work.revision {
                rw.env = qe.env;
                rw.work = qe.work;
                rw.tail = qe.tail;
                rw.frontend = qe.frontend;
                self.ready.insert(idx, rw);
            } else {
                self.ready.insert(idx, rw);
            }
            return;
        }
        self.works.insert(
            key.clone(),
            WorkEntry {
                frontend: Arc::clone(&qe.frontend),
                revision: qe.work.revision,
                kind: qe.work.payload.kind(),
                runner: None,
                abort_sent: false,
            },
        );
        self.ready.push_back(ReadyWork {
            frontend: qe.frontend,
            env: qe.env,
            work: qe.work,
            tail: qe.tail,
        });
    }

    /// Refuse every queued edit for `dataset_id` (its identity is GONE). Never silent
    /// loss: each frontend gets its edit back as a `stale_edit`-shaped refusal.
    fn flush_queued_edits(&mut self, dataset_id: &str, why: &str) {
        let Some(mut q) = self.queued_edits.remove(dataset_id) else {
            return;
        };
        while let Some(qe) = q.pop_front() {
            eprintln!(
                "[v2] flushing queued data_edit work_id={} ({dataset_id}): {why}",
                qe.work.work_id
            );
            let env = Envelope {
                v: 1,
                id: format!("v2-flush-edit-{}", qe.work.work_id),
                reply_to: None,
                session_id: Some(qe.frontend.session_id.clone()),
                format: None,
                ts: None,
                body: Message::Result(wire::ResultMsg {
                    work_id: qe.work.work_id.clone(),
                    revision: qe.work.revision,
                    status: Status::ValidationError,
                    payload: wire::ResultPayload::Data(wire::DataResult {
                        dataset_id: Some(dataset_id.to_string()),
                        dataset_revision: None,
                        rows: None,
                        schema: None,
                        error_message: Some(format!("edit dropped: {why}; refetch and retry")),
                        row_offset: None,
                        row_count: None,
                        truncated: None,
                        invalidation: None,
                        validation: Some(vec![wire::ValidationIssue {
                            column: None,
                            code: "stale_edit".to_string(),
                            message: format!("the dataset no longer exists: {why}"),
                            count: None,
                            rows: None,
                        }]),
                        inverse: None,
                    }),
                    module_version: None,
                    message: None,
                }),
            };
            if let Err(e) = qe.frontend.send(frame_envelope(&env)) {
                eprintln!(
                    "[v2] flush refusal to frontend {} failed: {e}",
                    qe.frontend.session_id
                );
            }
        }
    }

    // ── Abort / close / teardown ─────────────────────────────────────────────

    /// Abort (frontend → executor): forward the abort to the executor currently
    /// grinding this work's latest dispatched revision, if any. The ONLY unsolicited
    /// message a busy executor may see (besides an ignorable prefetch). No state
    /// change: interrupts are the executor's problem (§7.6); credits return at the
    /// terminal (abort is credit-neutral).
    fn route_abort(&mut self, fe: &FrontendRuntime, work_id: &str) {
        let Some(rid) = self
            .works
            .get(&(fe.session_id.clone(), work_id.to_string()))
            .and_then(|w| w.runner.clone())
        else {
            println!(
                "[v2] abort for unknown/idle work ({},{work_id})",
                fe.session_id
            );
            return;
        };
        let abort = abort_envelope(&fe.session_id, work_id);
        match self.executors.get(&rid) {
            Some(exec) => {
                if let Err(e) = exec.handle.send(&abort) {
                    eprintln!("[v2] abort to executor {rid} failed ({e}); evicting");
                    self.evict_executor(&rid);
                }
            }
            None => println!("[v2] abort target {rid} already gone"),
        }
    }

    /// Work discard (frontend → router), revision-granular like classic. v2 delta:
    /// **credit-neutral** — the abort push does NOT release the credit or the dataset
    /// refs; the late terminal does (keyed by the executor's inflight record). A
    /// wedged executor that never terminals is the hang detector's problem.
    fn close_work(&mut self, fe: &FrontendRuntime, work_id: String, revision: Option<u64>) {
        let key = (fe.session_id.clone(), work_id.clone());
        match revision {
            Some(rev) => {
                let entry = self.works.get(&key).filter(|w| w.revision == rev).cloned();
                if let Some(entry) = entry {
                    self.abort_if_running(&entry, &fe.session_id, &work_id);
                    self.works.remove(&key);
                    // Drop any queued successor of this work id too (it can never run).
                    self.ready.retain(|rw| rw.key() != key);
                }
                // A parked (implied-build / lane / module) instance of exactly this
                // revision is consumed forever as well — never a zombie waiter.
                self.parked
                    .retain(|k, pw| !(k == &key && pw.work.revision == rev));
                let dir = self.config.revision_dir(&fe.session_id, &work_id, rev);
                if !self.config.keep_workspaces {
                    let _ = self.janitor.send(Reclaim::Dir(dir));
                }
                println!(
                    "[v2] work {work_id} rev {rev} (session {}) closed; results_{rev} reclaimed",
                    fe.session_id
                );
            }
            None => {
                if let Some(entry) = self.works.remove(&key) {
                    self.abort_if_running(&entry, &fe.session_id, &work_id);
                }
                self.ready.retain(|rw| rw.key() != key);
                self.parked.remove(&key);
                let dir = self.config.work_workspace(&fe.session_id, &work_id);
                if !self.config.keep_workspaces {
                    let _ = self.janitor.send(Reclaim::Dir(dir));
                }
                println!(
                    "[v2] work {work_id} (session {}) closed; workspace reclaimed",
                    fe.session_id
                );
            }
        }
    }

    /// Push the one abort for a running dispatch (deduped via `abort_sent` — churn
    /// must never re-abort the same dispatch).
    fn abort_if_running(&mut self, entry: &WorkEntry, session_id: &str, work_id: &str) {
        if let Some(rid) = &entry.runner
            && !entry.abort_sent
            && let Some(exec) = self.executors.get(rid)
        {
            let abort = abort_envelope(session_id, work_id);
            if let Err(e) = exec.handle.send(&abort) {
                eprintln!("[v2] abort-on-close to executor {rid} failed ({e})");
            }
        }
        if let Some(w) = self
            .works
            .get_mut(&(session_id.to_string(), work_id.to_string()))
        {
            w.abort_sent = true;
        }
    }

    /// Release one dataset-path reference per entry (acquired at dispatch). A retired
    /// path whose refcount drains AND is no longer any dataset's `current_path` is
    /// enqueued for a single-file janitor delete (enqueue-once: after a map swap no
    /// dispatch resolves a retired path, so its refcount only ever hits zero once).
    fn release_paths(&mut self, paths: &[PathBuf]) {
        for path in paths {
            let Some(entry) = self.path_refs.get_mut(path) else {
                continue;
            };
            *entry = entry.saturating_sub(1);
            if *entry > 0 {
                continue;
            }
            let still_current = self.datasets.values().any(|e| &e.current_path == path);
            if still_current {
                continue;
            }
            self.path_refs.remove(path);
            println!("[v2] retired dataset file {} reclaimed", path.display());
            let _ = self.janitor.send(Reclaim::File(path.clone()));
        }
    }

    // ── Lifecycle / eviction (§9 — op-aware death) ──────────────────────────

    /// An executor died (pipe close / send failure / wedge recycle). Per-family
    /// death semantics (inherited from classic's `evict_runner`):
    ///
    /// | family | on executor death |
    /// |---|---|
    /// | analysis | fail fast to the frontend; resubmission IS retry (new revision) |
    /// | data_edit | chain head dies → nothing applied → the queue drains to the respawn |
    /// | data_open | dataset identity dies — half-written cache reclaimed, queued edits REFUSED |
    /// | data_view | the work fails, the dataset survives untouched |
    ///
    /// v2 delta: a work whose DESIRED revision is newer than the dead dispatch (a
    /// queued successor) is NOT failed — it stays queued and re-dispatches.
    fn evict_executor(&mut self, runner_id: &str) {
        let removed = self.executors.remove(runner_id);
        self.aios.remove(runner_id);
        let Some(exec) = removed else { return };

        // Views-era: fills ordered to this executor reset to Wanted (their source
        // refs released) — waiters were never dispatched, so worker death only
        // re-orders (§8). Re-ordering happens after the teardown below.
        self.reset_ordered_fills(runner_id);

        // Tell the provisioner the runner is gone (modules may be re-provisioned on a
        // later Provision; lanes are pinned and re-spawned immediately).
        let (modules, lanes) = advertised(&exec.capabilities);
        if let Some(p) = &self.provisioner
            && (!modules.is_empty() || !lanes.is_empty())
        {
            let _ = p.send(ProvReq::RunnerGone { modules, lanes });
        }
        self.recompute_catalog();

        // Fail the dead dispatches (owned) — op-aware.
        let mut edits_to_drain: Vec<String> = Vec::new();
        let mut affected: Vec<((String, String), DispatchRecord)> =
            exec.inflight.into_iter().collect::<Vec<_>>();
        affected.sort_by(|a, b| a.0.cmp(&b.0)); // deterministic failure order
        for (key, rec) in &affected {
            if let Some(dw) = &rec.data {
                match dw.op {
                    DataOp::Open => {
                        if let Some(entry) = self.datasets.remove(&dw.dataset_id) {
                            self.path_refs.remove(&entry.current_path);
                            let _ = self.janitor.send(Reclaim::File(entry.current_path));
                            self.flush_queued_edits(
                                &dw.dataset_id,
                                "the data lane stopped unexpectedly",
                            );
                        }
                    }
                    DataOp::Edit => edits_to_drain.push(dw.dataset_id.clone()),
                    _ => {}
                }
            }
            // Fail the frontend ONLY if no newer revision is waiting (queued successor).
            let fail_frontend = self
                .works
                .get(key)
                .map(|w| w.revision == rec.revision)
                .unwrap_or(false);
            if fail_frontend {
                if let Some(wk) = self.works.remove(key) {
                    let err = no_executor_result(
                        &key.1,
                        rec.revision,
                        &key.0,
                        "A fatal crash occurred while running the analysis (the runner stopped unexpectedly).",
                        rec.kind,
                    );
                    if let Err(e) = wk.frontend.send(err) {
                        eprintln!(
                            "[v2] eviction-error to frontend {} failed: {e}",
                            wk.frontend.session_id
                        );
                    }
                }
            } else if let Some(w) = self.works.get_mut(key) {
                // A newer revision is queued — it survives the death of its predecessor.
                w.runner = None;
                w.abort_sent = false;
            }
            self.release_paths(&rec.dataset_paths);
        }
        for dataset_id in edits_to_drain {
            self.drain_queued_edit(&dataset_id);
        }
        println!(
            "[v2] executor {runner_id} evicted ({} outstanding work unit(s) failed)",
            affected.len()
        );

        // Queue rescue: ready work no remaining executor can EVER serve must not sit
        // queued forever — park it (provisioner) or fail it visibly (no-silent-loss).
        self.rescue_orphaned_ready_work();
        // A builder died — its fills reset; another (or the respawn) takes them.
        self.order_fills();
        self.pump();
    }

    /// After an eviction, re-home ready work whose capability lost its last provider.
    fn rescue_orphaned_ready_work(&mut self) {
        if self.ready.is_empty() {
            return;
        }
        let orphaned: Vec<usize> = self
            .ready
            .iter()
            .enumerate()
            .filter(|(_, rw)| capable_executor_exists(&self.executors, &rw.work.payload).is_none())
            .map(|(i, _)| i)
            .collect();
        for idx in orphaned.into_iter().rev() {
            if let Some(rw) = self.ready.remove(idx) {
                self.works.remove(&rw.key());
                eprintln!(
                    "[v2] work {} orphaned (no provider left) — re-routing through the miss path",
                    rw.work.work_id
                );
                self.miss_work(rw.frontend, rw.env, &rw.work, rw.tail);
            }
        }
    }

    /// The frontend is gone: sweep its works (desired + queued + parked + chains),
    /// release refs, drop its datasets' bookkeeping, reclaim the session workspace.
    /// Inflight dispatches keep their credits until their terminals (credit-neutral
    /// by construction — the executor's records drive teardown).
    fn drop_frontend(&mut self, session_id: &str) {
        self.frontends.remove(session_id);
        self.aios.remove(session_id);

        let released: Vec<PathBuf> = self
            .works
            .iter()
            .filter(|((sid, _), _)| sid == session_id)
            .flat_map(|(_, _)| Vec::new())
            .collect::<Vec<_>>();
        let _ = released; // (refs live on dispatch records; nothing to release here)

        self.works.retain(|(sid, _), _| sid != session_id);
        self.ready.retain(|rw| rw.frontend.session_id != session_id);
        self.parked.retain(|k, _| k.0 != session_id);
        for chain in self.queued_edits.values_mut() {
            chain.retain(|qe| qe.frontend.session_id != session_id);
        }
        self.queued_edits.retain(|_, q| !q.is_empty());

        // Drop its dataset bookkeeping; the cache files go with the session workspace.
        let cache_paths: Vec<PathBuf> = self
            .datasets
            .values()
            .filter(|entry| entry.session == session_id)
            .map(|entry| entry.current_path.clone())
            .collect();
        self.datasets.retain(|_, entry| entry.session != session_id);
        for path in cache_paths {
            self.path_refs.remove(&path);
        }
        // Views-era: drop the session's view book entries — the blobs live under the
        // session workspace (wiped above by the janitor); Ordered entries release
        // their source refs. Held-by-inflight is moot: those dispatches' terminals
        // find their works swept already.
        let mut release: Vec<PathBuf> = Vec::new();
        let dead_views: Vec<String> = self
            .views
            .iter()
            .filter(|(_, e)| e.session == session_id)
            .map(|(id, e)| {
                if let Some(src) = &e.source {
                    release.push(src.clone());
                }
                id.clone()
            })
            .collect();
        for id in dead_views {
            self.views.remove(&id);
        }
        self.release_paths(&release);
        if !self.config.keep_workspaces {
            let _ = self
                .janitor
                .send(Reclaim::Dir(self.config.session_workspace(session_id)));
        }
        println!("[v2] frontend {session_id} dropped");
    }

    // ── Tick: hang scan (wedge → recycle) + park timeouts ──────────────────

    /// Executors with outstanding work and no activity within the hang timeout (§7).
    fn hung_executors(&self) -> Vec<String> {
        let now = now_ms();
        self.executors
            .values()
            .filter(|e| {
                e.outstanding > 0
                    && now.saturating_sub(e.last_activity) > self.config.hang_timeout_ms
            })
            .map(|e| e.handle.runner_id.clone())
            .collect()
    }

    /// The hang scan (the `Tick` handler's hung half): a wedged executor freezes its
    /// own credits (self-limiting) — the detector RECYCLES it (§6: no checkpoint ever
    /// comes → the provisioner kills the process; its pipe close evicts with full
    /// op-aware teardown; the respawn registers fresh with full credits and parked
    /// works re-dispatch). Attached runners (nobody spawned them) are evicted from
    /// the books directly — the lingering process is reaped by its own pipe close.
    fn scan_hung(&mut self) {
        for rid in self.hung_executors() {
            let (modules, lanes, managed) = match self.executors.get(&rid) {
                Some(e) => (
                    advertised(&e.capabilities).0,
                    advertised(&e.capabilities).1,
                    e.managed,
                ),
                None => continue,
            };
            eprintln!(
                "[v2] executor {rid} appears hung (outstanding work, no activity past timeout) — recycling"
            );
            match (&self.provisioner, managed) {
                (Some(p), true) => {
                    let _ = p.send(ProvReq::Recycle { modules, lanes });
                }
                _ => {
                    // No provisioner / attached: book-evict now (idempotent against the
                    // later pipe close).
                    self.evict_executor(&rid);
                }
            }
        }
    }

    /// Park-timeout backstop: fail work parked longer than the configured window, so
    /// a wedged provisioner never strands work silently.
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
                let what = match &pw.awaiting {
                    Awaiting::AnalysisRClassicJaspbase { module, .. } => {
                        format!("runner for module '{module}'")
                    }
                    Awaiting::Lane {
                        op: DataOp::View, ..
                    } => "data lane for views".to_string(),
                    Awaiting::Lane { format, .. } => format!("data lane for '{format}'"),
                    Awaiting::ViewBuild => "view build".to_string(),
                };
                eprintln!(
                    "[v2] parked work_id={} timed out after {}ms waiting for a {what}",
                    pw.work.work_id, self.park_timeout_ms
                );
                let err = no_executor_result(
                    &pw.work.work_id,
                    pw.work.revision,
                    &key.0,
                    &format!("Timed out waiting for a {what}."),
                    pw.work.payload.kind(),
                );
                let _ = pw.frontend.send(err);
            }
        }
    }

    /// The LRU sweep over the view book (the tick's cache half) — AV9 as amended:
    /// content-addressed files, LRU + disk budget, **the one hold rule**: a view
    /// referenced by a live dispatch is never reclaimed; everything else Ready is
    /// food. Nothing is evicted while under budget (no churn). Reclamation executes
    /// on the janitor (P1 — the router never touches the filesystem). Failed/Wanted/
    /// Ordered entries carry no bytes and are never swept; wanted-but-unwaited fills
    /// are harmless (a warm blob is LRU food, not a leak).
    fn scan_views(&mut self) {
        if self.views.is_empty() {
            return;
        }
        let held: std::collections::HashSet<&String> = self
            .executors
            .values()
            .flat_map(|e| e.inflight.values())
            .flat_map(|rec| rec.view_ids.iter())
            .collect();
        let total: u64 = self
            .views
            .values()
            .filter_map(|e| match e.state {
                ViewState::Ready { bytes } => Some(bytes),
                _ => None,
            })
            .sum();
        if total <= self.config.view_budget_bytes {
            return;
        }
        // Oldest touch first; held views skip the menu entirely.
        let mut food: Vec<(String, u64, u64)> = self
            .views
            .iter()
            .filter_map(|(id, e)| match e.state {
                ViewState::Ready { bytes } if !held.contains(id) => {
                    Some((id.clone(), e.touch_ms, bytes))
                }
                _ => None,
            })
            .collect();
        food.sort_by_key(|(_, touch, _)| *touch);
        let mut budget = total;
        for (id, touch, bytes) in food {
            if budget <= self.config.view_budget_bytes {
                break;
            }
            let Some(entry) = self.views.remove(&id) else {
                continue;
            };
            budget -= bytes;
            println!(
                "[v2] LRU: view {id} reclaimed ({} bytes, touched {}ms ago; {budget} bytes cached)",
                bytes,
                now_ms().saturating_sub(touch)
            );
            let _ = self.janitor.send(Reclaim::File(entry.target));
        }
    }

    // ── Module discovery (catalog maintenance — as classic) ──────────────────

    /// Recompute the catalog (libset ∪ advertisements), push to frontends ONLY on an
    /// actual change. Dedup by `(name, version)`: libset beats runners; among runners
    /// the earliest registration wins; canonical sort for order-stable equality.
    fn recompute_catalog(&mut self) {
        let mut catalog = self.libset_modules.clone();
        let mut ads: Vec<(u64, ModuleInfo)> = self
            .executors
            .values()
            .flat_map(|e| {
                e.capabilities.iter().filter_map(move |c| match c {
                    Capability::AnalysisRClassicJaspbase {
                        name,
                        version,
                        base_uri: Some(base_uri),
                    } => Some((
                        e.seq,
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
            return;
        }
        self.catalog = catalog;
        self.broadcast_modules();
    }

    fn broadcast_modules(&self) {
        let frame = frame_envelope(&modules_envelope(&self.catalog, None, None));
        for fe in self.frontends.values() {
            if let Err(e) = fe.send(frame.clone()) {
                eprintln!(
                    "[v2] modules push to frontend {} failed: {e}",
                    fe.session_id
                );
            }
        }
        println!(
            "[v2] module catalog changed: {} module(s), pushed to {} frontend(s)",
            self.catalog.len(),
            self.frontends.len()
        );
    }

    /// `data_changed` (data-edit-design §6): the one uniform buffer-invalidation
    /// path per dataset revision bump. Ordering is the frontend's safety net.
    fn broadcast_data_changed(
        &self,
        session_id: &str,
        dataset_id: &str,
        revision: u64,
        work_id: &str,
        view: ViewConsistency,
    ) {
        let Some(fe) = self.frontends.get(session_id) else {
            eprintln!("[v2] data_changed for {dataset_id} dropped: session {session_id} is gone");
            return;
        };
        let invalidation = if let Some(inv) = view.invalidation {
            inv
        } else {
            eprintln!(
                "[v2] data_changed for {dataset_id}: lane omitted the invalidation descriptor; broadcasting all"
            );
            wire::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None,
            }
        };
        let env = Envelope {
            v: 1,
            id: format!("v2-dchg-{dataset_id}-{revision}"),
            reply_to: None,
            session_id: Some(session_id.to_string()),
            format: None,
            ts: Some(now_ms()),
            body: Message::DataChanged(wire::DataChanged {
                dataset_id: dataset_id.to_string(),
                dataset_revision: revision,
                rows: view.rows,
                schema: view.schema,
                invalidation,
                cause: wire::DataChangedCause {
                    kind: wire::ChangeKind::Edit,
                    work_id: Some(work_id.to_string()),
                },
            }),
        };
        if let Err(e) = fe.send(frame_envelope(&env)) {
            eprintln!("[v2] data_changed to frontend {session_id} failed: {e}; dropping push");
        }
    }

    // ── Visible-error senders (no silent loss) ───────────────────────────────

    /// The D11 rejection: an edit's declared base revision is not the dataset's
    /// current revision — synthesized as a `validationError` result with the
    /// `stale_edit` code (the same shape a lane validation failure takes).
    fn send_stale_edit(
        &self,
        fe: &FrontendRuntime,
        w: &wire::Work,
        dataset_id: &str,
        base: u64,
        current: u64,
    ) {
        eprintln!(
            "[v2] work_id={} stale edit on {dataset_id}: base revision {base} != current {current}",
            w.work_id
        );
        let env = Envelope {
            v: 1,
            id: format!("v2-stale-edit-{}", w.work_id),
            reply_to: None,
            session_id: Some(fe.session_id.clone()),
            format: None,
            ts: None,
            body: Message::Result(wire::ResultMsg {
                work_id: w.work_id.clone(),
                revision: w.revision,
                status: Status::ValidationError,
                payload: wire::ResultPayload::Data(wire::DataResult {
                    dataset_id: Some(dataset_id.to_string()),
                    dataset_revision: Some(current),
                    rows: None,
                    schema: None,
                    error_message: Some(format!(
                        "stale edit: base revision {base} is not the current revision {current}; \
                         refetch and retry"
                    )),
                    row_offset: None,
                    row_count: None,
                    truncated: None,
                    invalidation: None,
                    validation: Some(vec![wire::ValidationIssue {
                        column: None,
                        code: "stale_edit".to_string(),
                        message: format!(
                            "base revision {base} is not the current revision {current}"
                        ),
                        count: None,
                        rows: None,
                    }]),
                    inverse: None,
                }),
                module_version: None,
                message: None,
            }),
        };
        if let Err(e) = fe.send(frame_envelope(&env)) {
            eprintln!("[v2] stale_edit to frontend {} failed: {e}", fe.session_id);
        }
    }

    /// A `dataset_not_ready` error for a work referencing an unknown / not-`Ready`
    /// dataset (stateless guard): the frontend retries after `dataset_ready`.
    fn send_dataset_not_ready(&self, fe: &FrontendRuntime, w: &wire::Work, dataset_id: &str) {
        eprintln!(
            "[v2] work_id={} references dataset '{dataset_id}' that is not ready",
            w.work_id
        );
        let env = Envelope {
            v: 1,
            id: format!("v2-ds-notready-{}", w.work_id),
            reply_to: None,
            session_id: Some(fe.session_id.clone()),
            format: None,
            ts: None,
            body: Message::Error(wire::ErrorMsg {
                code: "dataset_not_ready".to_string(),
                message: format!(
                    "dataset '{dataset_id}' is unknown or still opening; retry after dataset_ready"
                ),
                work_id: Some(w.work_id.clone()),
            }),
        };
        if let Err(e) = fe.send(frame_envelope(&env)) {
            eprintln!(
                "[v2] dataset_not_ready to frontend {} failed: {e}",
                fe.session_id
            );
        }
    }

    /// A `bad_request` error for a malformed work — carries the `work_id` so the
    /// frontend surfaces it on that work's slot; no silent loss.
    fn send_bad_request(&self, fe: &FrontendRuntime, w: &wire::Work, message: &str) {
        eprintln!("[v2] work_id={} is malformed: {message}", w.work_id);
        let env = Envelope {
            v: 1,
            id: format!("v2-badreq-{}", w.work_id),
            reply_to: None,
            session_id: Some(fe.session_id.clone()),
            format: None,
            ts: None,
            body: Message::Error(wire::ErrorMsg {
                code: "bad_request".to_string(),
                message: message.to_string(),
                work_id: Some(w.work_id.clone()),
            }),
        };
        if let Err(e) = fe.send(frame_envelope(&env)) {
            eprintln!("[v2] bad_request to frontend {} failed: {e}", fe.session_id);
        }
    }
}

// ─── Free functions: selection + synthetic results ───────────────────────

/// Can this executor serve this payload? (capability match — classic's rule)
fn can_serve(caps: &[Capability], payload: &WorkPayload) -> bool {
    match payload {
        WorkPayload::AnalysisRClassicJaspbase(a) => caps.iter().any(
            |c| matches!(c, Capability::AnalysisRClassicJaspbase { name, .. } if name == &a.module),
        ),
        WorkPayload::Rcode(_) => caps.iter().any(|c| matches!(c, Capability::Rcode {})),
        // Data work: same op; `data_open` additionally matches the source format
        // (§5.4: the routing table is a capability advertisement).
        WorkPayload::Data(d) => caps.iter().any(|c| match c {
            Capability::Data { op, formats } => {
                op == &d.op
                    && (d.op != DataOp::Open
                        || formats
                            .as_ref()
                            .is_some_and(|fs| fs.iter().any(|f| f == &d.format)))
            }
            _ => false,
        }),
    }
}

/// The pull discipline's selection: a capable executor WITH A FREE CREDIT
/// (`outstanding < slots` — §4 dispatch condition). Highest `priority` wins; ties
/// break to the most recent registration (`seq`).
fn select_free_executor(
    executors: &HashMap<String, Executor>,
    payload: &WorkPayload,
) -> Option<String> {
    executors
        .values()
        .filter(|e| e.outstanding < e.slots && can_serve(&e.capabilities, payload))
        .max_by_key(|e| (e.priority, e.seq))
        .map(|e| e.handle.runner_id.clone())
}

/// Does ANY live executor advertise this capability (busy or free)? For the
/// admit-vs-park decision: if one exists, the work queues (pull); if none does, the
/// work parks behind the provisioner.
fn capable_executor_exists(
    executors: &HashMap<String, Executor>,
    payload: &WorkPayload,
) -> Option<String> {
    executors
        .values()
        .filter(|e| can_serve(&e.capabilities, payload))
        .max_by_key(|e| (e.priority, e.seq))
        .map(|e| e.handle.runner_id.clone())
}

/// A FREE executor advertising the analysis-view build (`data_view_build`) — the
/// fill-order target (pull discipline for fills: no credit, free builders only —
/// a busy worker's socket holds at most aborts and ignorable hints).
fn select_view_builder(executors: &HashMap<String, Executor>) -> Option<String> {
    executors
        .values()
        .filter(|e| {
            e.outstanding < e.slots
                && e.capabilities.iter().any(|c| {
                    matches!(
                        c,
                        Capability::Data {
                            op: DataOp::ViewBuild,
                            ..
                        }
                    )
                })
        })
        .max_by_key(|e| (e.priority, e.seq))
        .map(|e| e.handle.runner_id.clone())
}

/// The `(modules, lanes)` a capability list advertises (provisioner reconciliation).
fn advertised(caps: &[Capability]) -> (Vec<String>, Vec<LaneKind>) {
    let modules: Vec<String> = caps
        .iter()
        .filter_map(|c| match c {
            Capability::AnalysisRClassicJaspbase { name, .. } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let lanes = if caps.iter().any(|c| {
        matches!(
            c,
            Capability::Data {
                op: DataOp::Open,
                ..
            }
        )
    }) {
        vec![LaneKind::RustData]
    } else {
        Vec::new()
    };
    (modules, lanes)
}

/// The `(module, version)` an analysis work targets, or `None`.
fn analysis_module(payload: &WorkPayload) -> Option<(&str, &str)> {
    match payload {
        WorkPayload::AnalysisRClassicJaspbase(a) => {
            Some((a.module.as_str(), a.module_version.as_str()))
        }
        WorkPayload::Rcode(_) => None,
        WorkPayload::Data(_) => None,
    }
}

/// An `abort` envelope — the one unsolicited message a busy executor may see.
fn abort_envelope(session_id: &str, work_id: &str) -> Vec<u8> {
    frame_envelope(&Envelope {
        v: 1,
        id: format!("v2-abort-{work_id}"),
        reply_to: None,
        session_id: Some(session_id.to_string()),
        format: None,
        ts: None,
        body: Message::Abort(wire::Abort {
            work_id: work_id.to_string(),
        }),
    })
}

/// A `running` marker (the §25.5 pending-ack): the work is accepted — queued,
/// parked, or running — never lost. Payload shape follows the work's kind (§19.2).
fn running_result(work_id: &str, revision: u64, session_id: &str, kind: WorkKind) -> Vec<u8> {
    let env = Envelope {
        v: 1,
        id: format!("v2-running-{work_id}"),
        reply_to: None,
        session_id: Some(session_id.to_string()),
        format: None,
        ts: None,
        body: Message::Result(wire::ResultMsg {
            work_id: work_id.to_string(),
            revision,
            status: Status::Running,
            payload: empty_payload(kind, None),
            module_version: None,
            message: None,
        }),
    };
    frame_envelope(&env)
}

/// A synthetic `superseded` reply (§3): the submitter's revision is older than what
/// the router holds; the frontend drops it client-side by revision (§23).
fn superseded_result(
    work_id: &str,
    revision: u64,
    session_id: &str,
    kind: WorkKind,
    superseded_by: u64,
) -> Vec<u8> {
    let env = Envelope {
        v: 1,
        id: format!("v2-superseded-{work_id}"),
        reply_to: None,
        session_id: Some(session_id.to_string()),
        format: None,
        ts: None,
        body: Message::Result(wire::ResultMsg {
            work_id: work_id.to_string(),
            revision,
            status: Status::Complete,
            payload: empty_payload(kind, None),
            module_version: None,
            message: Some(format!(
                "superseded: revision {revision} was replaced by revision {superseded_by}"
            )),
        }),
    };
    frame_envelope(&env)
}

/// A `fatalError` for a work no live executor can serve (dead/evicted executors
/// surface as visible errors instead of vanishing, §25.5).
fn no_executor_result(
    work_id: &str,
    revision: u64,
    session_id: &str,
    detail: &str,
    kind: WorkKind,
) -> Vec<u8> {
    let env = Envelope {
        v: 1,
        id: format!("v2-noexecutor-{work_id}"),
        reply_to: None,
        session_id: Some(session_id.to_string()),
        format: None,
        ts: None,
        body: Message::Result(wire::ResultMsg {
            work_id: work_id.to_string(),
            revision,
            status: Status::FatalError,
            payload: empty_payload(kind, Some(detail)),
            module_version: None,
            message: Some(detail.to_string()),
        }),
    };
    frame_envelope(&env)
}

/// A kind-correct result payload for synthetic results (§19.2: every result carries
/// its kind).
fn empty_payload(kind: WorkKind, error: Option<&str>) -> wire::ResultPayload {
    match kind {
        WorkKind::AnalysisRClassicJaspbase => {
            wire::ResultPayload::AnalysisRClassicJaspbase(wire::AnalysisResult {
                results: match error {
                    Some(detail) => serde_json::json!({
                        "error": true,
                        "errorMessage": detail,
                        "title": "Analysis could not be completed",
                    }),
                    None => serde_json::json!({ "title": "pending" }),
                },
                results_dir: None,
                images: None,
            })
        }
        WorkKind::Data => wire::ResultPayload::Data(wire::DataResult {
            dataset_id: None,
            dataset_revision: None,
            rows: None,
            schema: None,
            error_message: error.map(|s| s.to_string()),
            row_offset: None,
            row_count: None,
            truncated: None,
            invalidation: None,
            validation: None,
            inverse: None,
        }),
        WorkKind::Rcode => wire::ResultPayload::Rcode(match error {
            Some(detail) => serde_json::json!({ "error": true, "errorMessage": detail }),
            None => serde_json::json!({ "title": "pending" }),
        }),
    }
}

/// A `modules` envelope carrying `catalog`.
fn modules_envelope(
    catalog: &[ModuleInfo],
    reply_to: Option<String>,
    session_id: Option<String>,
) -> Envelope {
    Envelope {
        v: 1,
        id: format!("v2-modules-{}", now_ms()),
        reply_to,
        session_id,
        format: None,
        ts: None,
        body: Message::Modules(wire::ModulesMsg {
            modules: catalog.to_vec(),
        }),
    }
}

/// `register_ack` envelope.
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
            "v2-ack-{}",
            runner_id.clone().unwrap_or_else(|| "err".into())
        ),
        reply_to: Some(reply_to.to_string()),
        session_id: None,
        format: None,
        ts: None,
        body: Message::RegisterAck(wire::RegisterAck {
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

/// `welcome` envelope (the assigned session rides on the envelope's `session_id`).
fn welcome(
    ok: bool,
    session_id: Option<String>,
    channel_url: Option<String>,
    error: Option<&str>,
    reply_to: &str,
) -> Envelope {
    Envelope {
        v: 1,
        id: format!(
            "v2-welcome-{}",
            session_id.clone().unwrap_or_else(|| "err".into())
        ),
        reply_to: Some(reply_to.to_string()),
        session_id,
        format: None,
        ts: None,
        body: Message::Welcome(wire::Welcome {
            ok,
            channel_url,
            error: error.map(|s| s.to_string()),
        }),
    }
}

/// A generic error envelope (control-endpoint failures).
fn orch_err(reply_to: Option<&str>, code: &str, message: &str) -> Envelope {
    Envelope {
        v: 1,
        id: "v2-err".into(),
        reply_to: reply_to.map(|s| s.to_string()),
        session_id: None,
        format: None,
        ts: None,
        body: Message::Error(wire::ErrorMsg {
            code: code.into(),
            message: message.into(),
            work_id: None,
        }),
    }
}

// ─── Broker handle ────────────────────────────────────────────────────────

/// Start the provisioner thread from an already-completed libset scan and/or the
/// configured data lanes (production path; tests inject their own channels). The
/// provisioner reports outcomes directly onto the router's mailbox via the injected
/// `on_event` closure — a non-blocking mpsc send from its own thread.
pub(crate) fn spawn_provisioner(
    config: &Config,
    scan: Option<wire::provisioner::LibsetScan>,
    router_tx: mpsc::Sender<RouterMsg>,
) -> Option<(mpsc::Sender<ProvReq>, u64, Vec<ModuleInfo>)> {
    use std::time::Duration;
    use wire::provisioner::{AnalysisRunnerSpec, ProvEvent, RunnerProvisioner};

    if config.provisioner.is_none() && config.lane_specs.is_empty() {
        return None; // nothing to provision
    }
    let modules = scan.as_ref().map(|s| s.modules.clone()).unwrap_or_default();
    let (req_tx, req_rx) = mpsc::channel::<ProvReq>();
    let on_event = Box::new(move |ev: ProvEvent| match ev {
        ProvEvent::ProvisionFailed { module, reason } => {
            let _ = router_tx.send(RouterMsg::ProvisionFailed { module, reason });
        }
        ProvEvent::LaneFailed { lane, reason } => {
            let _ = router_tx.send(RouterMsg::LaneFailed { lane, reason });
        }
    });
    let (analysis, spawn_timeout_ms, park_timeout_ms) = match config.provisioner.as_ref() {
        Some(pc) => (
            Some(AnalysisRunnerSpec {
                runner_script: pc.runner_script.clone(),
                rscript_bin: pc.rscript_bin.clone(),
            }),
            pc.spawn_timeout_ms,
            pc.park_timeout_ms,
        ),
        None => (None, 120_000, 180_000),
    };
    let provisioner = RunnerProvisioner::new(
        scan.unwrap_or(wire::provisioner::LibsetScan {
            module_lib: HashMap::new(),
            lib_modules: HashMap::new(),
            modules: Vec::new(),
        }),
        analysis,
        config.lane_specs.clone(),
        config.control_url.clone(),
        Duration::from_millis(spawn_timeout_ms),
        req_rx,
        on_event,
    );
    std::thread::Builder::new()
        .name("v2-provisioner".into())
        .spawn(move || provisioner.run())
        .expect("spawn provisioner thread");
    Some((req_tx, park_timeout_ms, modules))
}

/// The shareable handle: the router's mailbox sender. All real state lives on the
/// router thread.
pub(crate) struct Broker {
    pub tx: mpsc::Sender<RouterMsg>,
}

impl Broker {
    /// Construct the router + broker with an already-built provisioner handle
    /// (production builds it via `crate::spawn_provisioner`; tests inject stubs).
    /// Mirrors classic's `start_inner` so the stub-provisioner test pattern carries.
    pub(crate) fn start_inner(
        config: Config,
        tx: mpsc::Sender<RouterMsg>,
        rx: mpsc::Receiver<RouterMsg>,
        provisioner: Option<(mpsc::Sender<ProvReq>, u64, Vec<ModuleInfo>)>,
    ) -> Arc<Broker> {
        // Janitor, then startup GC: at boot no sessions are live, so everything under
        // the dir root is stale from a previous run (best-effort).
        let janitor = start_janitor();
        let _ = janitor.send(Reclaim::Dir(std::path::PathBuf::from(
            &config.orchestrator_dir_root,
        )));
        let (prov_tx, park_timeout_ms, libset_modules) = match provisioner {
            Some((req_tx, timeout, modules)) => (Some(req_tx), timeout, modules),
            None => (None, 0, Vec::new()),
        };
        // Pre-spawn the pinned lanes so the first open pays no spawn latency.
        if let Some(p) = &prov_tx {
            for spec in &config.lane_specs {
                let _ = p.send(ProvReq::EnsureLane { lane: spec.kind });
            }
        }
        let router = Router {
            executors: HashMap::new(),
            frontends: HashMap::new(),
            works: HashMap::new(),
            ready: VecDeque::new(),
            parked: HashMap::new(),
            queued_edits: HashMap::new(),
            datasets: HashMap::new(),
            views: HashMap::new(),
            path_refs: HashMap::new(),
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
            catalog: libset_modules,
        };
        std::thread::Builder::new()
            .name("v2-router".into())
            .spawn(move || router.run(rx))
            .expect("spawn router thread");
        Arc::new(Broker { tx })
    }

    /// Arm the control REP socket (shared loop in `wire::transport`); the mailbox
    /// round-trip runs here, the strong `Aio` ref lives in the router's keep-alive map.
    pub(crate) fn arm_control(self: &Arc<Broker>, control: Arc<Socket>) -> Result<(), nng::Error> {
        let broker = Arc::clone(self);
        let aio = transport::arm_control_rep(
            &control,
            Box::new(move |env| {
                let reply_to = env.id.clone();
                let (rtx, rrx) = mpsc::channel();
                let _ = broker.tx.send(RouterMsg::Handshake { env, reply: rtx });
                rrx.recv().unwrap_or_else(|_| {
                    orch_err(Some(&reply_to), "router_unavailable", "router stopped")
                })
            }),
        )?;
        let _ = self.tx.send(RouterMsg::KeepAlive {
            key: "control".to_string(),
            aio,
        });
        Ok(())
    }

    // ── Test-only snapshots (round-trips through the mailbox) ──

    #[cfg(test)]
    pub(crate) fn runners_snapshot(&self) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryRunners(tx));
        rx.recv().unwrap_or_default()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn hung_snapshot(&self) -> Vec<String> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryHung(tx));
        rx.recv().unwrap_or_default()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn is_ever_registered(&self) -> bool {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryEverRegistered(tx));
        rx.recv().unwrap_or(false)
    }

    #[cfg(test)]
    pub(crate) fn datasets_snapshot(&self) -> Vec<(String, String, PathBuf, usize)> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryDatasets(tx));
        rx.recv().unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn credits_snapshot(&self) -> Vec<(String, usize, usize)> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryCredits(tx));
        rx.recv().unwrap_or_default()
    }

    #[cfg(test)]
    #[allow(dead_code)]
    pub(crate) fn ready_snapshot(&self) -> Vec<(String, String, u64)> {
        let (tx, rx) = mpsc::channel();
        let _ = self.tx.send(RouterMsg::QueryReady(tx));
        rx.recv().unwrap_or_default()
    }
}

// ─── tests (the must-pass list, orchestrator-v2-design §12 step 4) ─────────

#[cfg(test)]
mod tests {
    use super::*;
    use nng::options::{Options, RecvBufferSize, RecvTimeout, SendBufferSize};
    use nng::{Protocol, Socket};
    use wire::framing::deframe;
    use wire::{AnalysisWork, Hello, Register, ResultMsg, ResultPayload, Settings, Work};

    use serde_json::{Value, json};
    use std::sync::atomic::AtomicU64 as SeqAtomic;
    use std::time::{Duration, Instant};

    static TEST_SEQ: SeqAtomic = SeqAtomic::new(0);
    fn unique() -> u64 {
        TEST_SEQ.fetch_add(1, Ordering::SeqCst)
    }

    fn test_config(control_url: String) -> Config {
        Config {
            control_url,
            orchestrator_dir_root: format!("/tmp/jasp-v2-test-{}-{}", std::process::id(), unique()),
            hang_timeout_ms: 30_000,
            activity_min_ms: 1_000,
            keep_workspaces: false,
            max_inline_payload: wire::MAX_INLINE_PAYLOAD,
            provisioner: None,
            lane_specs: Vec::new(),
            // Generous by default; the LRU test shrinks it per-broker.
            view_budget_bytes: 64 * 1024 * 1024,
        }
    }

    /// Start a broker on a fresh inproc control endpoint.
    fn start_broker(control_url: String) -> Arc<Broker> {
        let config = test_config(control_url.clone());
        let (tx, rx) = mpsc::channel();
        let broker = Broker::start_inner(config, tx, rx, None);
        // Arm the control socket like `main` does.
        let control = Socket::new(Protocol::Rep0).unwrap();
        wire::transport::listen_control(&control, &control_url).unwrap();
        let control = Arc::new(control);
        Broker::arm_control(&broker, control).unwrap();
        broker
    }

    fn envelope(body: Message) -> Envelope {
        Envelope {
            v: 1,
            id: format!("t-{}", unique()),
            reply_to: None,
            session_id: None,
            format: None,
            ts: None,
            body,
        }
    }

    fn analysis_work(id: &str, module: &str, revision: u64) -> Envelope {
        envelope(Message::Work(Work {
            work_id: id.to_string(),
            revision,
            base_revision: None,
            dataset_ids: Vec::new(),
            views: None,
            payload: WorkPayload::AnalysisRClassicJaspbase(AnalysisWork {
                module: module.to_string(),
                module_version: "0.1".to_string(),
                analysis: "A".to_string(),
                options: Value::Null,
                preload_data: None,
                settings: Settings {
                    ppi: 96,
                    num_decimals: 3,
                },
            }),
        }))
    }

    /// REQ-register a mock executor (default slots=1 unless overridden); returns its
    /// PAIR channel + assigned id.
    fn register_executor(control_url: &str, module: &str) -> (Socket, String) {
        register_executor_full(control_url, module, 0, 1, None)
    }

    fn register_executor_full(
        control_url: &str,
        module: &str,
        priority: u32,
        slots: u32,
        caps_extra: Option<Vec<Capability>>,
    ) -> (Socket, String) {
        let req = Socket::new(Protocol::Req0).unwrap();
        req.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        req.dial(control_url).unwrap();
        let mut caps = vec![Capability::AnalysisRClassicJaspbase {
            name: module.to_string(),
            version: "0.1".to_string(),
            base_uri: None,
        }];
        if let Some(extra) = caps_extra {
            caps.extend(extra);
        }
        let reg = envelope(Message::Register(Register {
            runner_id: None,
            capabilities: caps,
            priority,
            slots,
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
        ch.set_opt::<RecvTimeout>(Some(Duration::from_millis(250)))
            .unwrap();
        ch.set_opt::<SendBufferSize>(64).unwrap();
        ch.set_opt::<RecvBufferSize>(64).unwrap();
        ch.dial(&channel_url).unwrap();
        (ch, runner_id)
    }

    /// REQ-hello a mock frontend; consumes the connect-time catalog frame.
    fn hello_frontend(control_url: &str) -> (Socket, String) {
        let req = Socket::new(Protocol::Req0).unwrap();
        req.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        req.dial(control_url).unwrap();
        let hello = envelope(Message::Hello(Hello {
            client_id: Some("test-client".to_string()),
            client_version: Some("0.0.0".to_string()),
        }));
        req.send(frame_envelope(&hello).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let welcome_raw = req.recv().expect("welcome");
        let welcome = deframe(&welcome_raw[..]).expect("valid welcome");
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
        ch.set_opt::<RecvTimeout>(Some(Duration::from_millis(250)))
            .unwrap();
        ch.set_opt::<SendBufferSize>(64).unwrap();
        ch.set_opt::<RecvBufferSize>(64).unwrap();
        ch.dial(&channel_url).unwrap();
        // Consume the connect-time catalog push.
        let first =
            deframe(&ch.recv().expect("initial catalog frame")[..]).expect("valid envelope");
        assert!(matches!(first.body, Message::Modules(_)));
        (ch, session_id)
    }

    /// Send a `result` on an executor channel with an explicit status.
    fn send_result_with(
        ch: &Socket,
        session_id: &str,
        work_id: &str,
        revision: u64,
        status: Status,
    ) {
        let result = Envelope {
            v: 1,
            id: format!("rn-{work_id}"),
            reply_to: None,
            session_id: Some(session_id.to_string()),
            format: None,
            ts: None,
            body: Message::Result(ResultMsg {
                work_id: work_id.to_string(),
                revision,
                status,
                payload: ResultPayload::AnalysisRClassicJaspbase(wire::AnalysisResult {
                    results: json!({"title": "ok"}),
                    results_dir: None,
                    images: None,
                }),
                module_version: None,
                message: None,
            }),
        };
        ch.send(frame_envelope(&result).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
    }

    fn send_result(ch: &Socket, session_id: &str, work_id: &str, revision: u64) {
        send_result_with(ch, session_id, work_id, revision, Status::Complete);
    }

    #[allow(dead_code)]
    fn send_activity(ch: &Socket) {
        let activity = envelope(Message::Activity(wire::Activity { work_id: None }));
        ch.send(frame_envelope(&activity).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
    }

    /// Recv a work envelope on an executor channel (with a small wait).
    fn recv_work(ch: &Socket) -> wire::Work {
        let raw = ch.recv().expect("a work frame");
        match deframe(&raw[..]).expect("valid envelope").body {
            Message::Work(w) => w,
            other => panic!("expected work, got {other:?}"),
        }
    }

    /// Recv a result envelope on a frontend channel, skipping non-terminal
    /// `running` markers (park acks, re-acks) — the terminal is what tests assert on.
    fn recv_terminal(fe: &Socket) -> ResultMsg {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            assert!(Instant::now() < deadline, "no terminal arrived");
            let raw = fe.recv().expect("a result frame");
            if let Some(env) = deframe(&raw[..])
                && let Message::Result(r) = env.body
                && r.status != Status::Running
            {
                return r;
            }
        }
    }

    /// Recv a result envelope on a frontend channel.
    fn recv_result(fe: &Socket) -> ResultMsg {
        let raw = fe.recv().expect("a result frame");
        match deframe(&raw[..]).expect("valid envelope").body {
            Message::Result(r) => r,
            other => panic!("expected result, got {other:?}"),
        }
    }

    /// Assert a recv TIMES OUT (nothing arrived within the window).
    fn assert_silent(sock: &Socket, what: &str) {
        match sock.recv() {
            Ok(_) => panic!("{what}: expected silence, got a frame"),
            Err(nng::Error::TimedOut) => {}
            Err(e) => panic!("{what}: unexpected error {e:?}"),
        }
    }

    fn drain_running_markers(fe: &Socket) {
        // Consume any `running` markers queued on the frontend channel.
        loop {
            match fe.recv() {
                Ok(raw) => {
                    if let Some(env) = deframe(&raw[..])
                        && let Message::Result(r) = &env.body
                        && r.status != Status::Running
                    {
                        panic!("drain_running_markers saw a terminal: {:?}", r.status);
                    }
                }
                Err(nng::Error::TimedOut) => return,
                Err(e) => panic!("drain error {e:?}"),
            }
        }
    }

    // ── Parity: the skeleton still pipes ──

    #[test]
    fn registration_yields_a_usable_channel() {
        let url = format!("inproc://v2-reg-{}", unique());
        let broker = start_broker(url.clone());
        let (_ch, runner_id) = register_executor(&url, "jaspTTests");
        assert!(runner_id.starts_with("r-"));
        assert!(broker.runners_snapshot().contains(&runner_id));
    }

    #[test]
    fn frontend_round_trips_with_correlation() {
        let url = format!("inproc://v2-rt-{}", unique());
        let _broker = start_broker(url.clone());
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);

        fe.send(frame_envelope(&analysis_work("w-1", "jaspTTests", 0)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let w = recv_work(&runner);
        assert_eq!(w.work_id, "w-1");

        send_result(&runner, &session, "w-1", 0);
        let r = recv_result(&fe);
        assert_eq!(r.work_id, "w-1");
        assert_eq!(r.status, Status::Complete);
    }

    // ── THE MUST-PASS LIST (§12 step 4) ──

    /// **Pull discipline**: a busy executor (slots=1) receives NOTHING until its
    /// in-flight work's terminal returns the credit. The classic failure class —
    /// works queueing in the PAIR buffer, unrecallable — is deleted by construction.
    #[test]
    fn never_dispatch_to_a_busy_executor() {
        let url = format!("inproc://v2-pull-{}", unique());
        let broker = start_broker(url.clone());
        let (runner, rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);

        fe.send(frame_envelope(&analysis_work("w-1", "jaspTTests", 0)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let w = recv_work(&runner);
        assert_eq!(w.work_id, "w-1");

        // A second work for the same module: queued, NOT pushed.
        fe.send(frame_envelope(&analysis_work("w-2", "jaspTTests", 0)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_silent(&runner, "busy executor must not receive w-2");
        let credits = broker.credits_snapshot();
        assert_eq!(credits, vec![(rid.clone(), 1, 1)], "1 of 1 credit in use");

        // The terminal returns the credit → w-2 dispatches (result = ready signal).
        send_result(&runner, &session, "w-1", 0);
        let w2 = recv_work(&runner);
        assert_eq!(w2.work_id, "w-2");
        let credits = broker.credits_snapshot();
        assert_eq!(credits, vec![(rid, 1, 1)], "credit re-acquired by w-2");

        // Both results reach the frontend (the first one before w-2, naturally).
        let r1 = recv_result(&fe);
        assert_eq!(r1.work_id, "w-1");
    }

    /// **Credit windows**: slots=N admits N concurrent works; N+1 waits.
    #[test]
    fn slots_window_dispatches_two_then_waits() {
        let url = format!("inproc://v2-slots-{}", unique());
        let broker = start_broker(url.clone());
        let (runner, rid) = register_executor_full(&url, "jaspTTests", 0, 2, None);
        let (fe, session) = hello_frontend(&url);

        for id in ["a", "b", "c"] {
            fe.send(frame_envelope(&analysis_work(id, "jaspTTests", 0)).as_slice())
                .map_err(|(_, e)| e)
                .unwrap();
        }
        // Two dispatch immediately (the window); the third waits.
        let wa = recv_work(&runner);
        let wb = recv_work(&runner);
        assert_eq!(wa.work_id, "a");
        assert_eq!(wb.work_id, "b");
        assert_silent(&runner, "window is full — c must wait");
        assert_eq!(broker.credits_snapshot(), vec![(rid.clone(), 2, 2)]);

        // A terminal frees ONE credit → exactly one more dispatch.
        let _ = wb;
        send_result(&runner, &session, "a", 0);
        let wc = recv_work(&runner);
        assert_eq!(wc.work_id, "c");
        assert_silent(&runner, "window refilled — nothing else to send");
        assert_eq!(broker.credits_snapshot(), vec![(rid, 2, 2)]);
    }

    /// **Churn = one abort** (§6): A/5 running; A/6 arrives (abort pushed); A/7
    /// arrives during the abort window (ZERO messages to the runner — the ready-queue
    /// churns A/6→A/7); the raced A/5 terminal is discarded by revision; only A/7
    /// ever dispatches.
    #[test]
    fn churn_sends_exactly_one_abort() {
        let url = format!("inproc://v2-churn-{}", unique());
        let _broker = start_broker(url.clone());
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);

        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 5)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let w = recv_work(&runner);
        assert_eq!(w.revision, 5);

        // A/6: supersede-running → the one abort push.
        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 6)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let raw = runner.recv().expect("the abort");
        match deframe(&raw[..]).unwrap().body {
            Message::Abort(a) => assert_eq!(a.work_id, "A"),
            other => panic!("expected abort, got {other:?}"),
        }

        // A/7 during the abort window: ready-queue churn, ZERO runner messages.
        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 7)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_silent(&runner, "churn must not send a second abort");

        // Raced completion: the runner finished A/5 normally before the abort landed.
        send_result(&runner, &session, "A", 5);
        // The stale result is discarded (frontend never sees rev 5)…
        // …and the credit return dispatches A/7 (A/6 was superseded in the queue).
        let w7 = recv_work(&runner);
        assert_eq!(w7.work_id, "A");
        assert_eq!(w7.revision, 7, "only the newest revision dispatches");

        // The frontend never saw the stale rev-5 result.
        drain_running_markers(&fe);
        assert_silent(&fe, "stale rev-5 result must not be forwarded");

        // Finish the story: A/7 completes, the frontend sees exactly that.
        send_result(&runner, &session, "A", 7);
        let r = recv_result(&fe);
        assert_eq!(r.work_id, "A");
        assert_eq!(r.revision, 7);
    }

    /// **Abort is credit-neutral** (§4): while the aborted work's terminal is pending,
    /// the credit stays held — a different work cannot jump the queue onto the same
    /// executor. The credit returns when the aborted terminal arrives.
    #[test]
    fn abort_is_credit_neutral() {
        let url = format!("inproc://v2-credneut-{}", unique());
        let broker = start_broker(url.clone());
        let (runner, rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);

        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 1)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_work(&runner).work_id, "A");

        // Supersede A/1 → abort push; the executor has NOT terminaled yet.
        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 2)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let _ = runner.recv().expect("the abort");

        // B (a different work id) must NOT dispatch — A/1 still holds the credit.
        fe.send(frame_envelope(&analysis_work("B", "jaspTTests", 1)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_silent(&runner, "credit held by the pending abort");
        assert_eq!(broker.credits_snapshot(), vec![(rid.clone(), 1, 1)]);

        // The aborted terminal returns the credit → A/2, then B (FIFO).
        send_result_with(&runner, &session, "A", 1, Status::Aborted);
        let w = recv_work(&runner);
        assert_eq!((w.work_id.as_str(), w.revision), ("A", 2));
        // A/2 holds the window again; B waits.
        assert_silent(&runner, "B still waits behind A/2");
        send_result(&runner, &session, "A", 2);
        assert_eq!(recv_work(&runner).work_id, "B");
    }

    /// **Raced completion is a no-op** (§6): the abort sits in the executor's buffer
    /// until loop-top; the runner treats it as a no-op (runner-side contract). The
    /// router side: the stale terminal is discarded, side effects intact, life goes on.
    #[test]
    fn raced_completion_is_a_no_op_router_side() {
        let url = format!("inproc://v2-raced-{}", unique());
        let _broker = start_broker(url.clone());
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);

        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 3)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_work(&runner).revision, 3);

        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 4)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let _abort = runner.recv().expect("the abort");

        // The runner ignores the abort (finished anyway) and completes A/3.
        send_result(&runner, &session, "A", 3);
        let w4 = recv_work(&runner);
        assert_eq!(w4.revision, 4);
        // The frontend must not have seen the stale rev-3 result.
        drain_running_markers(&fe);
        assert_silent(&fe, "stale rev-3 discarded");
        // And A/4 completes cleanly.
        send_result(&runner, &session, "A", 4);
        let r = recv_result(&fe);
        assert_eq!(r.revision, 4);
    }

    /// **Queued supersession**: while A/5 runs, A/6 and then A/7 queue; A/7 replaces
    /// A/6 IN PLACE. After A/5's terminal, A/7 (never A/6) dispatches.
    #[test]
    fn queued_supersession_replaces_in_place() {
        let url = format!("inproc://v2-qsup-{}", unique());
        let _broker = start_broker(url.clone());
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);

        fe.send(frame_envelope(&analysis_work("A", "jaspTTests", 5)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_work(&runner).revision, 5);

        for rev in [6, 7] {
            fe.send(frame_envelope(&analysis_work("A", "jaspTTests", rev)).as_slice())
                .map_err(|(_, e)| e)
                .unwrap();
        }
        // One abort only (for A/5); A/6→A/7 churn is silent.
        let _ = runner.recv().expect("the single abort");
        assert_silent(&runner, "no second abort, no premature dispatch");

        send_result(&runner, &session, "A", 5);
        let w = recv_work(&runner);
        assert_eq!(w.revision, 7, "A/7 replaced A/6 in the queue");
    }

    /// **Wedge → recycle** (§6/§9): a wedged executor (outstanding work, silent) is
    /// detected by the tick; with no provisioner it is book-evicted — its dispatched
    /// work fails fast to the frontend (resubmission IS retry), and a fresh executor
    /// serves the resubmission.
    #[test]
    fn wedge_is_recycled_and_work_fail_fasts() {
        let url = format!("inproc://v2-wedge-{}", unique());
        let config = Config {
            hang_timeout_ms: 150, // sub-tick: the detector ticks every 1s
            ..test_config(url.clone())
        };
        let (tx, rx) = mpsc::channel();
        let broker = Broker::start_inner(config, tx.clone(), rx, None);
        let control = Socket::new(Protocol::Rep0).unwrap();
        wire::transport::listen_control(&control, &url).unwrap();
        Broker::arm_control(&broker, Arc::new(control)).unwrap();
        transport::start_hang_detector(|| RouterMsg::Tick, tx);

        let (runner, rid) = register_executor(&url, "jaspTTests");
        let (fe, _session) = hello_frontend(&url);

        fe.send(frame_envelope(&analysis_work("W", "jaspTTests", 1)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_work(&runner).work_id, "W");
        // The mock goes silent (no activity, no terminal) — wedged.

        // Within ~1.5s the tick recycles it: book-eviction (the hung state is
        // transient — scan_hung evicts on the same tick that flags it) + fail-fast
        // to the frontend.
        let deadline = Instant::now() + Duration::from_millis(2_500);
        loop {
            if !broker.runners_snapshot().contains(&rid) {
                break;
            }
            assert!(Instant::now() < deadline, "wedged executor never evicted");
            std::thread::sleep(Duration::from_millis(50));
        }
        let r = recv_result(&fe);
        assert_eq!(r.work_id, "W");
        assert!(matches!(r.status, Status::FatalError), "fail fast");
        assert!(!broker.runners_snapshot().contains(&rid), "evicted");

        // A fresh executor serves the resubmission (new revision — retry).
        let (runner2, _rid2) = register_executor(&url, "jaspTTests");
        let (fe2, session2) = hello_frontend(&url);
        fe2.send(frame_envelope(&analysis_work("W", "jaspTTests", 2)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_work(&runner2).revision, 2);
        send_result(&runner2, &session2, "W", 2);
        let r2 = recv_result(&fe2);
        assert_eq!(r2.status, Status::Complete);
    }

    /// **Regression (idle executor + dispatch):** the wedge clock starts AT DISPATCH.
    /// An executor that sat idle longer than the hang timeout (idle executors send no
    /// activity — their last_activity ages) must NOT be recycled by the tick right
    /// after it is finally dispatched work: the stamp at dispatch gives the work its
    /// full hang_timeout to produce a message. Without the dispatch stamp this test
    /// fails — the first tick after dispatch reads the whole idle window as staleness
    /// and evicts mid-flight (the GUI's first open died exactly this way).
    #[test]
    fn work_dispatched_to_idle_executor_is_not_recycled() {
        let url = format!("inproc://v2-idle-{}", unique());
        let config = Config {
            hang_timeout_ms: 1_500, // > one tick, < the idle window below
            ..test_config(url.clone())
        };
        let (tx, rx) = mpsc::channel();
        let broker = Broker::start_inner(config, tx.clone(), rx, None);
        let control = Socket::new(Protocol::Rep0).unwrap();
        wire::transport::listen_control(&control, &url).unwrap();
        Broker::arm_control(&broker, Arc::new(control)).unwrap();
        transport::start_hang_detector(|| RouterMsg::Tick, tx);

        let (runner, rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);

        // Idle past the hang timeout: no messages, last_activity ages beyond it.
        std::thread::sleep(Duration::from_millis(2_100));

        fe.send(frame_envelope(&analysis_work("W", "jaspTTests", 1)).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_work(&runner).work_id, "W");

        // Hold the result until a tick has passed post-dispatch: the tick must find
        // the work in flight and the clock re-stamped — NOT evict (pre-fix staleness
        // here ≈ 3s > 1.5s → evicted; post-fix ≈ 0.9s → alive).
        std::thread::sleep(Duration::from_millis(1_200));
        assert!(
            broker.runners_snapshot().contains(&rid),
            "idle executor evicted on dispatch"
        );

        // The work completes within its fresh hang_timeout window.
        send_result(&runner, &session, "W", 1);
        let r = recv_result(&fe);
        assert_eq!(
            r.status,
            Status::Complete,
            "work on a long-idle executor must complete"
        );
        assert!(
            broker.runners_snapshot().contains(&rid),
            "survives its completed work"
        );
    }

    /// **Op-aware evictions** (§9): an open dies with its lane (dataset identity
    /// dropped, queued edits refused); a view's dataset survives the lane's death.
    #[test]
    fn op_aware_eviction_open_dies_view_survives() {
        let url = format!("inproc://v2-opaware-{}", unique());
        let broker = start_broker(url.clone());
        let (lane, _lid) = register_executor_full(
            &url,
            "unused-module",
            0,
            1,
            Some(vec![
                Capability::Data {
                    op: DataOp::Open,
                    formats: Some(vec!["csv".into()]),
                },
                Capability::Data {
                    op: DataOp::View,
                    formats: None,
                },
                Capability::Data {
                    op: DataOp::Edit,
                    formats: None,
                },
            ]),
        );
        let (fe, _session) = hello_frontend(&url);

        // Open a dataset.
        let open = envelope(Message::Work(Work {
            work_id: "open-1".into(),
            revision: 0,
            base_revision: None,
            dataset_ids: vec![],
            views: None,
            payload: WorkPayload::Data(wire::DataWork {
                op: DataOp::Open,
                source: "/nonexistent/e2e.csv".into(),
                cache_path: String::new(),
                format: "csv".into(),
                ingest: Default::default(),
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: wire::VIEW_CHUNK_BYTES,
                render: None,
                edit: None,
            }),
        }));
        fe.send(frame_envelope(&open).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let w = recv_work(&lane);
        assert_eq!(w.work_id, "open-1");
        // The router minted the dataset (Opening) — visible in the books.
        let snap = broker.datasets_snapshot();
        assert_eq!(snap.len(), 1, "the open minted a dataset entry");
        assert_eq!(snap[0].1, "opening");

        // The lane dies mid-open (pipe close → evict): the dataset identity dies
        // with it (fail-obsolete), the open fails fast to the frontend.
        drop(lane);
        let deadline = Instant::now() + Duration::from_millis(2_000);
        loop {
            if broker.datasets_snapshot().is_empty() {
                break;
            }
            assert!(Instant::now() < deadline, "dataset never dropped");
            std::thread::sleep(Duration::from_millis(25));
        }
        let r = recv_result(&fe);
        assert_eq!(r.work_id, "open-1");
        assert!(matches!(r.status, Status::FatalError));
    }

    /// A helper: open a (fake) CSV to Ready on the given lane and return the dataset id.
    /// Extract the orchestrator-stamped `dataset_id` from a data result payload.
    fn expect_data_id(r: ResultMsg) -> String {
        match r.payload {
            ResultPayload::Data(d) => d.dataset_id.expect("dataset_id stamped by the router"),
            other => panic!("expected data payload, got {other:?}"),
        }
    }

    fn open_dataset(_url: &str, lane: &Socket, fe: &Socket, session: &str) -> String {
        let open = envelope(Message::Work(Work {
            work_id: format!("open-{}", unique()),
            revision: 0,
            base_revision: None,
            dataset_ids: vec![],
            views: None,
            payload: WorkPayload::Data(wire::DataWork {
                op: DataOp::Open,
                source: "/nonexistent/e2e.csv".into(),
                cache_path: String::new(),
                format: "csv".into(),
                ingest: Default::default(),
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: wire::VIEW_CHUNK_BYTES,
                render: None,
                edit: None,
            }),
        }));
        fe.send(frame_envelope(&open).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let w = recv_work(lane);
        let wid = w.work_id.clone();
        let r_env = Envelope {
            v: 1,
            id: format!("lane-{wid}"),
            reply_to: None,
            session_id: Some(session.to_string()),
            format: None,
            ts: None,
            body: Message::Result(ResultMsg {
                work_id: wid,
                revision: 0,
                status: Status::Complete,
                payload: ResultPayload::Data(wire::DataResult {
                    dataset_id: None,
                    dataset_revision: None,
                    rows: Some(3),
                    schema: Some(json!([])),
                    error_message: None,
                    row_offset: None,
                    row_count: None,
                    truncated: None,
                    invalidation: None,
                    validation: None,
                    inverse: None,
                }),
                module_version: None,
                message: None,
            }),
        };
        lane.send(frame_envelope(&r_env).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let r = recv_result(fe);
        expect_data_id(r)
    }

    /// **Op-aware evictions, the view half**: a view's lane dies mid-view — the work
    /// fails, the DATASET survives untouched (evict-continue semantics).
    #[test]
    fn lane_death_fails_the_view_but_keeps_the_dataset() {
        let url = format!("inproc://v2-viewkeep-{}", unique());
        let broker = start_broker(url.clone());
        let (lane, _lid) = register_executor_full(
            &url,
            "unused-module",
            0,
            1,
            Some(vec![
                Capability::Data {
                    op: DataOp::Open,
                    formats: Some(vec!["csv".into()]),
                },
                Capability::Data {
                    op: DataOp::View,
                    formats: None,
                },
            ]),
        );
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        // Submit a view; the lane receives it, then dies mid-view.
        let view = envelope(Message::Work(Work {
            work_id: "view-1".into(),
            revision: 0,
            base_revision: None,
            dataset_ids: vec![ds.clone()],
            views: None,
            payload: WorkPayload::Data(wire::DataWork {
                op: DataOp::View,
                source: String::new(),
                cache_path: String::new(),
                format: "csv".into(),
                ingest: Default::default(),
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: wire::VIEW_CHUNK_BYTES,
                render: None,
                edit: None,
            }),
        }));
        fe.send(frame_envelope(&view).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_work(&lane).work_id, "view-1");
        drop(lane);

        // The view fails visibly; the dataset entry SURVIVES (Ready).
        let r = recv_result(&fe);
        assert_eq!(r.work_id, "view-1");
        assert!(matches!(r.status, Status::FatalError));
        let snap = broker.datasets_snapshot();
        assert_eq!(snap.len(), 1, "the dataset survives the lane's death");
        assert_eq!(snap[0].1, "ready");
    }

    /// **D11**: an edit whose declared base revision is not the dataset's current
    /// revision is refused at dispatch (stale_edit) — nothing is sent to the lane.
    #[test]
    fn stale_edit_is_rejected_at_dispatch() {
        let url = format!("inproc://v2-d11-{}", unique());
        let _broker = start_broker(url.clone());
        let (lane, _lid) = register_executor_full(
            &url,
            "unused-module",
            0,
            1,
            Some(vec![
                Capability::Data {
                    op: DataOp::Open,
                    formats: Some(vec!["csv".into()]),
                },
                Capability::Data {
                    op: DataOp::Edit,
                    formats: None,
                },
            ]),
        );
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        // Base revision 7 ≠ current 0 → refusal, nothing dispatched.
        let edit = envelope(Message::Work(Work {
            work_id: "edit-1".into(),
            revision: 7,
            base_revision: None,
            dataset_ids: vec![ds.clone()],
            views: None,
            payload: WorkPayload::Data(wire::DataWork {
                op: DataOp::Edit,
                source: String::new(),
                cache_path: String::new(),
                format: String::new(),
                ingest: Default::default(),
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: wire::VIEW_CHUNK_BYTES,
                render: None,
                edit: Some(Box::new(wire::EditOp::InsertRows { at: 1, count: 1 })),
            }),
        }));
        fe.send(frame_envelope(&edit).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let r = recv_result(&fe);
        assert_eq!(r.work_id, "edit-1");
        assert!(
            matches!(r.status, Status::ValidationError),
            "stale edit refused"
        );
        match &r.payload {
            ResultPayload::Data(d) => {
                assert!(
                    d.validation
                        .as_ref()
                        .is_some_and(|v| v.iter().any(|i| i.code == "stale_edit"))
                );
            }
            other => panic!("expected data payload, got {other:?}"),
        }
        assert_silent(&lane, "nothing must be dispatched for a stale edit");
    }

    // ── THE VIEWS PHASE (§12 step 5) ──────────────────────────────────────────

    /// Test-only: the view book as (id, state, bytes, held).
    fn views_snapshot(broker: &Broker) -> Vec<(String, String, u64, bool)> {
        let (tx, rx) = std::sync::mpsc::channel();
        broker
            .tx
            .send(RouterMsg::QueryViews(tx))
            .expect("query views");
        rx.recv_timeout(Duration::from_secs(5))
            .expect("views snapshot")
    }

    /// Recv a raw frame on an executor channel and parse the JSON part as a Value —
    /// how the tests see INJECTED envelope fields (`dataset_paths`, `view_refs`)
    /// that the typed `Work` deliberately doesn't carry.
    fn recv_raw_json(ch: &Socket) -> (serde_json::Value, Message) {
        let raw = ch.recv().expect("a frame");
        let len = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
        let v: serde_json::Value = serde_json::from_slice(&raw[4..4 + len]).unwrap();
        let env = deframe(&raw[..]).expect("valid envelope");
        (v, env.body)
    }

    /// An analysis work with stapled view specs.
    fn views_work(
        work_id: &str,
        revision: u64,
        module: &str,
        dataset: &str,
        views: Vec<wire::ViewSpec>,
    ) -> Envelope {
        let mut w = analysis_work(work_id, module, revision);
        let Message::Work(inner) = &mut w.body else {
            unreachable!()
        };
        inner.dataset_ids = vec![dataset.to_string()];
        inner.views = Some(views);
        w
    }

    fn view_spec(dataset: &str, name: &str, as_type: wire::ViewLevel) -> wire::ViewSpec {
        wire::ViewSpec {
            dataset_id: dataset.to_string(),
            columns: Some(vec![wire::ViewColumn {
                name: name.to_string(),
                as_type,
            }]),
            filter: None,
            all: false,
        }
    }

    /// The fill-order/confirm pair on a lane channel.
    fn recv_fill(ch: &Socket) -> wire::CacheFill {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let raw = ch.recv().expect("a fill frame");
            if let Some(env) = deframe(&raw[..]) {
                if let Message::CacheFill(f) = env.body {
                    return f;
                }
                // (a work for a lane op — the caller races us; put it back conceptually
                // by failing loudly: view tests use dedicated lanes)
                panic!("expected cache_fill, got {:?}", env.body);
            }
            assert!(Instant::now() < deadline, "no fill arrived");
        }
    }

    fn send_cache_filled(ch: &Socket, view_id: &str, bytes: Option<u64>, error: Option<String>) {
        let env = Envelope {
            v: 1,
            id: format!("filled-{view_id}"),
            reply_to: None,
            session_id: None,
            format: None,
            ts: None,
            body: Message::CacheFilled(wire::CacheFilled {
                view_id: view_id.to_string(),
                bytes,
                error,
            }),
        };
        ch.send(frame_envelope(&env).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
    }

    /// A lane advertising the full data + view-build capability set.
    fn register_view_lane(url: &str) -> (Socket, String) {
        register_executor_full(
            url,
            "unused-module",
            0,
            1,
            Some(vec![
                Capability::Data {
                    op: DataOp::Open,
                    formats: Some(vec!["csv".into()]),
                },
                Capability::Data {
                    op: DataOp::ViewBuild,
                    formats: None,
                },
            ]),
        )
    }

    /// **The implied build, end to end** (§8): a stapled miss parks the work (never
    /// dispatched), the router orders ONE fill to the free ViewBuild-capable worker,
    /// the worker's `cache_filled` un-parks the work — and the dispatch carries the
    /// RESOLVED `view_refs` (AV10) next to the untouched `dataset_paths` bridge.
    #[test]
    fn view_miss_parks_until_cache_filled() {
        let url = format!("inproc://v2-vmiss-{}", unique());
        let broker = start_broker(url.clone());
        let (lane, _lid) = register_view_lane(&url);
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        let spec = view_spec(&ds, "score", wire::ViewLevel::Scale);
        let id = wire::view_id(&spec, 0); // fresh dataset, revision 0
        fe.send(frame_envelope(&views_work("V", 1, "jaspTTests", &ds, vec![spec])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();

        // Parked, not dispatched — even though the analysis runner is idle.
        assert_silent(&runner, "a view miss never dispatches");
        // The book shows Wanted → Ordered once order_fills ran.
        let snap = views_snapshot(&broker);
        assert_eq!(snap.len(), 1, "one view entry: {snap:?}");
        assert_eq!(snap[0].0, id);
        assert_eq!(snap[0].1, "ordered");

        // The fill went to the free builder.
        let fill = recv_fill(&lane);
        assert_eq!(fill.view_id, id);
        assert_eq!(fill.spec.dataset_id, ds);
        assert_eq!(fill.base_revision, 0);

        // Confirm → Ready → un-park → dispatch with resolved refs.
        send_cache_filled(&lane, &id, Some(1234), None);
        let (v, body) = recv_raw_json(&runner);
        assert!(matches!(body, Message::Work(w) if w.work_id == "V"));
        let refs = v["view_refs"].as_array().expect("view_refs injected");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0]["dataset_id"], ds);
        assert_eq!(refs[0]["view_id"], id);
        assert!(
            refs[0]["path"].as_str().unwrap().contains(&id),
            "the ref points at the content-addressed blob: {}",
            refs[0]["path"]
        );
        // The migration bridge rides along untouched.
        assert!(
            v["dataset_paths"][&ds].is_string(),
            "dataset_paths still injected"
        );
        let snap = views_snapshot(&broker);
        assert_eq!(snap[0].1, "ready");
        assert_eq!(snap[0].2, 1234);
        assert!(snap[0].3, "held by the live dispatch");
    }

    /// **Cache-hit fast path**: a second work stapling the SAME spec dispatches
    /// immediately (no fill, no park) and the hold rule tracks the new dispatch.
    #[test]
    fn view_hit_dispatches_without_a_build() {
        let url = format!("inproc://v2-vhit-{}", unique());
        let broker = start_broker(url.clone());
        let (lane, _lid) = register_view_lane(&url);
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        let spec = view_spec(&ds, "score", wire::ViewLevel::Scale);
        let id = wire::view_id(&spec, 0);
        fe.send(
            frame_envelope(&views_work("V1", 1, "jaspTTests", &ds, vec![spec.clone()])).as_slice(),
        )
        .map_err(|(_, e)| e)
        .unwrap();
        let fill = recv_fill(&lane);
        assert_eq!(fill.view_id, id);
        send_cache_filled(&lane, &id, Some(100), None);
        let _w1 = recv_work(&runner);
        send_result(&runner, &session, "V1", 1);
        let _r = recv_result(&fe);

        // Same spec again: instant dispatch, nothing to the lane.
        fe.send(frame_envelope(&views_work("V2", 1, "jaspTTests", &ds, vec![spec])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let w2 = recv_work(&runner);
        assert_eq!(w2.work_id, "V2");
        assert_silent(&lane, "a hit must not re-order a fill");
        assert!(views_snapshot(&broker)[0].3, "held by V2's dispatch");
        send_result(&runner, &session, "V2", 1);
        let _ = recv_result(&fe);
    }

    /// **Pass-through** (§8): `all: true` unfiltered IS the base file — dispatch is
    /// immediate, the ref names the BASE cache path, no view book entry ever exists.
    #[test]
    fn passthrough_hits_the_base_file() {
        let url = format!("inproc://v2-vpass-{}", unique());
        let broker = start_broker(url.clone());
        let (lane, _lid) = register_view_lane(&url);
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        let spec = wire::ViewSpec {
            dataset_id: ds.clone(),
            columns: None,
            filter: None,
            all: true,
        };
        fe.send(frame_envelope(&views_work("P", 1, "jaspTTests", &ds, vec![spec])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let (v, body) = recv_raw_json(&runner);
        assert!(matches!(body, Message::Work(w) if w.work_id == "P"));
        let refs = v["view_refs"].as_array().unwrap();
        assert_eq!(refs.len(), 1);
        assert!(refs[0]["view_id"].is_null(), "pass-through has no blob id");
        assert_eq!(
            refs[0]["path"], v["dataset_paths"][&ds],
            "the ref IS the base cache file — zero copy"
        );
        assert!(
            views_snapshot(&broker).is_empty(),
            "no book entry for a pass-through"
        );
        send_result(&runner, &session, "P", 1);
        let _ = recv_result(&fe);
    }

    /// **Filters are refused visibly** (the derived-columns gate): no dispatch, no
    /// fill, a `bad_request` error naming the work — never a silent ignore.
    #[test]
    fn filter_specs_are_refused() {
        let url = format!("inproc://v2-vfilt-{}", unique());
        let broker = start_broker(url.clone());
        let (lane, _lid) = register_view_lane(&url);
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        let spec = wire::ViewSpec {
            filter: Some("some_mask".into()),
            ..view_spec(&ds, "score", wire::ViewLevel::Scale)
        };
        fe.send(frame_envelope(&views_work("F", 1, "jaspTTests", &ds, vec![spec])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let raw = fe.recv().expect("the refusal");
        let env = deframe(&raw[..]).unwrap();
        match env.body {
            Message::Error(e) => {
                assert_eq!(e.code, "bad_request");
                assert!(e.message.contains("filters are not supported"));
                assert_eq!(e.work_id.as_deref(), Some("F"));
            }
            other => panic!("expected an error, got {other:?}"),
        }
        assert_silent(&runner, "a refused work never dispatches");
        assert_silent(&lane, "a refused work never orders a fill");
        assert!(views_snapshot(&broker).is_empty());
    }

    /// **Deterministic build failures poison the view** and fail waiters visibly —
    /// retrying an unbuildable spec can never succeed, so it is never retried.
    #[test]
    fn build_failure_fails_waiters_and_poisons() {
        let url = format!("inproc://v2-vfail-{}", unique());
        let broker = start_broker(url.clone());
        let (lane, _lid) = register_view_lane(&url);
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        let spec = view_spec(&ds, "score", wire::ViewLevel::Scale);
        let id = wire::view_id(&spec, 0);
        fe.send(
            frame_envelope(&views_work("W1", 1, "jaspTTests", &ds, vec![spec.clone()])).as_slice(),
        )
        .map_err(|(_, e)| e)
        .unwrap();
        assert_eq!(recv_fill(&lane).view_id, id);
        send_cache_filled(&lane, &id, None, Some("unknown column 'score'".into()));

        // The waiter fails visibly with the builder's own reason (after its park
        // running-marker — recv_terminal skips markers).
        let r = recv_terminal(&fe);
        assert_eq!(r.work_id, "W1");
        assert!(matches!(r.status, Status::FatalError));
        match r.payload {
            ResultPayload::AnalysisRClassicJaspbase(_) => {} // error rides the message
            other => panic!("expected analysis payload, got {other:?}"),
        }

        // A second work with the SAME spec is refused at staple time (poisoned).
        fe.send(frame_envelope(&views_work("W2", 1, "jaspTTests", &ds, vec![spec])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let r2 = recv_terminal(&fe);
        assert_eq!(r2.work_id, "W2");
        assert!(matches!(r2.status, Status::FatalError));
        let snap = views_snapshot(&broker);
        assert_eq!(snap[0].1, "failed", "the view is poisoned: {snap:?}");
        assert_silent(&runner, "poisoned specs never dispatch");
    }

    /// **The one hold rule** (§4): under budget pressure only UNHELD ready views are
    /// LRU food — a view referenced by a live dispatch is never reclaimed. The tick
    /// is driven directly (deterministic — no hang-detector cadence in the test).
    #[test]
    fn hold_rule_lru_reclaims_only_unheld_views() {
        let url = format!("inproc://v2-vhold-{}", unique());
        // Two 50 MiB views against a 64 MiB budget: pressure only when one is unheld.
        let config = Config {
            view_budget_bytes: 64 * 1024 * 1024,
            ..test_config(url.clone())
        };
        let (tx, rx) = mpsc::channel();
        let broker = Broker::start_inner(config, tx, rx, None);
        let control = Socket::new(Protocol::Rep0).unwrap();
        wire::transport::listen_control(&control, &url).unwrap();
        Broker::arm_control(&broker, Arc::new(control)).unwrap();
        let tick = |broker: &Broker| {
            broker.tx.send(RouterMsg::Tick).unwrap();
            // The tick is synchronous on the router thread; QueryViews shares the
            // mailbox, so its reply proves the Tick ran (a fence).
            let (qtx, qrx) = std::sync::mpsc::channel();
            broker.tx.send(RouterMsg::QueryViews(qtx)).unwrap();
            qrx.recv_timeout(Duration::from_secs(5)).unwrap()
        };

        let (lane, _lid) = register_view_lane(&url);
        // slots=2: HA and HB dispatch CONCURRENTLY — each holds its view at the same
        // time (the whole point: pressure while both are held).
        let (runner, _rid) = register_executor_full(&url, "jaspTTests", 0, 2, None);
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane, &fe, &session);

        // Two views, both Ready; two dispatches holding one each.
        let spec_a = view_spec(&ds, "a", wire::ViewLevel::Scale);
        let id_a = wire::view_id(&spec_a, 0);
        let spec_b = view_spec(&ds, "b", wire::ViewLevel::Scale);
        let id_b = wire::view_id(&spec_b, 0);
        fe.send(frame_envelope(&views_work("HA", 1, "jaspTTests", &ds, vec![spec_a])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_fill(&lane).view_id, id_a);
        send_cache_filled(&lane, &id_a, Some(50 * 1024 * 1024), None);
        let _wa = recv_work(&runner); // HA dispatched — holds id_a

        fe.send(frame_envelope(&views_work("HB", 1, "jaspTTests", &ds, vec![spec_b])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        assert_eq!(recv_fill(&lane).view_id, id_b);
        send_cache_filled(&lane, &id_b, Some(50 * 1024 * 1024), None);
        let _wb = recv_work(&runner); // HB dispatched — holds id_b

        // Both held, 100 MiB > 64 MiB budget: the sweep must reclaim NOTHING.
        let snap = tick(&broker);
        assert_eq!(snap.len(), 2, "held views are never reclaimed: {snap:?}");
        assert!(snap.iter().all(|s| s.3), "both held");

        // Terminal HB: id_b unheld → the next tick reclaims it (LRU); id_a stays
        // (still held by HA). (recv_terminal — the park marker must not satisfy us.)
        send_result(&runner, &session, "HB", 1);
        let r = recv_terminal(&fe);
        assert_eq!(r.work_id, "HB");
        let snap = tick(&broker);
        assert_eq!(snap.len(), 1, "the unheld view was reclaimed: {snap:?}");
        assert_eq!(snap[0].0, id_a, "the held one survives");

        send_result(&runner, &session, "HA", 1);
        let _ = recv_terminal(&fe);
    }

    /// **Worker death mid-build just re-orders** (§8): the waiters were never
    /// dispatched, so nothing frontend-visible fails — the fill resets to Wanted and
    /// the next capable worker takes it; the work then flows exactly as on a hit.
    #[test]
    fn worker_death_reorders_the_fill() {
        let url = format!("inproc://v2-vreorder-{}", unique());
        let broker = start_broker(url.clone());
        // One lane first — it serves the open (deterministic: the only Open).
        let (lane1, _lid1) = register_view_lane(&url);
        let (runner, _rid) = register_executor(&url, "jaspTTests");
        let (fe, session) = hello_frontend(&url);
        let ds = open_dataset(&url, &lane1, &fe, &session);
        // A second builder joins: higher seq ⇒ it wins the fill order.
        let (lane2, lid2) = register_view_lane(&url);

        let spec = view_spec(&ds, "score", wire::ViewLevel::Scale);
        let id = wire::view_id(&spec, 0);
        fe.send(frame_envelope(&views_work("R", 1, "jaspTTests", &ds, vec![spec])).as_slice())
            .map_err(|(_, e)| e)
            .unwrap();
        let fill = recv_fill(&lane2);
        assert_eq!(fill.view_id, id);
        assert_silent(&runner, "still parked — nothing dispatches");

        // lane2 dies with the fill outstanding: eviction resets the fill to Wanted,
        // and the re-order hands it to lane1 — the work never fails, never re-submits.
        drop(lane2);
        let deadline = Instant::now() + Duration::from_millis(2_000);
        loop {
            if !broker.runners_snapshot().contains(&lid2) {
                break;
            }
            assert!(Instant::now() < deadline, "dead builder never evicted");
            std::thread::sleep(Duration::from_millis(25));
        }
        let fill2 = recv_fill(&lane1);
        assert_eq!(
            fill2.view_id, id,
            "the SAME fill re-ordered to the survivor"
        );

        // Confirm on lane1 → dispatch as if nothing happened.
        send_cache_filled(&lane1, &id, Some(64), None);
        let w = recv_work(&runner);
        assert_eq!(w.work_id, "R");
        send_result(&runner, &session, "R", 1);
        let r = recv_terminal(&fe);
        assert_eq!(r.status, Status::Complete);
    }
}
