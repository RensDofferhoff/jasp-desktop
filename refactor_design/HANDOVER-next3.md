# HANDOVER — Orchestrator broker: implemented, one deadlock unresolved

**Status:** the new broker (REQ/REP control + per-peer PAIR channels + non-blocking `try_send` +
per-peer backpressure policy) is **implemented and compiles clean** (clippy clean). **7 of 8 unit
tests hang.** One test (`registration_yields_a_usable_channel`) passes. The hang is a specific
deadlock I could not root-cause. This document describes exactly what I observed and ruled out.

Read `orchestrator-design.md` for the settled design. This document is ONLY about the unresolved
deadlock and the current code state.

## 1. What's done and working

- `messages.rs`: `Hello`, `Welcome`, `Activity`, `Deregister` added; `RegisterAck` has
  `channel_url` + `activity_min_interval_ms`. Schema regenerated.
- `main.rs`: full rewrite — REP control endpoint, per-peer PAIR channels via `Aio`, non-blocking
  `try_send` with per-peer backpressure policy (runner → evict on `TryAgain`; frontend → best-effort
  drop), `SENDBUF=RECVBUF=128` on orchestrator channel sockets, `(session_id, work_id)` correlation,
  `pipe_notify` eviction, hang-detector thread.
- Probes in `orchestrator/examples/`: `reqrep_probe` (REQ/REP + framing: PASS), `send_block_probe`
  (inproc PAIR send buffers when peer connected: PASS), `relay_probe` (two-channel relay with inline
  sends: PASS), `tcp_port_probe` (tcp `:0` readback with `u16::from_be` byte-swap fix: PASS),
  `buf_probe` (pair1 default `SENDBUF=RECVBUF=0`: confirmed).
- R runners (`runner_alpha.R`, `runner_jaspbase.R`) migrated to new handshake (REQ/REP + PAIR
  channel). `probe_register_disconnect.R` migrated; cross-language Rust↔R disconnect test passes.

## 2. The unresolved deadlock

### Symptom

`frontend_round_trips_with_correlation` (and 6 other tests that route work) hang for >60s. The test
has `RecvTimeout=5s` on all recv calls, so a hang >60s means the test thread is in a **blocking
send** (no timeout), not a recv.

### Exact debug output (with `--test-threads=1 --nocapture`)

```
[orch] runner registered r-1 channel=inproc://jasp-ch-0-r-1
[pipe] runner=r-1 event=AddPre
[pipe] runner=r-1 event=AddPost
[orch] frontend registered s-2 channel=inproc://jasp-ch-0-s-2
[orch] fe->rn work work_id=w-1 revision=0 session=s-2 runner=r-1
[DBG] route_work: before work.write().insert()
[DBG] route_work: after work.write().insert(), before runner.send()
[DBG] route_work: runner.send() -> Ok(())
```

Then: **hang**. No further output. The test never reaches `runner.recv()`.

### What this tells us

- `route_work` ran on an NNG pool thread (concurrently with the test thread).
- `work.write().insert()` succeeded (no lock deadlock there).
- `runner.send()` (`try_send`) returned `Ok(())` — the work was buffered in the broker's runner
  channel `SENDBUF=128`.
- The test thread is blocked in `frontend.send(work)` (a blocking send with no timeout). The debug
  output from `route_work` (NNG pool thread) appeared concurrently, but the test thread's
  `frontend.send(work)` has not returned.

### What I ruled out

- **Lock deadlock in `route_work`**: ruled out — `work.write().insert()` succeeded (debug print
  after it appeared).
- **`runner.send()` blocking**: ruled out — it's `try_send` (non-blocking), returned `Ok(())`.
- **`RecvTimeout` not applied**: the test thread is in a blocking *send*, not a recv.
- **Runner evicted before work delivered**: ruled out — `runner.send() -> Ok(())` means the work was
  buffered; the runner wasn't evicted.

### The unresolved question

**Why does the test's `frontend.send(work)` (blocking send on inproc PAIR) not return, even though
the broker's frontend channel has `RECVBUF=128` and the broker's frontend channel recv Aio is armed?**

The debug output shows `route_work` ran (NNG pool thread), which means the broker's frontend channel
recv Aio fired and processed the message. So the message WAS received by the broker. So the test's
`frontend.send(work)` should have returned (the message was accepted into the broker's
`RECVBUF=128`). But the test thread is still blocked in `frontend.send(work)`.

### Hypotheses for the next agent (untested)

1. **Blocking send on inproc PAIR blocks until the recv Aio *processes* the message, not just
   buffers it.** If the recv Aio is busy running `route_work` (which does `runner.send()`), and the
   blocking send waits for the recv Aio to finish processing, there's a deadlock: the blocking send
   waits for the recv Aio, and the recv Aio is busy. This would explain why the debug output from
   `route_work` appeared (the recv Aio ran) but the test's send didn't return (it's waiting for the
   recv Aio to finish, but the recv Aio is still running `route_work`). **This is my best guess.**

2. **The test's `frontend.send(work)` and the broker's frontend channel recv Aio are on the same
   NNG I/O thread.** For inproc, the send and the recv Aio callback might run on the same thread. If
   the send blocks waiting for the recv Aio to process, and the recv Aio is on the same thread, the
   thread is blocked in the send and can't run the recv Aio. Deadlock.

3. **`RecvTimeout` doesn't work for inproc PAIR in the nng crate.** If `RecvTimeout` doesn't apply
   to inproc PAIR, then `runner.recv()` blocks forever. But the test thread is in a blocking *send*,
   not a recv, so this doesn't explain the hang directly. (But it might explain why the test doesn't
   panic after 5s.)

### What to try next

- **Add a debug print in the test right before `frontend.send(work)`** to confirm the test thread is
  in `frontend.send(work)` (not somewhere else).
- **Change `frontend.send(work)` to `frontend.try_send(work)`** in the test. If the test then
  proceeds (and the work is delivered), the deadlock is confirmed to be in the blocking send.
- **If the blocking send is the issue**: the test's mock sockets should use non-blocking sends (or
  the test should post a recv before sending). This is a test-side fix, not a broker fix.
- **Alternatively**: investigate whether the broker's frontend channel recv Aio should re-arm
  *before* processing the message (recv → re-arm → process), so the recv Aio is always armed and the
  blocking send can complete immediately.

## 3. Settled design decisions (don't relitigate)

- REQ/REP control endpoint (handshake only), per-peer PAIR channels (data only).
- Non-blocking `try_send` on channel sends; runner → evict on `TryAgain`; frontend → best-effort
  drop (user session, tolerate transient full buffer).
- `SENDBUF=RECVBUF=128` on orchestrator channel sockets; runner/client set 64 on their side.
- `(session_id, work_id)` correlation key; `session_id` stamped by orchestrator on work envelope.
- `pipe_notify` `RemovePost` for eviction (not recv-`Closed`, which doesn't fire for a listening
  PAIR when the peer disconnects).
- inproc PAIR default `SENDBUF=RECVBUF=0` (confirmed by `buf_probe`); peers MUST set `RECVBUF>0`
  for the broker's non-blocking `try_send` to land.
- tcp `:0` readback requires `u16::from_be` byte-swap fix (nng crate bug, confirmed by
  `tcp_port_probe`).
- Abstract IPC not available via nng crate URL API (NUL byte truncated at FFI boundary).

## 4. Current code state

- `orchestrator/src/main.rs`: has debug `[DBG]` prints in `route_work` (remove after debugging).
  Test helpers `register_runner_with_priority` and `hello_frontend` set `RECVBUF=64, SENDBUF=64` on
  the mock channel sockets.
- `orchestrator/src/messages.rs`: complete, schema regenerated.
- `refactor_design/runner_alpha.R`, `runner_jaspbase.R`: migrated to new handshake.
- `refactor_design/probe_register_disconnect.R`: migrated; cross-language disconnect test passes.
- `refactor_design/integration_test.R`: NOT yet migrated (still old two-socket protocol).
- `Desktop/jaspclient/jaspclient.{h,cpp}`: NOT yet migrated (still old PAIR-dial protocol).

## 5. What's next (after the deadlock is resolved)

1. Remove `[DBG]` debug prints from `route_work`.
2. Migrate `integration_test.R` to new handshake.
3. Migrate `Desktop/jaspclient/jaspclient.{h,cpp}` to new handshake.
4. Apply §19.5 spec delta to `neo-jasp.md` (hello drops required `session_id`; welcome gains
   orchestrator-assigned `session_id` on the envelope).
5. Update `orchestrator-design.md` with the three design corrections (sender-thread → try_send;
   abstract IPC unavailable; tcp readback byte-swap).
6. Run full suite + cross-language test + live t-test round trip.
