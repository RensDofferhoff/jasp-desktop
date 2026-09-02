//! Shared orchestrator-side NNG plumbing — extracted from classic's `main.rs`
//! (orchestrator-v2 design §11 rule 1: *extract, don't fork*), so the hard-won
//! NNG traps live in exactly one place and both orchestrators use them:
//!
//! * **`NNG_OPT_RECVMAXSIZE` silently discards** oversized messages (libnng's
//!   ~1 MiB default; neo-jasp §25.5) — every channel we allocate raises the
//!   ceiling to `max_inline_payload + RECV_MARGIN`.
//! * **A dialed PAIR does not error its recv when the listener dies** — hence
//!   `pipe_notify` + `PipeEvent::RemovePost` for disconnect detection (a
//!   *listening* PAIR's recv does not return `Closed` when the peer leaves
//!   either).
//! * **Pre-connect send buffering** — a PAIR send to a not-yet-dialed peer
//!   buffers and returns immediately (the catalog-as-first-frame and
//!   dispatch-on-registration mechanisms rely on it).
//! * **The nng 1.0.1 readback-port endianness quirk** — see [`readback_url`].
//!
//! The callbacks here run on NNG's pool threads and do NO routing (P1): they
//! recv, re-arm (always — so the socket stays receptive and a peer's blocking
//! send can always land in RECVBUF), then forward over the injected closures,
//! which are expected to be non-blocking mailbox sends.

use crate::framing::deframe_parts;
use crate::{Envelope, Message};
use nng::options::{LocalAddr, Options, RecvBufferSize, RecvMaxSize, SendBufferSize};
use nng::{Aio, AioResult, Listener, Pipe, PipeEvent, Protocol, Socket};
use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

/// Orchestrator-side outbound channel buffer (messages, orch → peer). This is the cushion
/// that absorbs a slow or bursty peer — most importantly a frontend whose UI thread has
/// momentarily stalled — before `try_send` returns `TryAgain`. Deeper = more tolerance for
/// transient peer silence; the backpressure policy (runner → evict, frontend → drop)
/// decides what a full buffer *means*. Runners/clients set 64 on their side.
pub const ORCH_SEND_BUF: i32 = 256;
/// Orchestrator-side inbound channel buffer (messages, peer → orch). The recv `Aio` drains
/// this near-instantly (it only re-arms and enqueues to the router), so a shallow depth is
/// ample.
pub const ORCH_RECV_BUF: i32 = 128;

/// Read back the address a listener actually bound, as a dialable URL — the single source
/// of truth for "what do peers dial?" For inproc/ipc this is the (unique) address we asked
/// for; for tcp it is the OS-assigned ephemeral `:0` port.
///
/// Corrects a one-byte quirk in nng 1.0.1 (probe-verified): its `From<nng_sockaddr>`
/// converts the address from network order but passes the port through raw, whereas NNG
/// stores `sa_port` in network order and `SocketAddrV4/V6::new` expect host order — so the
/// readback port is endian-flipped on little-endian hosts. `u16::from_be` is the portable
/// network→host fix (a no-op on big-endian, a swap on little-endian).
pub fn readback_url(listener: &Listener) -> Result<String, nng::Error> {
    Ok(match listener.get_opt::<LocalAddr>()? {
        nng::SocketAddr::Inet(v4) => format!("tcp://{}:{}", v4.ip(), u16::from_be(v4.port())),
        nng::SocketAddr::Inet6(v6) => format!("tcp://[{}]:{}", v6.ip(), u16::from_be(v6.port())),
        // inproc / ipc: the crate's Display already renders a dialable url.
        other => format!("{other}"),
    })
}

/// Allocate a dedicated PAIR v1 data channel and return its `(socket, dialable_url)`.
///
/// Uniform across transports: bind a candidate address with `Listener::new`, then use the
/// address the OS *actually* bound as the dialable URL. For inproc/ipc the candidate is a
/// unique name/path we construct (the per-broker `nonce` keeps it collision-free across
/// brokers in one process, since inproc names are process-global) and the dialable URL is
/// the candidate itself. For tcp the candidate is `host:0`, so the OS assigns a
/// guaranteed-free ephemeral port which we read back via [`readback_url`]. No port ranges,
/// no retry loops, and no transport-specific code beyond the candidate-address syntax.
///
/// Raises `RecvMaxSize` to `recv_max` (§18.4/§4.4) — libnng's ~1 MiB default *silently
/// discards* larger messages, and view chunks (~20 MB) plus any future bulk ride these
/// channels.
pub fn allocate_channel(
    scheme: &str,
    tcp_host: &str,
    nonce: u64,
    id: &str,
    recv_max: usize,
) -> Result<(Arc<Socket>, String), nng::Error> {
    let sock = Socket::new(Protocol::Pair1)?;
    let url = match scheme {
        "inproc" => {
            let url = format!("inproc://jasp-ch-{nonce}-{id}");
            let listener = Listener::new(&sock, &url)?;
            let _ = listener; // socket keeps the listener alive; handle is Copy
            url
        }
        "ipc" => {
            let url = format!(
                "ipc:///tmp/jasp-ch-{nonce}-{id}-{}.sock",
                std::process::id()
            );
            let listener = Listener::new(&sock, &url)?;
            let _ = listener;
            url
        }
        // tcp: delegate the port to the OS (`:0`); the readback tells us which port we got.
        _ => {
            let listener = Listener::new(&sock, &format!("tcp://{tcp_host}:0"))?;
            readback_url(&listener)?
        }
    };
    // Share the socket between the Aio recv loop and direct sends.
    // Orchestrator-side buffers: the outbound SENDBUF is the backpressure cushion for a
    // slow peer (256 — tolerates a stalled frontend / bursty results); the inbound RECVBUF
    // stays shallow (128) because the recv Aio drains it immediately. When SENDBUF fills,
    // `try_send` returns `TryAgain` and the per-peer policy applies (runner → evict,
    // frontend → drop).
    sock.set_opt::<SendBufferSize>(ORCH_SEND_BUF)?;
    sock.set_opt::<RecvBufferSize>(ORCH_RECV_BUF)?;
    sock.set_opt::<RecvMaxSize>(recv_max)?;
    let channel = Arc::new(sock);
    Ok((channel, url))
}

/// Arm a peer data channel's recv loop (the P1 callback shape).
///
/// * `on_msg(env, binary)` — a deframed message arrived; the §18.1 binary tail rides
///   along verbatim (view TSV, edit cells, inverse IPC bytes — never through the JSON
///   parser). Called on an NNG pool thread: it must be a non-blocking mailbox send.
/// * `on_disconnect()` — the peer's pipe was removed, or the channel errored: evict.
///
/// The recv is re-armed BEFORE forwarding so the socket is always receptive. The caller
/// owns the returned `Aio` (the trampoline holds only a weak ref — dropping it stops the
/// loop); the canonical keep-alive is the router's `aios` map.
pub fn arm_peer_channel(
    channel: &Arc<Socket>,
    on_msg: Box<dyn Fn(Envelope, Vec<u8>) + Send + Sync + 'static>,
    on_disconnect: Box<dyn Fn() + Send + Sync + 'static>,
) -> Result<Aio, nng::Error> {
    let on_disconnect = std::sync::Arc::new(on_disconnect);
    let on_disconnect_pn = Arc::clone(&on_disconnect);
    channel.pipe_notify(move |_pipe: Pipe, event: PipeEvent| {
        if matches!(event, PipeEvent::RemovePost) {
            on_disconnect_pn();
        }
    })?;
    let ch = Arc::clone(channel);
    let on_disconnect_aio = Arc::clone(&on_disconnect);
    let aio = Aio::new(move |aio: Aio, res: AioResult| match res {
        AioResult::Recv(Ok(msg)) => {
            // Forward BEFORE re-arming: the mailbox send is non-blocking (unbounded
            // mpsc), and completing completions on different pool threads may race —
            // forwarding first preserves WIRE ORDER in the router's total order (P1),
            // which supersession and FIFO dispatch depend on.
            if let Some((env, binary)) = deframe_parts(&msg[..]) {
                on_msg(env, binary.to_vec());
            } else {
                // Never-swallow (the 2026-08-31 UI smoke-test lesson): an unparseable
                // frame would otherwise vanish with NO trace on either side. A missing
                // required field (id, work_id) is the classic cause; log it loudly so
                // the seam bug shows itself.
                eprintln!(
                    "[orch] peer sent an undecodable frame ({} bytes) — dropped \
                     (missing required fields? id / work_id)",
                    msg.len()
                );
            }
            // Re-arm AFTER forwarding so the socket is always receptive again.
            if let Err(e) = ch.recv_async(&aio) {
                eprintln!("[orch] channel re-arm failed: {e}");
            }
        }
        AioResult::Recv(Err(e)) => {
            println!("[orch] channel closed: {e}");
            on_disconnect_aio();
        }
        _ => {}
    })?;
    channel.recv_async(&aio)?;
    Ok(aio)
}

/// A `bad_frame` error envelope (control-endpoint replies must always answer a REQ).
pub fn bad_frame_error(reply_to: Option<&str>) -> Envelope {
    Envelope {
        v: 1,
        id: "orch-err".into(),
        reply_to: reply_to.map(|s| s.to_string()),
        session_id: None,
        format: None,
        ts: None,
        body: Message::Error(crate::ErrorMsg {
            code: "bad_frame".into(),
            message: "undecodable handshake".into(),
            work_id: None,
        }),
    }
}

/// Arm the control REP socket's recv loop. The callback forwards each handshake to
/// `on_handshake` (expected: a non-blocking mailbox send + brief wait on a one-shot reply
/// channel — the router thread does the actual work) and sends the reply envelope on the
/// REP socket. REP enforces recv→send alternation, so the reply is sent *then* the recv is
/// re-armed. The caller owns the returned `Aio`.
pub fn arm_control_rep(
    control: &Arc<Socket>,
    on_handshake: Box<dyn Fn(Envelope) -> Envelope + Send + Sync + 'static>,
) -> Result<Aio, nng::Error> {
    let ctl = Arc::clone(control);
    let aio = Aio::new(move |aio: Aio, res: AioResult| match res {
        AioResult::Recv(Ok(msg)) => {
            // REP must reply to every request before the next recv, so always produce one.
            let reply = match crate::framing::deframe(&msg[..]) {
                Some(env) => on_handshake(env),
                None => bad_frame_error(None),
            };
            if let Err((_m, e)) = ctl.try_send(crate::framing::frame_envelope(&reply).as_slice()) {
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
    Ok(aio)
}

/// Listen on the control URL. For IPC, a hard crash can leave a stale socket file; on
/// `AddressInUse` we probe-connect, and if nothing answers, unlink the stale file and
/// retry once. TCP/inproc have no stale-file problem.
pub fn listen_control(sock: &Socket, url: &str) -> Result<(), nng::Error> {
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

/// A dedicated slow loop (classic design §7 explicitly permits this) that paces a periodic
/// tick onto the given mailbox — the router does the actual scan on its own thread.
/// `make_tick` builds the mailbox message (each orchestrator has its own enum).
pub fn start_hang_detector<T, F>(make_tick: F, tx: mpsc::Sender<T>)
where
    T: Send + 'static,
    F: Fn() -> T + Send + 'static,
{
    std::thread::spawn(move || {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if tx.send(make_tick()).is_err() {
                break; // router gone
            }
        }
    });
}
