# Orchestrator Broker/Router — Design & Implementation Spec

**Status:** settled design; implement against this in one coherent pass.
**Scope:** the orchestrator's connection management, registration, per-peer channels,
message routing/correlation, concurrency model, liveness, and failure handling — built
**fully and peer-uniformly** so the broker needs no further architectural rework.

This is the orchestrator's *internal* design. The wire shapes themselves live in
`neo-jasp.md` (§17–§25) and `orchestrator/src/messages.rs` (the single source of truth for
the types; the JSON Schema is generated from them). Where this doc and the spec agree, the
spec is normative; this doc fixes the *implementation* decisions the spec leaves open.

---

## 0. Why this document exists

Repeatedly, load-bearing constraints were discovered *during* implementation (polyamorous
PAIR is deprecated; the AIO concurrency model; per-message locking; IPC stale-socket
cleanup; ephemeral addressing). Each forced a redesign. This document front-loads every such
decision so implementation is mechanical translation, not discovery.

## 1. Goals & non-goals

**Goals**
- One **unified control endpoint** where *every* peer (frontend, runner, future peers) phones
  in to register. One code path for all peers.
- **Per-peer dedicated data channels** (no deprecated polyamorous mode).
- **Transport-agnostic**: runs over `ipc://`, `tcp://`, `inproc://` (tests) by configuration.
- Correct, lock-free-where-possible **concurrency** (NNG async I/O).
- Robust **lifecycle & failure** handling: disconnect eviction, liveness/hang detection.
- **No-silent-loss** routing: work that cannot be served surfaces as a visible error.

**Non-goals (explicitly out of this increment)**
- The data plane (`dataset_open` lane routing, Arrow cache, `data_edit`/`data_update`) — Phase 3.
- Module *discovery contents* (`list_modules`/`modules` payloads) — Phase 4 (the frontend
  *channel* is in scope; the module-list payload is not).
- Runner-internal work-queue reconciliation (`neo-jasp.md` §20) — runner-side, separate work.
- Make-before-break runner rotation, memory-based pool sizing — deferred.
- Full reconnect idempotency, auth/security (§27) — deferred/best-effort.

## 2. Transport model (transport-agnostic)

### 2.1 The two layers
- **Control endpoint:** one well-known address, configured by `JASP_ORCH_URL`
  (e.g. `ipc:///run/jasp/orch.sock`, `tcp://127.0.0.1:9555`, or `inproc://…` in tests). The URL
  embeds the protocol version for *discovery* (§17.2); *validation* is the `v` field in the
  handshake (alpha: accept `v == 1`, reject otherwise).
- **Data channels:** one dedicated channel per peer, allocated by the orchestrator at handshake
  and communicated in the ack. The channel's transport **matches the control endpoint's scheme**.

### 2.2 Scheme-aware channel allocation
The allocator keys off the control URL's scheme so the design never hardcodes a transport:

| Control scheme | Channel allocation | Notes |
|---|---|---|
| `tcp://host:port` | `tcp://host:0` → read back the OS-assigned ephemeral port via the `LocalAddr` option → `tcp://host:<port>` | Ephemeral ⇒ never collides, no stale state. |
| `ipc://path` | a **unique** path per channel (e.g. `<runtime_dir>/jasp-ch-<id>-<rand>.sock`); **abstract** socket name on Linux as the no-filesystem, auto-cleaned default | Unique per allocation ⇒ no stale-file collision for channels. |
| `inproc://name` | `inproc://jasp-ch-<id>` | Tests only (same-process). |

**Stale-socket handling (control endpoint only).** Channels are unique-per-allocation, so they
never collide. The only *fixed* address is the control endpoint, so it is the only stale-file
risk (a hard crash can leave an IPC file; the next `listen` then fails `EADDRINUSE`). Strategy:
- **Linux IPC:** use an **abstract** socket name for the control endpoint — no filesystem
  presence, OS auto-cleans on process death. Zero stale-file problem.
- **Portable IPC fallback:** on `listen` failure with `EADDRINUSE`, probe-connect the path; if
  nothing answers, `unlink` the stale file and retry once; else fail loudly.
- **TCP / named-pipes (Windows):** the OS releases the address on process death; no file to clean.

### 2.3 Discovery (minimal for alpha)
`JASP_ORCH_URL` is the rendezvous. The "second frontend probes the well-known URL to join an
existing backend" behavior (§17.2) is a thin refinement: a frontend that can connect joins; one
that cannot may start a backend. Not built in this increment; the address is configured.

## 3. Peer model & connection lifecycle (uniform for all peers)

### 3.1 Protocol choice
- **Control endpoint: REQ/REP.** Peers open a **REQ** socket, dial the control endpoint, send
  their handshake, and receive the reply. The orchestrator runs one **REP** socket. REP accepts
  many clients on one socket and auto-routes each reply to its requester — exactly the
  "many peers phone in, get an ack" pattern — and it is a core, **non-deprecated** pattern.
  *(PAIR v1 is 1:1 and cannot serve many peers; polyamorous PAIR is deprecated. REQ/REP is the
  clean answer.)*
- **Data channels: PAIR v1 (mono).** One 1:1 channel per peer; all data flows here.

### 3.2 Handshake (all peers connect the same way — §17.2)
```
peer                                         orchestrator (REP on JASP_ORCH_URL)
  |-- REQ dial JASP_ORCH_URL -------------------->|
  |-- hello{v,client_id?,…} | register{v,…} ----->|  validate v; allocate channel; assign ids
  |<-- welcome{ok,session_id,channel_url,…} ------|  (REP auto-routes the reply to this peer)
  |    | register_ack{ok,runner_id,channel_url} --|
  |  (close REQ)                                  |
  |-- PAIR dial channel_url --------------------->|  orchestrator already listens on channel_url
  |<============ work / result / abort ==========>|  (per-peer PAIR channel, AIO-driven)
```
- **Frontend** sends `hello{v, client_id?, client_version?}` — its `client_id` is an optional
  **stable** hint for reconnect idempotency (the analogue of a runner's `runner_id` hint). The
  orchestrator is the **authority on identity**: it **assigns the canonical `session_id` (`s-N`)**
  and replies `welcome{ok, session_id, channel_url}` (and, later, `modules`). This mirrors runner
  registration exactly — the peer offers a hint; the orchestrator assigns the canonical id.
- **Runner** sends `register{capabilities, priority, environment, …}`; orchestrator assigns the
  canonical `runner_id` (`r-N`), allocates a channel, replies `register_ack{runner_id, channel_url,
  activity_min_interval_ms?}`.
- The control connection is **transient**: one request→reply, then the peer leaves the REP socket
  and moves to its PAIR channel. Handshakes are rare and fast; the REP loop processes them
  serially (concurrent handshakes via REP *contexts* is the documented scaling path if ever needed).

### 3.3 Data phase
All `work` / `result` / `abort` (and later `modules`, `data_*`) flow on the peer's PAIR channel,
framed per §18.1 (`[u32 BE len][JSON][binary]`). The control endpoint carries **only** the handshake.

### 3.4 Teardown & reconnect
- **Graceful:** runner sends `deregister`; frontend closes. Orchestrator evicts and closes the channel.
- **Ungraceful:** the peer's PAIR channel closes → the channel's AIO recv returns `Closed` →
  evict (runner) / drop (frontend) and clean up outstanding work (§6.5, §7).
- **Reconnect (best-effort):** a peer simply re-handshakes and gets a new channel. Idempotent
  re-attach by `client_id`/`runner_id` hint (§19.5) is deferred; reconnect yields a fresh identity.

## 4. Data structures

All orchestrator-wide state lives in one `Arc<Broker>` captured by every AIO callback.

```rust
struct Broker {
    // Cold, structural state — behind RwLocks (read-heavy; written on lifecycle events).
    runners:   RwLock<HashMap<String /*runner_id*/, Arc<RunnerRuntime>>>,
    frontends: RwLock<HashMap<String /*session_id*/, Arc<FrontendRuntime>>>,
    work:      RwLock<HashMap<(String, String) /*(session_id, work_id)*/, WorkRoute>>, // §6
    counter:   AtomicU64,                                           // r-N / s-N assignment (lock-free)
    config:    Config,                                              // urls, timeouts, dataset path
}

struct RunnerRuntime {
    channel:      Socket,            // this runner's PAIR data channel (the routing target)
    capabilities: Vec<Capability>,
    priority:     u32,
    seq:          u64,               // registration order; recency tie-break (§25.6)
    managed:      bool,              // true = orchestrator-spawned (killable); false = attached
    // Hot state — lock-free atomics, updated on every message (§5, §7):
    last_activity:  AtomicU64,       // epoch-ms of last inbound message
    outstanding:    AtomicUsize,     // in-flight work count (for hang detection)
}

struct FrontendRuntime {
    channel:    Socket,              // this frontend's PAIR data channel
    session_id: String,              // orchestrator-assigned scope key (s-N)
    client_id:  Option<String>,      // stable client hint for reconnect resume (§3.4); mapping deferred
}

struct WorkRoute {
    frontend: Arc<FrontendRuntime>,  // where results for this work go
    runner:   Arc<RunnerRuntime>,    // who is running it
    revision: u64,                   // stale-result guard (§23)
}
```

**Key separation (the thing that keeps the hot path lock-free):**
- **Cold/structural** state (membership, capabilities, correlation) lives in `RwLock` maps — read
  on routing, written only on lifecycle events (register/disconnect/work-start/work-end).
- **Hot/per-message** state (`last_activity`, `outstanding`) lives in **atomics inside the
  `Arc<Runtime>`**. A peer's own channel callback holds that `Arc` and bumps the atomic with
  **no map lock at all**.

### 4.1 Capability matching & runner selection (§9.4, §25.6)
`select_runner(&runners, payload) -> Option<Arc<RunnerRuntime>>`:
- **Analysis work** `module@version` → runners advertising `Capability::Analysis{name,version}`
  with matching `name`. Version policy: exact match preferred; else the configured policy (best
  available, recording the producing version, or fail `version_mismatch`). Alpha: match by name,
  prefer exact version.
- **Rcode work** → any runner advertising `Capability::Rcode{}`.
- Among matches: **highest `priority`** wins; tie → **most recent registration** (highest `seq`).
- Returns the chosen `Arc<RunnerRuntime>` (clone), or `None`.

## 5. Concurrency model

### 5.1 NNG async I/O (`Aio`), NNG's thread pool
We use the safe crate's `Aio` (no FFI). NNG runs an internal task thread pool over the OS event
mechanism (`epoll`/`kqueue`/IOCP — cross-platform, NNG-owned). We register a callback per socket;
NNG invokes it when that socket is ready. **We own zero per-peer threads and write no poll loop.**

The verified `Aio` mechanics (from the crate source):
- `Aio::new(|aio: Aio, res: AioResult| { … })` — callback is `Fn(Aio, AioResult) + Send + Sync + 'static`.
- NNG hands the callback an **owned `Aio` clone**; re-arm by calling `socket.recv_async(&aio)` with it.
- `Aio` is `Arc`-backed: **keep at least one strong `Aio` alive** (stored in the Runtime/Broker);
  the trampoline holds only a weak ref, so dropping all strong `Aio`s stops the loop.
- `AioResult::Recv(Ok(msg))` → the message; `Recv(Err(e))` → close/error (our eviction path).
- **Callbacks may run concurrently** on different pool threads (documented). Hence §5.2.
- **Never panic in a callback** (the crate aborts on panic). All errors handled, no `unwrap` on
  peer-driven data.

### 5.2 The locking discipline
- **Lock → clone the `Arc` you need → drop the lock → then do I/O.** Never hold a lock across a
  socket send/recv. Avoids races *and* lock starvation.
- **Hot path is lock-free:** a channel callback bumps `Runtime.last_activity` / `outstanding`
  (atomics) and forwards via a socket it already holds — **no map lock per message.**
- **Map locks are rare & mostly read:** routing takes a *read* lock on `runners` (concurrent);
  register/disconnect take *write* locks (rare); the hang-detector takes a *read* lock to snapshot
  the `Arc`s, then inspects atomics lock-free.
- Sockets are `Send + Sync`; concurrent sends to the frontend from multiple callbacks are safe.

### 5.3 The callback inventory
| Socket | Callback responsibility |
|---|---|
| control REP | recv `hello`/`register` → validate → allocate channel + arm its AIO → insert Runtime → send `welcome`/`register_ack` → re-arm REP recv |
| each PAIR channel (runner) | recv `result`/`data_update`/… → bump `last_activity` → route (§6) → re-arm; `Closed` → evict |
| each PAIR channel (frontend) | recv `work`/`abort`/… → bump `last_activity` → route (§6) → re-arm; `Closed` → drop frontend, fail its outstanding work |

## 6. Message routing & correlation

**`work_id` is session-scoped, not globally unique.** A client chooses its own `work_id`s freely
(e.g. `a0`); two different sessions may both use `a0` without collision, because the *session* is
the namespace. Therefore the orchestrator's routing/correlation key is the compound
**`(session_id, work_id)`**, and the `work` table is keyed on it. The runner does not need to know
the session: when routing a work, the orchestrator stamps the envelope's `session_id` (it knows it
from the source channel) so that `(session_id, work_id)` is carried end-to-end and a `result` is
unambiguous. *(Alternative considered: orchestrator mints an internal globally-unique work handle
and maps it to `(session_id, work_id)`; rejected as needless indirection — carrying the compound key
is simpler and keeps the client's own `work_id` visible throughout.)*

`work_table: RwLock<HashMap<(String /*session_id*/, String /*work_id*/), WorkRoute>>` — the
correlation table. Read-heavy (every result looks it up), written on work-start/work-end; the
`RwLock` keeps result lookups concurrent.

### 6.1 Work (frontend → runner)
On a frontend channel recv `work`:
1. Read-lock `runners`, `select_runner(payload)` → clone `Arc<RunnerRuntime>`; drop lock.
2. If `None` **and any runner has ever registered** → send `fatalError` result back to this
   frontend (**no silent loss**, §25.5). If no runner ever registered → (alpha leniency) drop.
3. Stamp the envelope `session_id` (from the source channel's `FrontendRuntime`) so the work carries
   the compound key `(session_id, work_id)` to the runner and back.
4. Inject runner-facing fields (§19.3): absolute `dataset_paths`, per-work `output_dir`
   (§6.4 path layout — already session/work namespaced).
5. Record `work[(session_id, work_id)] = WorkRoute{frontend, runner, revision}` (write-lock).
6. `runner.outstanding += 1`; `runner.channel.send(work)`.

### 6.2 Result (runner → frontend)
On a runner channel recv `result` (carrying `session_id` + `work_id`):
1. `runner.last_activity.store(now)`; on a terminal status, `runner.outstanding -= 1`.
2. Read-lock `work`, look up `(session_id, work_id)` → `WorkRoute`; drop lock.
3. Forward the result verbatim to `workroute.frontend.channel` (the jaspResults tree is opaque).
4. On terminal status (`complete`/`fatalError`/…), remove the entry (write-lock).
- **Stale guard (§23):** if `result.revision < workroute.revision`, drop it (superseded).

### 6.3 Abort (frontend → runner)
On a frontend channel recv `abort{work_id}`: the frontend's `session_id` is known from the channel;
read-lock `work` on `(session_id, work_id)` → `WorkRoute.runner` → forward `abort` to that runner's
channel. (Interrupt semantics are the runner's problem, §7.6.)

### 6.4 Path layout (session/work namespacing)
`session_id` scopes the filesystem so two clients never collide. All orchestrator-managed paths are
namespaced `/<root>/<session_id>/<work_id>/…`:
- **scratch** (per-work temp): `<scratch_root>/<session_id>/<work_id>/` — this is the `output_dir`
  injected into the work (§19.3).
- **results** (per-work artifacts): `<results_root>/<session_id>/<work_id>/`.
- **dataset cache** (per-session, §4.9): `<dataset_cache_root>/<session_id>/<dataset_id>` (Feather).
Roots are config (desktop: under the user runtime/cache dir). `session_id` is orchestrator-minted
(`s-N`, alphanumeric), so these paths are safe by construction — no client-supplied path component.

### 6.5 Failure forwarding
If a runner disconnects with outstanding work, each affected `(session_id, work_id)` gets a
`fatalError` result sent to its frontend (`"runner disconnected"`), and the entry is removed. Work
is never lost into the void.

## 7. Liveness & failure (§25.4)

Two failure modes, handled separately:
- **Death (crash/exit):** the peer's PAIR channel closes → its AIO recv returns `Closed` → evict
  (runner) / drop (frontend) → fail outstanding work (§6.5). Prompt on IPC and TCP; no timeout.
- **Hang (alive but wedged):** each runner's `last_activity: AtomicU64` is stamped on **every**
  inbound message (a `result` *is* an activity signal; an opportunistic `activity` message —
  §19.4, rate-limited by `activity_min_interval_ms` — covers busy-but-quiet). A periodic
  **hang-detector tick** (a timer callback or a dedicated slow loop) read-locks `runners`,
  snapshots the `Arc`s, and for each runner with `outstanding > 0` and
  `now - last_activity > busy_hang_timeout`: **managed** → SIGKILL + mark for re-run on a fresh
  runner; **attached** → surface "appears hung." Idle runners need no activity (death is caught by
  channel close; a wedged idle runner is caught on its next work assignment).

`busy_hang_timeout` and `activity_min_interval_ms` are config; defaults generous (the fundamental
R "wedged in C" limit, §7.6, is mitigated, not solved).

## 8. Configuration & defaults

| Env | Meaning | Default |
|---|---|---|
| `JASP_ORCH_URL` | control endpoint (scheme selects transport) | `ipc://` abstract (Linux) / `tcp://127.0.0.1:9555` fallback |
| `JASP_ORCH_TEST_DATASET` | alpha dataset path injected into work | `test_data/debug.arrow` (made absolute) |
| `JASP_ORCH_HANG_TIMEOUT_MS` | `busy_hang_timeout` | generous (e.g. 30000) |
| `JASP_ORCH_ACTIVITY_MIN_MS` | suggested `activity` rate-limit advertised to runners | 1000 |
| `JASP_RUNNER_VERBOSE` | runner-side option dump (existing) | off |

`cargo run -- --schema` still prints the generated JSON Schema from `messages.rs`.

## 9. Client handshake contract (what peers must do)

Pointers to the normative shapes in §17.2/§19.5; the orchestrator implements its side:
- **Runner:** REQ-dial control → send `register{v, runner_id?, capabilities, …}` → recv
  `register_ack{runner_id, channel_url}` → close REQ → PAIR-dial `channel_url` → recv `work`, send
  `result`/`data_update`/`activity`. (`runner_jaspbase.R`, `runner_alpha.R` updated accordingly.)
- **Frontend:** REQ-dial control → send `hello{v, client_id?}` → recv `welcome{session_id, channel_url}`
  → close REQ → PAIR-dial `channel_url` → send `work`/`abort`, recv `result`. (`integration_test.R`
  and the C++ `JaspClient` updated accordingly.)

## 10. Migration from the current alpha

Current: two shared PAIR sockets — frontend `:9555`, runner `:9556` — the runner socket
polyamorous-by-registry, broadcast routing.
New: **one REP control endpoint + per-peer PAIR channels**, targeted routing by socket.

Concrete changes:
- `messages.rs`: add `channel_url` (+ `activity_min_interval_ms?`) to `RegisterAck`; add
  `Hello`/`Welcome`; add `Activity`; `session_id` already on the envelope. Regenerate schema.
- `main.rs`: replace the two-socket broker with: REP control endpoint + scheme-aware channel
  allocator + per-peer PAIR channels via `Aio` + `Arc<Broker>` (RwLock maps + atomics) + routing
  (§6) + hang-detector tick (§7).
- Clients (§9): `runner_jaspbase.R`, `runner_alpha.R`, `integration_test.R`, C++ `JaspClient`.

## 11. Test plan & invariants

Unit (Rust, inproc, mock REQ/PAIR peers):
1. **Registration → channel:** a runner REQ-registers, receives `register_ack` with a usable
   `channel_url`, dials it, and the orchestrator tracks it.
2. **Two-runner targeted routing:** two runners advertise different modules; a `work` for module X
   reaches *only* runner X (the other's channel recv times out). Priority + recency tie-breaks.
3. **Frontend channel + correlation:** a frontend hellos, gets a channel, submits work; the result
   returns on *its* channel with matching `work_id`/`revision`.
4. **Disconnect eviction:** drop a runner's channel → registry evicts; outstanding work → `fatalError`
   to the frontend (no silent loss); a fresh `work` for the dead runner errors.
5. **Liveness:** stamp `last_activity`; with `outstanding>0` and a stale timestamp the hang-detector
   flags the runner.
6. **Transport-agnostic:** the same routing test over a `tcp://` control endpoint (ephemeral channels).

Integration / live:
- `integration_test.R` updated to the handshake → **16/16** (or its updated equivalent) green.
- Cross-language Rust↔R disconnect test (existing `r_runner_disconnect`, adapted to the handshake).
- Live round trip: real JASP → control handshake → channel → `runner_jaspbase` → real t-test table.

## 12. Open / deferred (explicit)
- **Spec delta (§19.5):** `hello` no longer carries a required `session_id`; the orchestrator
  **assigns** `session_id` (`s-N`) in `welcome`, mirroring `runner_id` in `register_ack`. The
  frontend's optional `client_id` is the stable reconnect hint. Update `neo-jasp.md` §19.5 to match.
- Version-handshake depth (alpha: accept `v==1`).
- Reconnect idempotency by `client_id`/`runner_id` hint.
- Module-discovery *payloads* (`list_modules`/`modules`) — Phase 4.
- `data_*` routing + Arrow cache — Phase 3.
- Make-before-break rotation; memory-based pool sizing.
- Concurrent handshakes via REP contexts (only if handshake rate ever matters).
- auth/security (§27).
