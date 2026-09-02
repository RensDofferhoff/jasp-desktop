//! Probe: pinpoint the TCP/Listener::new/REQ-REP failure.
//!
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example tcp_listener_probe
use nng::options::{LocalAddr, Options, RecvTimeout};
use nng::{Listener, Protocol, Socket};
use std::time::Duration;

fn try_roundtrip(label: &str, rep: &Socket, dial_url: &str) {
    let req = Socket::new(Protocol::Req0).unwrap();
    req.set_opt::<RecvTimeout>(Some(Duration::from_secs(2)))
        .unwrap();
    match req.dial(dial_url) {
        Ok(()) => {}
        Err(e) => {
            println!("[{label}] dial FAILED: {e}");
            return;
        }
    }
    match req.send("ping".as_bytes()) {
        Ok(()) => {}
        Err((_, e)) => {
            println!("[{label}] send FAILED: {e}");
            return;
        }
    }
    match rep.recv() {
        Ok(m) => println!(
            "[{label}] REP recv OK: {:?}",
            String::from_utf8_lossy(&m[..])
        ),
        Err(e) => {
            println!("[{label}] REP recv FAILED: {e}");
            return;
        }
    }
    let _ = rep.send("pong".as_bytes());
    match req.recv() {
        Ok(m) => println!(
            "[{label}] full round-trip OK: {:?}",
            String::from_utf8_lossy(&m[..])
        ),
        Err(e) => println!("[{label}] REQ recv FAILED: {e}"),
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // A. Socket::listen on a FIXED tcp port (the alpha's known-good pattern).
    let rep_a = Socket::new(Protocol::Rep0)?;
    rep_a.listen("tcp://127.0.0.1:19555")?;
    println!("[A] Socket::listen tcp://127.0.0.1:19555");
    try_roundtrip(
        "A Socket::listen fixed-tcp",
        &rep_a,
        "tcp://127.0.0.1:19555",
    );

    // B. Listener::new on a FIXED tcp port.
    let rep_b = Socket::new(Protocol::Rep0)?;
    let _lb = Listener::new(&rep_b, "tcp://127.0.0.1:19556")?;
    println!("[B] Listener::new tcp://127.0.0.1:19556");
    try_roundtrip("B Listener::new fixed-tcp", &rep_b, "tcp://127.0.0.1:19556");

    // C. Listener::new on EPHEMERAL tcp port, read back via LocalAddr.
    let rep_c = Socket::new(Protocol::Rep0)?;
    let _lc = Listener::new(&rep_c, "tcp://127.0.0.1:0")?;
    let addr_c: nng::SocketAddr = _lc.get_opt::<LocalAddr>()?;
    let url_c = format!("{addr_c}");
    println!("[C] Listener::new ephemeral, readback = {url_c}");
    try_roundtrip("C Listener::new ephemeral-tcp", &rep_c, &url_c);

    // D. PAIR over TCP via Listener::new ephemeral (rule out REQ/REP specifically).
    let pair_d = Socket::new(Protocol::Pair1)?;
    let _ld = Listener::new(&pair_d, "tcp://127.0.0.1:0")?;
    let addr_d: nng::SocketAddr = _ld.get_opt::<LocalAddr>()?;
    let url_d = format!("{addr_d}");
    println!("[D] PAIR Listener::new ephemeral, readback = {url_d}");
    let dialer = Socket::new(Protocol::Pair1)?;
    dialer.set_opt::<RecvTimeout>(Some(Duration::from_secs(2)))?;
    match dialer.dial(&url_d) {
        Ok(()) => {
            let _ = dialer.send("hi".as_bytes());
            match pair_d.recv() {
                Ok(m) => println!(
                    "[D] PAIR round-trip OK: {:?}",
                    String::from_utf8_lossy(&m[..])
                ),
                Err(e) => println!("[D] PAIR recv FAILED: {e}"),
            }
        }
        Err(e) => println!("[D] PAIR dial FAILED: {e}"),
    }

    println!("\n[probe] done");
    Ok(())
}
