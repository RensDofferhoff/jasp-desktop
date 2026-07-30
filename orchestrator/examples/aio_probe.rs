//! Proof of NNG's *safe* Aio recv-loop mechanics (no FFI) on this crate version.
//!
//! A server socket whose Aio callback prints each received message and re-arms, driven by a
//! client sending several messages. Demonstrates, on NNG's own internal event loop (the main
//! thread just sleeps):
//!   * the callback fires on recv,
//!   * we can pull the `Message` out of `AioResult::Recv(Ok(_))`,
//!   * re-arming from inside the callback works across multiple completions,
//!   * keeping the `Aio` alive in scope keeps the loop running (the trampoline only holds a weak
//!     ref, so a dropped `Aio` would silently stop the loop).
//!
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example aio_probe
use nng::{Aio, AioResult, Protocol, Socket};
use std::sync::Arc;
use std::time::Duration;

const ADDR: &str = "inproc://aio-probe";

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let server = Arc::new(Socket::new(Protocol::Pair1)?);
    server.listen(ADDR)?;

    // The recv loop: one Aio, re-armed from inside its own callback. `srv` is the socket the
    // callback re-arms on; it is captured (Arc<Socket> is Send+Sync+'static).
    let srv = Arc::clone(&server);
    let aio = Aio::new(move |aio: Aio, res: AioResult| match res {
        AioResult::Recv(Ok(msg)) => {
            println!("[server] recv: {}", String::from_utf8_lossy(msg.as_slice()));
            if let Err(e) = srv.recv_async(&aio) {
                eprintln!("[server] re-arm failed: {e}");
            }
        }
        AioResult::Recv(Err(e)) => println!("[server] recv ended: {e}"),
        _ => println!("[server] other event"),
    })?;

    // Arm the first recv. `aio` MUST stay in scope: the callback trampoline only holds a weak ref
    // to it, so dropping `aio` here would stop the loop after the first message.
    server.recv_async(&aio)?;

    let client = Socket::new(Protocol::Pair1)?;
    client.dial(ADDR)?;
    for i in 1..=3 {
        client
            .send(format!("hello {i}").as_bytes())
            .map_err(|(_, e)| e)?;
        std::thread::sleep(Duration::from_millis(50));
    }

    // The loop is driven entirely by NNG's internal pool; main just waits.
    std::thread::sleep(Duration::from_millis(500));
    println!("[main] done");
    Ok(())
}
