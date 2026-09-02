//! Runner provisioner — spawns runner processes on demand (§9.3).
//!
//! The provisioner is the *only* place runner processes are created and destroyed. It exists to get
//! blocking process-lifecycle work (spawn, wait, kill) off the single-threaded router thread, exactly
//! like the janitor gets blocking filesystem work off it. The router never touches a process; it only
//! exchanges messages with this thread.
//!
//! # Protocol with the router
//!
//! * **Router → provisioner** ([`ProvReq`], on a dedicated channel):
//!   - `Provision { module, version }` — some parked work needs a runner for `module`. Idempotent:
//!     the provisioner ignores it if that module is already spawning or already has a live runner.
//!   - `EnsureLane { lane }` — ensure a data-plane lane is alive (spawn if not); idempotent. Sent
//!     at boot (pre-spawn, so the first open pays no spawn latency) and when data work parks.
//!   - `RunnerUp { modules, lanes }` — the router accepted a registration advertising these; clear
//!     the in-flight spawn for them (this is the authoritative "the spawn worked" signal).
//!   - `RunnerGone { modules, lanes }` — a runner was evicted/died. Modules: forget, so a later
//!     `Provision` re-spawns. Lanes: re-spawn immediately — lanes are pinned and auto-restarted.
//! * **Provisioner → router** ([`ProvEvent`], via the `on_event` callback injected at
//!   construction): the provisioner reports an outcome by *calling* it from its own thread.
//!   `main.rs` wires the callback onto the router's mailbox — a non-blocking mpsc send — so this
//!   module never sees the router's message types and no relay thread is needed.
//!   - `ProvisionFailed { module, reason }` — the module cannot be provided (not in the libset,
//!     or the spawn crashed / never registered within the timeout). The router fails the parked
//!     work.
//!   - `LaneFailed { lane, reason }` — same for a lane; the router fails the data work parked
//!     awaiting it.
//!
//! # Reconciliation: why registration is the success signal
//!
//! The provisioner spawns a runner and then *waits for the router to tell it the runner registered*
//! (`RunnerUp`). A spawned process that dies before registering is a **boot failure** (detected by
//! reaping the child) and yields `ProvisionFailed`. A spawned process that registered and *later*
//! dies is reported by the router as `RunnerGone` (it noticed the channel close) and simply allows
//! re-provisioning on the next `Provision`.

use crate::ModuleInfo;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// A pinned data-plane lane the provisioner keeps alive (dataset-manager-design §1: lanes
/// are pinned, not rotated). Today the Rust data-runner (CSV open; later Arrow-native open
/// + all writes); later the R utility lane for reader-heavy formats (spss/excel/stata/sas).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LaneKind {
    /// The Rust data-runner binary (`jasp-data-runner`).
    RustData,
}

/// How to spawn a lane: program + arguments. The control endpoint rides as `JASP_ORCH_URL`.
#[derive(Debug, Clone)]
pub struct LaneSpec {
    pub kind: LaneKind,
    pub program: PathBuf,
    pub args: Vec<String>,
}

/// How to spawn an analysis-module runner (R): interpreter + entry script. The libpath rides
/// as `JASP_RUNNER_LIBDIR`, the control endpoint as an argument.
#[derive(Debug, Clone)]
pub struct AnalysisRunnerSpec {
    pub runner_script: PathBuf,
    pub rscript_bin: String,
}

/// Router → provisioner requests.
#[derive(Debug)]
pub enum ProvReq {
    /// Parked work needs a runner for `module` (requested `version`). Idempotent.
    Provision { module: String, version: String },
    /// Ensure a lane is alive (spawn if not). Idempotent.
    EnsureLane { lane: LaneKind },
    /// A runner advertising these modules/lanes just registered — its spawn succeeded.
    RunnerUp {
        modules: Vec<String>,
        lanes: Vec<LaneKind>,
    },
    /// A runner for these modules/lanes was evicted or died. Modules: forget, so a later
    /// `Provision` re-spawns. Lanes: re-spawn immediately (auto-restart — lanes are pinned).
    RunnerGone {
        modules: Vec<String>,
        lanes: Vec<LaneKind>,
    },
    /// **v2 (additive; classic never sends it):** the router's hang detector declared a
    /// wedged runner (outstanding work, no activity past the timeout). Kill every spawned
    /// child providing any of these modules/lanes — the process death surfaces as a pipe
    /// close on its channel, which evicts it router-side with full op-aware teardown.
    /// Lanes re-spawn immediately (auto-restart, same as `RunnerGone`). Children nobody
    /// spawned (attached runners) match nothing here; the router evicts those from its
    /// books directly.
    Recycle {
        modules: Vec<String>,
        lanes: Vec<LaneKind>,
    },
}

/// Provisioner → router events, delivered via the injected `on_event` callback.
#[derive(Debug)]
pub enum ProvEvent {
    /// `module` cannot be provided; fail its parked work with `reason`.
    ProvisionFailed { module: String, reason: String },
    /// `lane` cannot be provided (spawn failed, or never registered in time); fail the data
    /// work parked awaiting it with `reason`.
    LaneFailed { lane: LaneKind, reason: String },
}

/// What a tracked spawned process provides.
#[derive(Debug, Clone)]
enum Provides {
    Modules(Vec<String>),
    Lane(LaneKind),
}

/// One spawned runner process we are tracking.
struct Spawned {
    child: Child,
    /// What this process provides: analysis modules or a lane.
    provides: Provides,
    started: Instant,
    /// Set true once the router reports `RunnerUp` for it.
    registered: bool,
}

/// The runner provisioner. Runs on its own thread ([`RunnerProvisioner::run`]).
pub struct RunnerProvisioner {
    /// module → libpath: which self-consistent library provides each module.
    module_lib: HashMap<String, PathBuf>,
    /// libpath → the modules it contains (what a runner bound to it advertises).
    lib_modules: HashMap<PathBuf, Vec<String>>,
    /// How to spawn analysis runners; `None` when only lanes are configured.
    analysis: Option<AnalysisRunnerSpec>,
    /// Control endpoint URL the spawned runner dials to register.
    control_url: String,
    /// A spawned runner that has not registered within this window is a boot failure.
    spawn_timeout: Duration,
    /// How often to poll spawned children (reaping + boot-timeout).
    tick: Duration,

    req_rx: mpsc::Receiver<ProvReq>,
    /// Reports outcomes to the router. `main.rs` injects a closure that re-wraps the event into
    /// a `RouterMsg` and enqueues it on the router's mailbox; this module stays ignorant of the
    /// router's types. Called from the provisioner thread — safe because an mpsc send never
    /// blocks, which is also why no relay thread is needed.
    on_event: Box<dyn Fn(ProvEvent) + Send>,

    /// module → pid, for modules that are currently spawning OR have a live runner. The dedup key:
    /// a `Provision` for a module present here is ignored.
    active: HashMap<String, u32>,
    /// Configured lanes: kind → how to spawn them. Empty → no lanes.
    lanes: HashMap<LaneKind, LaneSpec>,
    /// lane → pid, for lanes currently spawning OR live. The `EnsureLane` dedup key.
    lane_active: HashMap<LaneKind, u32>,
    /// pid → spawned-process record.
    spawned: HashMap<u32, Spawned>,
}

impl RunnerProvisioner {
    pub fn new(
        scan: LibsetScan,
        analysis: Option<AnalysisRunnerSpec>,
        lanes: Vec<LaneSpec>,
        control_url: String,
        spawn_timeout: Duration,
        req_rx: mpsc::Receiver<ProvReq>,
        on_event: Box<dyn Fn(ProvEvent) + Send>,
    ) -> Self {
        // The libset is scanned exactly once, before the router thread exists (the scan is
        // filesystem I/O; the router must never block). `Broker::start` performs the scan and
        // shares it: the routing indexes seed the provisioner, the module metadata seeds the
        // discovery catalog.
        Self {
            module_lib: scan.module_lib,
            lib_modules: scan.lib_modules,
            analysis,
            lanes: lanes.into_iter().map(|s| (s.kind, s)).collect(),
            control_url,
            spawn_timeout,
            tick: Duration::from_millis(250),
            req_rx,
            on_event,
            active: HashMap::new(),
            lane_active: HashMap::new(),
            spawned: HashMap::new(),
        }
    }

    /// The event loop: drain requests, then reap/timeout children, on a short tick.
    pub fn run(mut self) {
        loop {
            // Drain all pending requests without blocking (so a burst of `Provision`s is handled
            // before we reap), then block up to `tick` for the next one.
            loop {
                match self.req_rx.try_recv() {
                    Ok(req) => {
                        if !self.handle(req) {
                            return;
                        }
                    }
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => return,
                }
            }
            match self.req_rx.recv_timeout(self.tick) {
                Ok(req) => {
                    if !self.handle(req) {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
            self.reap();
        }
    }

    /// Handle one request. Returns `false` to signal the loop should exit (unused today; the loop
    /// exits on disconnect, but kept for symmetry/future shutdown messages).
    fn handle(&mut self, req: ProvReq) -> bool {
        match req {
            ProvReq::Provision { module, version } => self.provision(module, version),
            ProvReq::EnsureLane { lane } => self.provision_lane(lane),
            ProvReq::RunnerUp { modules, lanes } => {
                for m in modules {
                    if let Some(pid) = self.active.get(&m).copied()
                        && let Some(s) = self.spawned.get_mut(&pid)
                    {
                        s.registered = true;
                    }
                }
                for lane in lanes {
                    if let Some(pid) = self.lane_active.get(&lane).copied()
                        && let Some(s) = self.spawned.get_mut(&pid)
                    {
                        s.registered = true;
                    }
                }
            }
            ProvReq::RunnerGone { modules, lanes } => {
                self.forget(modules, lanes);
            }
            ProvReq::Recycle { modules, lanes } => {
                // Kill every spawned child providing any of these (the reaper cleans up
                // the records; the pipe close evicts it router-side).
                let mut victims = 0usize;
                for s in self.spawned.values_mut() {
                    let matches = match &s.provides {
                        Provides::Modules(ms) => ms.iter().any(|m| modules.contains(m)),
                        Provides::Lane(l) => lanes.contains(l),
                    };
                    if matches {
                        let _ = s.child.kill();
                        victims += 1;
                    }
                }
                println!(
                    "[provisioner] recycle: killed {victims} wedged child(ren) \
                     (modules {modules:?}, lanes {lanes:?})"
                );
                // Same bookkeeping as RunnerGone: forget modules, re-spawn lanes
                // (auto-restart). The kill makes lane re-spawn immediate.
                self.forget(modules, lanes);
            }
        }
        true
    }

    /// `RunnerGone`/`Recycle` bookkeeping: forget modules (so a future `Provision`
    /// re-spawns), re-spawn lanes immediately (auto-restart — lanes are pinned; every
    /// respawn is caused by exactly one death, so this cannot loop on its own).
    fn forget(&mut self, modules: Vec<String>, lanes: Vec<LaneKind>) {
        for m in modules {
            // Forget the module so a future Provision re-spawns. Leave the process record
            // for the reaper if it is still around (it may already be dead).
            if let Some(pid) = self.active.remove(&m)
                && let Some(s) = self.spawned.get_mut(&pid)
            {
                // If it was registered and is now gone, drop our claim on its other
                // modules too only when the process is reaped; here we just release `m`.
                if let Provides::Modules(modules) = &mut s.provides {
                    modules.retain(|x| x != &m);
                }
            }
        }
        for lane in lanes {
            self.lane_active.remove(&lane);
            self.provision_lane(lane);
        }
    }

    /// Spawn a runner for `module` if nothing is already providing it.
    fn provision(&mut self, module: String, version: String) {
        if self.active.contains_key(&module) {
            return; // already spawning or live — idempotent
        }
        let Some(lib) = self.module_lib.get(&module).cloned() else {
            self.fail(
                module,
                "module not present in any configured libpath (libset)".to_string(),
            );
            return;
        };
        let modules = self
            .lib_modules
            .get(&lib)
            .cloned()
            .unwrap_or_else(|| vec![module.clone()]);

        let Some(spec) = self.analysis.as_ref() else {
            self.fail(module, "no analysis-runner configuration".to_string());
            return;
        };

        println!(
            "[provisioner] spawning runner for module {module} (version {version}) from {}",
            lib.display()
        );

        let child = Command::new(&spec.rscript_bin)
            .arg(&spec.runner_script)
            .arg(&self.control_url)
            .env("JASP_RUNNER_LIBDIR", &lib)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn();

        match child {
            Ok(child) => {
                let pid = child.id();
                for m in &modules {
                    self.active.insert(m.clone(), pid);
                }
                self.spawned.insert(
                    pid,
                    Spawned {
                        child,
                        provides: Provides::Modules(modules),
                        started: Instant::now(),
                        registered: false,
                    },
                );
            }
            Err(e) => self.fail(module, format!("failed to spawn runner process: {e}")),
        }
    }

    /// Spawn a lane if nothing is already providing it (idempotent).
    fn provision_lane(&mut self, lane: LaneKind) {
        if self.lane_active.contains_key(&lane) {
            return; // already spawning or live
        }
        let Some(spec) = self.lanes.get(&lane).cloned() else {
            self.fail_lane(lane, "lane not configured".to_string());
            return;
        };

        println!(
            "[provisioner] spawning lane {:?} from {}",
            lane,
            spec.program.display()
        );

        let child = Command::new(&spec.program)
            .args(&spec.args)
            .env("JASP_ORCH_URL", &self.control_url)
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn();

        match child {
            Ok(child) => {
                let pid = child.id();
                self.lane_active.insert(lane, pid);
                self.spawned.insert(
                    pid,
                    Spawned {
                        child,
                        provides: Provides::Lane(lane),
                        started: Instant::now(),
                        registered: false,
                    },
                );
            }
            Err(e) => self.fail_lane(lane, format!("failed to spawn lane process: {e}")),
        }
    }

    /// Reap exited children and time out spawns that never registered.
    fn reap(&mut self) {
        let mut finished: Vec<(u32, Provides, Option<String>)> = Vec::new();
        for (pid, s) in self.spawned.iter_mut() {
            match s.child.try_wait() {
                Ok(Some(status)) => {
                    // Process exited. If it never registered, that is a boot failure.
                    let err = if s.registered {
                        None
                    } else {
                        Some(format!(
                            "runner exited before registering (status {status})"
                        ))
                    };
                    finished.push((*pid, s.provides.clone(), err));
                }
                Ok(None) => {
                    // Still running. Boot-timeout applies only before registration.
                    if !s.registered && s.started.elapsed() > self.spawn_timeout {
                        let _ = s.child.kill();
                        finished.push((
                            *pid,
                            s.provides.clone(),
                            Some(format!(
                                "runner did not register within {}s",
                                self.spawn_timeout.as_secs()
                            )),
                        ));
                    }
                }
                Err(e) => {
                    eprintln!("[provisioner] error waiting on pid {pid}: {e}");
                }
            }
        }

        for (pid, provides, err) in finished {
            self.spawned.remove(&pid);
            match provides {
                Provides::Modules(modules) => {
                    for m in &modules {
                        self.active.remove(m);
                    }
                    if let Some(reason) = err {
                        for m in modules {
                            self.fail(m, reason.clone());
                        }
                    }
                }
                Provides::Lane(lane) => {
                    // Clear the live claim only if it still points at this pid (a
                    // RunnerGone-triggered respawn may already own the slot with a fresh pid).
                    if self.lane_active.get(&lane) == Some(&pid) {
                        self.lane_active.remove(&lane);
                    }
                    // A registered lane's death is handled by the router's RunnerGone
                    // (auto-restart); only boot failures are reported here.
                    if let Some(reason) = err {
                        self.fail_lane(lane, reason);
                    }
                }
            }
        }
    }

    fn fail(&self, module: String, reason: String) {
        eprintln!("[provisioner] cannot provision {module}: {reason}");
        (self.on_event)(ProvEvent::ProvisionFailed { module, reason });
    }

    fn fail_lane(&self, lane: LaneKind, reason: String) {
        eprintln!("[provisioner] cannot provision lane {lane:?}: {reason}");
        (self.on_event)(ProvEvent::LaneFailed { lane, reason });
    }
}

/// The result of scanning a libset: the provisioner's routing indexes AND the discovery metadata
/// for every module found. Produced once at startup and shared between the provisioner (which
/// module maps to which libpath) and the router's module catalog (what the frontend can load).
#[derive(Debug, Clone, Default)]
pub struct LibsetScan {
    /// module → libpath: which self-consistent library provides each module.
    pub module_lib: HashMap<String, PathBuf>,
    /// libpath → the modules it contains (what a runner bound to it advertises).
    pub lib_modules: HashMap<PathBuf, Vec<String>>,
    /// Discovery metadata — `{name, version, base_uri}` per module, sorted by name and deduped
    /// by `(name, version)` for a deterministic catalog.
    pub modules: Vec<ModuleInfo>,
}

/// Scan a libset for JASP modules. Each libset entry is a **base directory** whose subdirectories
/// are libpaths (self-contained R libraries); within each libpath the installed packages are
/// subdirectories, and the analysis modules are those that are `jasp*` and ship a `Description.qml`
/// (the same rule the runner uses in `scan_modules`). Returns the provisioner's routing indexes
/// (module-name → libpath, libpath → module-names) and the discovery metadata for each module
/// (name/version from its DESCRIPTION, base URI the directory itself).
pub fn scan_libset(libset: &[PathBuf]) -> LibsetScan {
    let mut module_lib: HashMap<String, PathBuf> = HashMap::new();
    let mut lib_modules: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut modules: Vec<ModuleInfo> = Vec::new();
    for base in libset {
        let Ok(libpaths) = std::fs::read_dir(base) else {
            eprintln!("[provisioner] cannot read libset base {}", base.display());
            continue;
        };
        for libpath in subdirs(libpaths) {
            let Ok(packages) = std::fs::read_dir(&libpath) else {
                continue;
            };
            // Keep the module directories themselves: the base URI is the directory we found by
            // scanning — never constructed from the module name.
            let module_dirs: Vec<(PathBuf, String)> = subdirs(packages)
                .filter(|pkg| is_module_dir(pkg))
                .filter_map(|pkg| dir_name(&pkg).map(|name| (pkg, name)))
                .collect();
            let names: Vec<String> = module_dirs.iter().map(|(_, name)| name.clone()).collect();
            for name in &names {
                module_lib.insert(name.clone(), libpath.clone());
            }
            if !names.is_empty() {
                lib_modules.insert(libpath.clone(), names);
            }
            for (dir, fallback_name) in &module_dirs {
                modules.push(module_info(dir, fallback_name));
            }
        }
    }
    // Canonical, deterministic catalog: sorted by (name, version, base_uri), then deduped by
    // (name, version) so a module present in two libpaths yields one entry.
    modules.sort();
    modules.dedup_by(|a, b| a.name == b.name && a.version == b.version);
    LibsetScan {
        module_lib,
        lib_modules,
        modules,
    }
}

/// Discovery metadata for one module directory: `name`/`version` from its DESCRIPTION (R's DCF
/// format), falling back to the directory name / an empty version if it is missing or unreadable
/// (every properly installed package ships one, so this is purely defensive), and a `file://`
/// base URI of the directory itself.
fn module_info(dir: &Path, fallback_name: &str) -> ModuleInfo {
    let (name, version) = match std::fs::read_to_string(dir.join("DESCRIPTION")) {
        Ok(text) => {
            let fields = parse_dcf(&text);
            (
                fields
                    .get("Package")
                    .cloned()
                    .unwrap_or_else(|| fallback_name.to_string()),
                fields.get("Version").cloned().unwrap_or_default(),
            )
        }
        Err(e) => {
            eprintln!(
                "[provisioner] no readable DESCRIPTION in {} ({e}); using dir name",
                dir.display()
            );
            (fallback_name.to_string(), String::new())
        }
    };
    ModuleInfo {
        name,
        version,
        base_uri: file_uri(dir),
    }
}

/// Parse `Field: value` pairs from R's DCF format (the DESCRIPTION file). Continuation lines
/// (leading whitespace) are ignored — `Package` and `Version` are always single-line.
fn parse_dcf(text: &str) -> HashMap<String, String> {
    text.lines()
        .filter(|line| !line.starts_with(char::is_whitespace))
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.trim().to_string(), value.trim().to_string()))
        .collect()
}

/// Render an absolute directory path as a trailing-slash `file://` URI, percent-encoding every
/// byte outside RFC 3986's unreserved set (plus `/`) so paths with spaces or non-ASCII survive.
/// `file://` on desktop; for the webapp the same path logic serves `http://` — the scheme is the
/// only thing that changes.
pub fn file_uri(dir: &Path) -> String {
    let path = dir
        .to_str()
        .map(str::to_string)
        .unwrap_or_else(|| dir.to_string_lossy().into_owned());
    let mut out = String::with_capacity(path.len() + 8);
    out.push_str("file://");
    for &b in path.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    if !out.ends_with('/') {
        out.push('/');
    }
    out
}

/// The subdirectories among a `read_dir`'s entries (skipping files and unreadable entries).
fn subdirs(entries: std::fs::ReadDir) -> impl Iterator<Item = PathBuf> {
    entries.flatten().map(|e| e.path()).filter(|p| p.is_dir())
}

/// A path's file name as an owned `String`, if it is valid UTF-8.
fn dir_name(path: &Path) -> Option<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
}

/// True if `dir` looks like an installed JASP module (`jasp*` with a `Description.qml`). Exposed for
/// tests and for any caller that wants the module test without a full libset scan.
pub fn is_module_dir(dir: &Path) -> bool {
    dir.is_dir()
        && dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("jasp"))
            .unwrap_or(false)
        && dir.join("Description.qml").is_file()
}
