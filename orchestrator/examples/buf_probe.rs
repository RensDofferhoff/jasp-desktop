//! Probe: what is the default socket send/recv buffer depth (NNG_OPT_SENDBUF/RECVBUF) for our NNG?
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example buf_probe
use nng::options::{Options, RecvBufferSize, SendBufferSize};
use nng::{Protocol, Socket};

fn main() {
    for (name, proto) in [
        ("pair1", Protocol::Pair1),
        ("rep0", Protocol::Rep0),
        ("req0", Protocol::Req0),
    ] {
        let s = Socket::new(proto).unwrap();
        let sb = s.get_opt::<SendBufferSize>().unwrap();
        let rb = s.get_opt::<RecvBufferSize>().unwrap();
        println!("{name}: default SENDBUF = {sb:?}, RECVBUF = {rb:?}");
    }
}
