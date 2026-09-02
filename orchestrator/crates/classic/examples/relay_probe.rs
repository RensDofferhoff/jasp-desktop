//! Probe: does the broker's two-channel RELAY pattern deadlock with inline synchronous sends?
//! No RwLocks, no pipe_notify, no sender thread — just two PAIR channels whose Aio recv callbacks
//! relay to each other via synchronous sends, exactly like route_work / route_result. If this hangs,
//! the relay/send pattern is the culprit; if it completes, the original deadlock lived elsewhere
//! (locks / pipe_notify / lifecycle) and the sender thread may have been unnecessary.
//!
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example relay_probe
use nng::options::{Options, RecvTimeout};
use nng::{Aio, AioResult, Protocol, Socket};
use std::sync::Arc;
use std::time::Duration;

fn main() {
    let fe_ch = Arc::new(Socket::new(Protocol::Pair1).unwrap());
    fe_ch.listen("inproc://relay-fe").unwrap();
    let rn_ch = Arc::new(Socket::new(Protocol::Pair1).unwrap());
    rn_ch.listen("inproc://relay-rn").unwrap();

    // frontend-ch recv -> relay inline to runner-ch
    let rn = Arc::clone(&rn_ch);
    let fe = Arc::clone(&fe_ch);
    let fe_aio = Aio::new(move |aio: Aio, res: AioResult| {
        if let AioResult::Recv(Ok(m)) = res {
            println!("[relay] fe->rn forwarding {} bytes", m.as_slice().len());
            let _ = rn.send(m.as_slice());
            println!("[relay] fe->rn send returned");
            let _ = fe.recv_async(&aio);
        }
    })
    .unwrap();
    fe_ch.recv_async(&fe_aio).unwrap();

    // runner-ch recv -> relay inline to frontend-ch
    let fe2 = Arc::clone(&fe_ch);
    let rn2 = Arc::clone(&rn_ch);
    let rn_aio = Aio::new(move |aio: Aio, res: AioResult| {
        if let AioResult::Recv(Ok(m)) = res {
            println!("[relay] rn->fe forwarding {} bytes", m.as_slice().len());
            let _ = fe2.send(m.as_slice());
            println!("[relay] rn->fe send returned");
            let _ = rn2.recv_async(&aio);
        }
    })
    .unwrap();
    rn_ch.recv_async(&rn_aio).unwrap();

    let _keep = (fe_aio, rn_aio); // keep recv Aios alive

    // mock peers
    let mock_fe = Socket::new(Protocol::Pair1).unwrap();
    mock_fe
        .set_opt::<RecvTimeout>(Some(Duration::from_secs(3)))
        .unwrap();
    mock_fe.dial("inproc://relay-fe").unwrap();
    let mock_rn = Socket::new(Protocol::Pair1).unwrap();
    mock_rn
        .set_opt::<RecvTimeout>(Some(Duration::from_secs(3)))
        .unwrap();
    mock_rn.dial("inproc://relay-rn").unwrap();
    std::thread::sleep(Duration::from_millis(100));

    println!("[main] mock_fe sends work");
    mock_fe.send("work".as_bytes()).map_err(|(_, e)| e).unwrap();
    println!("[main] mock_rn recv ...");
    match mock_rn.recv() {
        Ok(m) => println!(
            "[main] runner got: {:?}",
            String::from_utf8_lossy(m.as_slice())
        ),
        Err(e) => println!("[main] runner recv FAILED: {e}"),
    }
    println!("[main] mock_rn sends result");
    mock_rn
        .send("result".as_bytes())
        .map_err(|(_, e)| e)
        .unwrap();
    println!("[main] mock_fe recv ...");
    match mock_fe.recv() {
        Ok(m) => println!(
            "[main] frontend got: {:?}",
            String::from_utf8_lossy(m.as_slice())
        ),
        Err(e) => println!("[main] frontend recv FAILED: {e}"),
    }
    println!("[main] done");
}
