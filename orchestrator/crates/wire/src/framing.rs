//! Framing (§18.1) — `[u32 BE json_len][JSON][binary…]`.
//!
//! Extracted from classic's `main.rs` / the data-runner's `main.rs`, where it was
//! duplicated. Bulk bytes (a view result's TSV, an edit's forward cells, an
//! inverse's Arrow-IPC blob) ride the binary tail and never go through the JSON
//! parser on any hop.

use crate::Envelope;

/// Frame raw JSON bytes into `[u32 BE length][json bytes]`.
pub fn frame_bytes(json: &[u8]) -> Vec<u8> {
    frame_parts(json, &[])
}

/// Frame with an optional binary tail: `[u32 BE json_len][JSON][binary…]` (§18.1). View
/// results carry their escaped TSV in the tail — bulk bytes never go through the JSON
/// parser on any hop.
pub fn frame_parts(json: &[u8], binary: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + json.len() + binary.len());
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(json);
    out.extend_from_slice(binary);
    out
}

/// Frame a typed [`Envelope`].
pub fn frame_envelope(env: &Envelope) -> Vec<u8> {
    frame_bytes(&serde_json::to_vec(env).expect("serialize envelope"))
}

/// Parse `[u32 BE length][json bytes][…]` into a typed [`Envelope`].
pub fn deframe(body: &[u8]) -> Option<Envelope> {
    deframe_parts(body).map(|(env, _)| env)
}

/// Split a frame (§18.1) into its JSON envelope and the trailing binary payload (empty
/// slice when the frame is JSON-only). The bulk bytes are never parsed as JSON.
pub fn deframe_parts(body: &[u8]) -> Option<(Envelope, &[u8])> {
    if body.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + len {
        return None;
    }
    let env = serde_json::from_slice(&body[4..4 + len]).ok()?;
    Some((env, &body[4 + len..]))
}

/// Epoch milliseconds (saturates to 0 before the epoch). Lives here because every
/// envelope stamping site (router, data worker) uses the same clock.
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
