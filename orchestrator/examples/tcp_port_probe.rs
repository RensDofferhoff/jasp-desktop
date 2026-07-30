//! Probe: confirm the nng 1.0.1 `LocalAddr` port byte-swap, and that correcting it yields a
//! dialable ephemeral TCP address.
//!
//! NNG's facility for a fresh TCP port is `tcp://host:0` (OS-assigned ephemeral) read back via the
//! listener's `LocalAddr` option. The crate's `From<nng_sockaddr>` byte-swaps the address but NOT
//! the port (NNG stores `sa_port` in network order; `SocketAddrV4::new` wants host order), so on a
//! little-endian host the readback port is endian-flipped. This probe binds `:0`, reads the port
//! back, and dials BOTH the raw and the byte-swapped port to prove which one actually connects.
//!
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example tcp_port_probe
use nng::options::{LocalAddr, Options, RecvTimeout};
use nng::{Listener, Protocol, Socket};
use std::net::SocketAddrV4;
use std::time::Duration;

fn dial_probe(url: &str) -> Result<(), String> {
    let req = Socket::new(Protocol::Req0).map_err(|e| e.to_string())?;
    req.set_opt::<RecvTimeout>(Some(Duration::from_millis(800)))
        .map_err(|e| e.to_string())?;
    req.dial(url).map_err(|e| e.to_string())?;
    // A successful REQ dial + send + REP recv proves the endpoint is genuinely live.
    req.send("ping".as_bytes()).map_err(|(_, e)| e.to_string())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let rep = Socket::new(Protocol::Rep0)?;
    let listener = Listener::new(&rep, "tcp://127.0.0.1:0")?;
    let addr: nng::SocketAddr = listener.get_opt::<LocalAddr>()?;

    let nng::SocketAddr::Inet(v4) = addr else {
        println!("[probe] unexpected addr family: {addr:?}");
        return Ok(());
    };
    let raw_port: u16 = v4.port();
    let fixed_port: u16 = raw_port.swap_bytes();
    let ip = *v4.ip();
    println!("[probe] LocalAddr readback (raw, as the crate gives it): {v4}");
    println!("[probe] raw port      = {raw_port} (0x{raw_port:04x})");
    println!("[probe] swapped port  = {fixed_port} (0x{fixed_port:04x})");

    // Drain the REP socket in the background so a successful dial+send doesn't block on send buffer.
    let rep2 = rep.clone();
    std::thread::spawn(move || {
        loop {
            if rep2.recv().is_err() {
                break;
            }
        }
    });

    let raw_url = format!("tcp://{ip}:{raw_port}");
    let fixed_url = format!("tcp://{ip}:{fixed_port}");

    println!("\n[probe] dialing RAW readback port  {raw_url} ...");
    match dial_probe(&raw_url) {
        Ok(()) => println!("[probe]   RAW port CONNECTED (no bug — surprising!)"),
        Err(e) => println!("[probe]   RAW port failed: {e}"),
    }
    println!("[probe] dialing SWAPPED port       {fixed_url} ...");
    match dial_probe(&fixed_url) {
        Ok(()) => println!("[probe]   SWAPPED port CONNECTED — byte-swap theory CONFIRMED"),
        Err(e) => println!("[probe]   SWAPPED port failed: {e}"),
    }

    let _ = SocketAddrV4::new(*v4.ip(), fixed_port); // (kept for clarity)
    println!("\n[probe] done");
    Ok(())
}
