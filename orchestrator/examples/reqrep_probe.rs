//! Probe: REQ/REP + §18.1 framing.
//!
//! Confirms that REQ/REP cooked mode carries our `[u32 BE len][JSON]` payload transparently,
//! and that a REP socket auto-routes replies to the correct REQ peer when multiple clients
//! are connected. This validates the control-endpoint mechanics the orchestrator design leans on.
//!
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example reqrep_probe
use nng::options::{Options, RecvTimeout};
use nng::{Protocol, Socket};
use std::time::Duration;

/// Frame JSON bytes into `[u32 BE length][json bytes]` (§18.1).
fn frame(json: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + json.len());
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(json);
    out
}

/// Deframe `[u32 BE length][json bytes]` into a `serde_json::Value`.
fn deframe(body: &[u8]) -> Option<serde_json::Value> {
    if body.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + len {
        return None;
    }
    serde_json::from_slice(&body[4..4 + len]).ok()
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "inproc://reqrep-probe";

    // ── REP server ──
    let rep = Socket::new(Protocol::Rep0)?;
    rep.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))?;
    rep.listen(addr)?;
    println!("[rep] listening on {addr}");

    // ── Two REQ clients (tests auto-routing) ──
    let req_a = Socket::new(Protocol::Req0)?;
    req_a.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))?;
    req_a.dial(addr)?;

    let req_b = Socket::new(Protocol::Req0)?;
    req_b.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))?;
    req_b.dial(addr)?;

    // ── Client A sends a framed "register" ──
    let register = serde_json::json!({
        "v": 1, "type": "register", "id": "reg-a",
        "capabilities": [{"kind": "analysis", "name": "jaspTTests", "version": "0.95.5"}],
        "priority": 0
    });
    req_a
        .send(frame(&serde_json::to_vec(&register)?).as_slice())
        .map_err(|(_, e)| e)?;
    println!("[req_a] sent register");

    // REP receives client A's request.
    let msg = rep.recv()?;
    let got = deframe(&msg[..]).expect("valid frame from req_a");
    println!(
        "[rep] received from req_a: type={} id={}",
        got["type"], got["id"]
    );
    assert_eq!(got["type"], "register");
    assert_eq!(got["id"], "reg-a");

    // REP replies — auto-routed to client A.
    let ack_a = serde_json::json!({
        "v": 1, "type": "register_ack", "id": "ack-a", "reply_to": "reg-a",
        "ok": true, "runner_id": "r-1",
        "channel_url": "inproc://ch-r-1", "activity_min_interval_ms": 1000
    });
    rep.send(frame(&serde_json::to_vec(&ack_a)?).as_slice())
        .map_err(|(_, e)| e)?;
    println!("[rep] sent register_ack to req_a");

    // Client A receives its ack.
    let reply_a = req_a.recv()?;
    let got_a = deframe(&reply_a[..]).expect("valid ack for req_a");
    println!(
        "[req_a] received: type={} runner_id={}",
        got_a["type"], got_a["runner_id"]
    );
    assert_eq!(got_a["type"], "register_ack");
    assert_eq!(got_a["runner_id"], "r-1");
    assert_eq!(got_a["channel_url"], "inproc://ch-r-1");

    // ── Client B sends a framed "hello" ──
    let hello = serde_json::json!({
        "v": 1, "type": "hello", "id": "hello-b",
        "client_id": "desktop-42"
    });
    req_b
        .send(frame(&serde_json::to_vec(&hello)?).as_slice())
        .map_err(|(_, e)| e)?;
    println!("[req_b] sent hello");

    // REP receives client B's request.
    let msg = rep.recv()?;
    let got = deframe(&msg[..]).expect("valid frame from req_b");
    println!(
        "[rep] received from req_b: type={} id={}",
        got["type"], got["id"]
    );
    assert_eq!(got["type"], "hello");
    assert_eq!(got["id"], "hello-b");

    // REP replies — auto-routed to client B.
    let welcome = serde_json::json!({
        "v": 1, "type": "welcome", "id": "welcome-b", "reply_to": "hello-b",
        "ok": true, "session_id": "s-1", "channel_url": "inproc://ch-s-1"
    });
    rep.send(frame(&serde_json::to_vec(&welcome)?).as_slice())
        .map_err(|(_, e)| e)?;
    println!("[rep] sent welcome to req_b");

    // Client B receives its welcome.
    let reply_b = req_b.recv()?;
    let got_b = deframe(&reply_b[..]).expect("valid welcome for req_b");
    println!(
        "[req_b] received: type={} session_id={}",
        got_b["type"], got_b["session_id"]
    );
    assert_eq!(got_b["type"], "welcome");
    assert_eq!(got_b["session_id"], "s-1");
    assert_eq!(got_b["channel_url"], "inproc://ch-s-1");

    println!("\n[probe] REQ/REP + §18.1 framing + multi-client auto-routing: PASS");
    Ok(())
}
