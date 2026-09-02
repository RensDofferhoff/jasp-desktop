//! The janitor — off-router filesystem reclamation.
//!
//! Extracted verbatim from classic's `main.rs`. The router only *decides* (pure
//! memory) and enqueues non-blocking; this thread does the blocking I/O
//! sequentially ("the router never blocks" invariant, P1).

use std::path::PathBuf;
use std::sync::mpsc;

/// A deferred filesystem reclamation request. `Dir` reclaims a workspace tree
/// (`remove_dir_all`); `File` reclaims a single retired dataset cache file
/// (`remove_file`) — the dataset manager's new-file + map-swap mechanism retires
/// files the janitor deletes once their refcount drains.
pub enum Reclaim {
    Dir(PathBuf),
    File(PathBuf),
}

/// Mailbox for the janitor: reclamation requests.
pub type JanitorTx = mpsc::Sender<Reclaim>;

/// Spawn the single janitor thread and return its mailbox. Filesystem deletion is
/// blocking I/O of unbounded duration, so it must never run on the router thread.
/// Deletion is best-effort — `NotFound` is treated as success (already gone), so
/// cleanup is idempotent and a failed or repeated delete is harmless; startup GC
/// is the backstop for anything leaked.
pub fn start_janitor() -> JanitorTx {
    let (tx, rx) = mpsc::channel::<Reclaim>();
    std::thread::Builder::new()
        .name("orch-janitor".into())
        .spawn(move || {
            while let Ok(reclaim) = rx.recv() {
                let (path, result) = match reclaim {
                    Reclaim::Dir(path) => (path.clone(), std::fs::remove_dir_all(&path)),
                    Reclaim::File(path) => (path.clone(), std::fs::remove_file(&path)),
                };
                match result {
                    Ok(()) => println!("[orch] reclaimed {}", path.display()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => eprintln!("[orch] cleanup failed for {}: {e}", path.display()),
                }
            }
        })
        .expect("spawn janitor thread");
    tx
}
