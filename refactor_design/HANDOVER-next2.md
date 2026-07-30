# HANDOVER — Orchestrator Broker/Router implementation (start here)

**Status:** the broker/router is fully **designed** (`orchestrator-design.md`, settled) but **not yet
implemented**. The current alpha broker (two shared PAIR sockets, broadcast routing) still works and
all tests are green — it will be **replaced** by the design below. This is the focused brief for the
agent who implements the new broker. `HANDOVER-next.md` carries the broader project history.

## 1. What we are doing

Replace the alpha's two-socket broadcast broker with the **per-channel, transport-agnostic,
event-driven broker** specified in `orchestrator-design.md`. Build it **fully and peer-uniformly**
(frontends AND runners) so it needs no further architectural rework, then migrate the clients
(R runners, `integration_test.R`, C++ `JaspClient`) to the new handshake.

**Read `orchestrator-design.md` in full first** — it is the implementation spec. This file is the
orientation, the decisions we argued through, the build order, and the hard-won gotchas.

## 2. The design in one breath

- **One unified control endpoint** (well-known URL, `JASP_ORCH_URL`): every peer phones in here.
  Implemented as **REQ/REP** (peers = REQ, orchestrator = REP) because PAIR is 1:1 and poly PAIR is
  deprecated. The control endpoint carries **only the handshake**.
- **Per-peer PAIR v1 data channels**: allocated at handshake, handed back in the ack
  (`register_ack{channel_url}` / `welcome{channel_url}`). All `work`/`result`/`abort` flows here.
- **Transport-agnostic**: channel allocation is scheme-aware — `tcp://` → ephemeral `:0` port,
  `ipc://` → unique path (abstract socket on Linux = no stale file), `inproc://` for tests.
- **Concurrency**: NNG `Aio` callbacks on NNG's cross-platform thread pool. No per-peer threads, no
  hand-rolled poll, no FFI. **Atomics** for hot per-message state (`last_activity`, `outstanding`);
  **`RwLock`** for cold structural state (registries, correlation table). Lock → clone Arc → drop → I/O.
- **`session_id`** is orchestrator-assigned (`s-N`), the scope of one client — namespaces
  client-chosen `work_id`s, datasets, and scratch/results dirs. Frontend sends only an optional
  stable `client_id` (reconnect hint). Correlation key is **`(session_id, work_id)`**.
- **Liveness**: death = channel close → evict; hang = `last_activity` atomics + `busy_hang_timeout`
  tick (managed → SIGKILL + rerun, attached → surface "hung").

## 3. Key decisions (argued through — don't relitigate unless you find a real problem)

1. **Per-channel PAIR, NOT poly.** Polyamorous PAIR (the crate's `Polyamorous` option /
   `nng_pair1_open_poly`) is **deprecated**; NNG removes it once a Mesh pattern lands. Per-channel is
   the durable choice and matches spec §17.2.
2. **Control endpoint is REQ/REP, NOT PAIR.** PAIR is 1:1 — it cannot accept many peers phoning in.
   REQ/REP is the non-deprecated "many clients, one server, reply auto-routed" pattern, and a one-shot
   handshake fits its request→reply model exactly. Consequence: the control endpoint is
   handshake-only; ALL ongoing traffic lives on the per-peer channels.
3. **`session_id` is orchestrator-assigned, not client-generated.** Its purpose is collision-avoidance
   (it namespaces client-chosen `work_id`s, datasets, workdirs), so the authority that enforces the
   separation must mint it and guarantee uniqueness. It also flows into filesystem paths, so a
   server-minted alphanumeric id is injection-safe by construction. The client's stable identity is
   `client_id` (reconnect resume). Mirrors `runner_id` assignment.
4. **Correlation key is `(session_id, work_id)`.** `work_id` alone is NOT globally unique — two
   clients may both use `a0`. The work-table is keyed on the pair; the orchestrator stamps
   `session_id` onto the work envelope so the pair round-trips through the runner.
5. **Hot state = atomics, cold state = RwLock.** `last_activity`/`outstanding` are per-runner atomics
   bumped with **no lock** on the per-message hot path (each runner's channel callback holds its
   `Arc<RunnerRuntime>` directly). Registries/work-table are `RwLock` (read-heavy: routing reads,
   lifecycle writes). Avoids a per-message write-lock bottleneck.
6. **Frontend channels are IN SCOPE now** (not deferred to Phase 4) — the model is peer-uniform, so
   the increment includes migrating the C++ `JaspClient` + `integration_test.R` to the handshake.
7. **Transport-agnostic** (the user's call — NNG's core strength). No IPC-only lock-in; abstract IPC
   is the Linux optimization, not an assumption.

## 4. What's already done & verified (do not break)

- **Phase 1 tidy** (HANDOVER-next.md): absolute dataset paths, per-work `output_dir`, honest cwd,
  `JASP_RUNNER_VERBOSE` gate. (`output_dir` becomes session-namespaced under the new design — §6.4 —
  subsuming this.)
- **Arrow thin slice**: orchestrator injects `test_data/debug.arrow`; `runner_jaspbase.R` reads it via
  `jaspRunner/R/data.R::read_jasp_data()`; `.typeDataset` deleted; live t-test round trip verified.
- **Current alpha broker**: two PAIR sockets (`:9555` frontend, `:9556` runner), registry keyed by
  `Pipe`, `pipe_notify` eviction, broadcast routing. Rust tests 5/1, integration test 16/16,
  cross-language Rust↔R disconnect test green. **This is what gets replaced.**

## 5. Build plan (ordered)

1. **Probes first** — de-risk the mechanics the design leans on; if a probe contradicts the design,
   fix the design on paper before coding. Write each as `orchestrator/examples/*.rs`, run via
   `cargo run --manifest-path orchestrator/Cargo.toml --example <name>`:
   - **REQ/REP + §18.1 framing**: a REQ peer sends a framed `[u32 len][JSON]` envelope to a REP
     server and gets a framed reply — confirm REQ/REP cooked mode carries our payload transparently.
   - **Abstract IPC**: confirm the abstract-socket URL syntax in the Rust `nng` crate and that it
     leaves **no stale file** after a crash on this Linux box.
   - **Aio keep-alive/re-arm**: a PAIR socket whose Aio callback re-arms via `recv_async(&aio)`;
     confirm the loop continues across multiple messages and stops when the stored `Aio` is dropped.
2. **`messages.rs`**: add `channel_url` (+ `activity_min_interval_ms?`) to `RegisterAck`; add
   `Hello`/`Welcome`; add `Activity`; `session_id` already on the envelope. Regenerate the schema
   (`cargo run -- --schema`). Apply the §19.5 spec delta (hello drops required `session_id`;
   `welcome` gains orchestrator-assigned `session_id`) to `neo-jasp.md`.
3. **`main.rs`**: replace the two-socket broker with the design — REP control endpoint + scheme-aware
   channel allocator + per-peer PAIR channels via `Aio` + `Arc<Broker>` (`RwLock` maps + atomics) +
   routing/correlation (§6, keyed `(session_id, work_id)`) + hang-detector tick (§7).
4. **Migrate clients** to the handshake (§9): `runner_jaspbase.R`, `runner_alpha.R`,
   `integration_test.R`, C++ `JaspClient`.
5. **Verify**: Rust unit tests (two-runner targeted routing, correlation, disconnect eviction,
   no-silent-loss, liveness, transport-agnostic over TCP), integration test (updated handshake),
   cross-language Rust↔R, and the live JASP → t-test round trip.

## 6. Key files

- `refactor_design/orchestrator-design.md` — **THE implementation spec** (read first).
- `refactor_design/neo-jasp.md` — normative wire-protocol spec (§17–§25); §19.5 has a pending delta.
- `refactor_design/HANDOVER-next.md` — broader project history.
- `orchestrator/src/{main.rs, messages.rs}` — the code to change.
- `jaspRunner/R/data.R` — Arrow read path (untouched by this increment).
- `refactor_design/{runner_jaspbase.R, runner_alpha.R, integration_test.R}` — clients to migrate.
- `Desktop/jaspclient/{jaspclient.h,cpp}` — C++ client to migrate.

## 7. Gotchas (hard-won — don't rediscover)

- **PAIR v1 is 1:1** — a single PAIR socket accepts ONE peer. A many-peer endpoint MUST be REQ/REP
  (or deprecated poly). This is why the control endpoint is REQ/REP.
- **Polyamorous PAIR is deprecated** (`Polyamorous` option / `nng_pair1_open_poly`). Don't use it.
- **Aio keep-alive**: the `Aio` from `Aio::new` must be **stored** (the trampoline holds only a weak
  ref); drop all strong `Aio`s and the loop silently stops. Callback signature is
  `Fn(Aio, AioResult) + Send + Sync + 'static`; NNG hands the callback an owned `Aio` clone — re-arm
  with `socket.recv_async(&aio)`. `AioResult::Recv(Ok(msg))` / `Recv(Err(Closed))`.
- **Callbacks run concurrently** on NNG's pool (documented). Hence atomics for hot state; never hold
  a lock across a socket op (lock → clone Arc → drop → I/O). **Never panic in a callback** (aborts).
- **`session_id` minted by the orchestrator**; `work_id` is client-chosen and session-scoped; the
  correlation key is `(session_id, work_id)`.
- **Transport**: TCP leaves no stale file on crash; IPC filesystem paths do (use abstract sockets on
  Linux, or unlink-on-EADDRINUSE). inproc is same-process only (tests).
- The Rust `nng` crate exposes `Aio`, `Socket::recv_async`/`send_async`, `pipe_notify`,
  `Message::set_pipe` in the **safe** API — **no FFI needed**.

## 8. First commands

```sh
cargo test --manifest-path orchestrator/Cargo.toml                          # current: 5 passed, 1 ignored
cargo test --manifest-path orchestrator/Cargo.toml -- --ignored r_runner_disconnect   # cross-lang
Rscript refactor_design/integration_test.R                                  # with orchestrator up: 16/16
```

Then write the probes (§5.1), confirm the mechanics, then implement §5.2–§5.5.
