//! The NEO wire protocol, as Rust types — THE single source of truth.
//!
//! Extracted verbatim from the classic orchestrator's `src/messages.rs` into the
//! `wire` crate (orchestrator-v2 design §11/§12 step 1: a pure move, zero behavior
//! change; classic and v2 both compile against this crate so the protocol can
//! never fork). Because the types are defined here with `serde` + `schemars`:
//!
//! * (de)serialization is **derived**, not hand-written — a field typo is a compile error;
//! * a **JSON Schema** for the whole protocol is generated from these very types
//!   (`cargo run --bin jasp-orchestrator -- --schema`), so the C++ and R sides can
//!   be validated against it;
//! * the types double as **living documentation** of neo-jasp.md §18–§19.
//!
//! The crate also owns the byte framing (§18.1, [`framing`]) and — so the
//! NNG traps live in exactly one place — the shared orchestrator-side channel
//! plumbing ([`transport`]) and the off-thread filesystem janitor ([`janitor`]).
//!
//! Wire rules (v2 design §3): every change here is **additive**; `v` stays 1.

pub mod framing;
pub mod janitor;
pub mod messages;
pub mod provisioner;
pub mod transport;

pub use messages::*;
