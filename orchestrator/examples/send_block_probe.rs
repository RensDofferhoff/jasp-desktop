//! Probe: does a synchronous inproc PAIR `send` block until the peer posts a recv? And does doing
//! it inside an Aio recv callback therefore park an NNG pool thread?
//!
//! We arm an Aio recv on `srv`. When it fires, the callback does a SYNCHRONOUS `srv.send()` back to
//! `cli` while `cli` has NOT posted a recv. We then observe (via an atomic phase flag) whether that
//! send returns immediately (buffered) or blocks until `cli` recvs.
//!
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example send_block_probe
use nng::options::{Options, RecvTimeout};
use nng::{Aio, AioResult, Protocol, Socket};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn main() {
    let url = "inproc://sendblock-probe";
    let srv = Arc::new(Socket::new(Protocol::Pair1).unwrap());
    srv.listen(url).unwrap();
    let cli = Arc::new(Socket::new(Protocol::Pair1).unwrap());
    cli.dial(url).unwrap();
    std::thread::sleep(Duration::from_millis(100)); // let the inproc pipe connect

    // phase: 0 = callback not entered; 1 = inside the sync send; 2 = send returned.
    let phase = Arc::new(AtomicUsize::new(0));
    let srv_c = Arc::clone(&srv);
    let ph = Arc::clone(&phase);
    let aio = Aio::new(move |aio: Aio, res: AioResult| {
        if let AioResult::Recv(Ok(_)) = res {
            ph.store(1, Ordering::SeqCst);
            // Synchronous send back to cli. At this instant cli has NOT posted a recv.
            let _ = srv_c.send("reply".as_bytes());
            ph.store(2, Ordering::SeqCst);
            let _ = srv_c.recv_async(&aio);
        }
    })
    .unwrap();
    srv.recv_async(&aio).unwrap();
    let _keep = aio; // keep the recv Aio alive for the life of main

    // Trigger from a separate thread so a synchronous-delivery hang can't stall `main`.
    let cli_t = Arc::clone(&cli);
    std::thread::spawn(move || {
        let _ = cli_t.send("trigger".as_bytes());
    });

    std::thread::sleep(Duration::from_millis(800));
    let p = phase.load(Ordering::SeqCst);
    println!("[probe] 800ms after trigger, cli has NOT recvd: phase = {p}");
    match p {
        0 => println!("[probe] callback never fired (unexpected — connection/recv issue)"),
        1 => println!("[probe] => sync send inside the callback is BLOCKING (peer not recving)"),
        2 => {
            println!("[probe] => sync send returned immediately (buffered; NOT the deadlock cause)")
        }
        _ => {}
    }

    if p == 1 {
        println!("[probe] now posting a recv on cli to see if the blocked send unblocks ...");
        cli.set_opt::<RecvTimeout>(Some(Duration::from_secs(2)))
            .unwrap();
        match cli.recv() {
            Ok(m) => println!(
                "[probe] cli received {:?} -> the blocked send delivered once the peer recvd",
                String::from_utf8_lossy(&m[..])
            ),
            Err(e) => println!("[probe] cli recv error: {e}"),
        }
        std::thread::sleep(Duration::from_millis(200));
        println!(
            "[probe] phase now = {} (2 = send completed after peer recvd)",
            phase.load(Ordering::SeqCst)
        );
    }

    println!("[probe] done");
}
