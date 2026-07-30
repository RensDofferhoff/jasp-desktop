//! Probe: Abstract IPC sockets on Linux.
//!
//! Confirms the abstract-socket URL syntax in the Rust `nng` crate and that it leaves
//! **no stale file** after a crash/drop on this Linux box. Abstract sockets (sun_path starts
//! with NUL) have no filesystem presence and are auto-cleaned by the kernel.
//!
//! NNG's IPC transport recognises abstract names when the path component starts with a NUL
//! byte. In a URL string we encode this as `ipc://\0name` — but since Rust strings can hold
//! interior NULs, we build the URL with an explicit '\0'.
//!
//! Run: cargo run --manifest-path orchestrator/Cargo.toml --example ipc_abstract_probe
use nng::options::{Options, RecvTimeout};
use nng::{Protocol, Socket};
use std::time::Duration;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // ── 1. Try abstract IPC (Linux-only; path starts with NUL) ──
    // NNG ipc transport: if sun_path[0] == '\0', it's an abstract socket.
    // We build the URL with an embedded NUL: "ipc://\0jasp-probe-<pid>"
    let abstract_url = format!("ipc://\0jasp-probe-{}", std::process::id());
    println!(
        "[probe] trying abstract IPC url (len={}, starts_with ipc://\\0): {}",
        abstract_url.len(),
        abstract_url.starts_with("ipc://\0")
    );

    let server = Socket::new(Protocol::Pair1)?;
    match server.listen(&abstract_url) {
        Ok(()) => {
            println!("[probe] abstract IPC listen: OK — no filesystem path created");
        }
        Err(e) => {
            println!("[probe] abstract IPC listen FAILED: {e}");
            println!(
                "[probe] (this is expected on non-Linux; falling back to filesystem IPC test)"
            );
            return test_filesystem_ipc();
        }
    }

    // Verify no file was created (the path after ipc:// is "\0jasp-probe-<pid>";
    // the filesystem path would be "/tmp/jasp-probe-<pid>" or similar — check a few).
    let pid = std::process::id();
    for candidate in [
        format!("/tmp/jasp-probe-{pid}"),
        format!("/tmp/\0jasp-probe-{pid}"),
        format!("jasp-probe-{pid}"),
    ] {
        assert!(
            !std::path::Path::new(&candidate).exists(),
            "abstract socket must not create file: {candidate}"
        );
    }
    println!("[probe] no stale file on creation: OK");

    // Round-trip a message.
    let client = Socket::new(Protocol::Pair1)?;
    client.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))?;
    client.dial(&abstract_url)?;
    client
        .send("ping-abstract".as_bytes())
        .map_err(|(_, e)| e)?;

    let msg = server.recv()?;
    assert_eq!(&msg[..], b"ping-abstract");
    println!("[probe] abstract IPC round-trip: OK");

    // Drop everything (simulate crash) — kernel auto-cleans abstract sockets.
    drop(client);
    drop(server);
    std::thread::sleep(Duration::from_millis(100));

    // Still no file.
    for candidate in [
        format!("/tmp/jasp-probe-{pid}"),
        format!("jasp-probe-{pid}"),
    ] {
        assert!(
            !std::path::Path::new(&candidate).exists(),
            "abstract socket must not leave stale file: {candidate}"
        );
    }
    println!("[probe] no stale file after drop (crash simulation): OK");

    println!("\n[probe] Abstract IPC on Linux: PASS");
    Ok(())
}

/// Fallback: test filesystem IPC with unique paths and manual cleanup.
fn test_filesystem_ipc() -> Result<(), Box<dyn std::error::Error>> {
    let pid = std::process::id();
    let path = format!("/tmp/jasp-fs-probe-{pid}.sock");
    let url = format!("ipc://{path}");

    // Clean up any stale file from a previous crashed run.
    let _ = std::fs::remove_file(&path);

    let server = Socket::new(Protocol::Pair1)?;
    server.listen(&url)?;
    println!("[probe] filesystem IPC listen on {url}: OK");

    let client = Socket::new(Protocol::Pair1)?;
    client.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))?;
    client.dial(&url)?;
    client.send("ping-fs".as_bytes()).map_err(|(_, e)| e)?;

    let msg = server.recv()?;
    assert_eq!(&msg[..], b"ping-fs");
    println!("[probe] filesystem IPC round-trip: OK");

    drop(client);
    drop(server);

    // Filesystem IPC DOES leave a stale file — this is why abstract is preferred on Linux.
    let stale = std::path::Path::new(&path).exists();
    println!("[probe] stale file after drop: {stale} (expected true for filesystem IPC)");
    let _ = std::fs::remove_file(&path); // manual cleanup

    println!(
        "\n[probe] Filesystem IPC (fallback): PASS (use abstract on Linux to avoid stale files)"
    );
    Ok(())
}
