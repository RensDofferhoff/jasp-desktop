//! NEO JASP data-runner — the Rust data-plane lane.
//!
//! A self-attaching runner that advertises `Capability::Data { op: data_open, formats: ["csv"] }`
//! plus `Capability::Data { op: data_view }` and serves both:
//!
//! * `data_open` — converts the source CSV into a canonical Arrow Feather file at the
//!   orchestrator-assigned `cache_path` (the `csv2arrow` production path, ported in
//!   [`csv2arrow`]) and replies with the `kind:"data"` result payload (`{schema, rows}`, §19.2
//!   *Result payloads by kind*) — a few KB; the data itself never crosses the wire.
//! * `data_view` — slices the cached Feather and renders the requested row window as escaped
//!   TSV ([`arrowview`], data-view-format.md §1.2) in the frame's **binary part**
//!   (`format:"text/tsv"`, §18.1). The lane contract relaxes to "a window-sized blob" for
//!   this op only (dataset-manager-as-implemented §7); every response is bounded by the
//!   request's `max_bytes` + a small envelope.
//!
//! Connection model (mirrors the R runner / §19.5): REQ dial on the control endpoint →
//! `register` → `register_ack { channel_url }` → PAIR dial of the data channel → all work/result
//! flows there.
//!
//! Scope: CSV open + views of any cache. No edits, no Arrow-native open, no R lane
//! (spss/excel) — later.
//!
//! Run: `JASP_ORCH_URL=tcp://127.0.0.1:9555 cargo run --bin jasp-data-runner`

#[path = "../arrowview.rs"]
mod arrowview;
#[path = "../csv2arrow.rs"]
mod csv2arrow;
#[path = "../messages.rs"]
mod messages;

use messages::{Capability, DataOp, Envelope, Message, Status, WorkPayload};
use nng::options::{
    Options, RecvBufferSize, RecvMaxSize, RecvTimeout, SendBufferSize, SendTimeout,
};
use nng::{Pipe, PipeEvent, Protocol, Socket};
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

// ─── Framing (§18.1) — same as the orchestrator ──────────────────────

fn frame_bytes(json: &[u8]) -> Vec<u8> {
    frame_parts(json, &[])
}

/// Frame with an optional binary tail: `[u32 BE json_len][JSON][binary…]` (§18.1). View
/// results put the escaped TSV in the tail — never inside a JSON string.
fn frame_parts(json: &[u8], binary: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + json.len() + binary.len());
    out.extend_from_slice(&(json.len() as u32).to_be_bytes());
    out.extend_from_slice(json);
    out.extend_from_slice(binary);
    out
}

fn frame_envelope(env: &Envelope) -> Vec<u8> {
    frame_bytes(&serde_json::to_vec(env).expect("serialize envelope"))
}

fn deframe(body: &[u8]) -> Option<Envelope> {
    if body.len() < 4 {
        return None;
    }
    let len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    if body.len() < 4 + len {
        return None;
    }
    serde_json::from_slice(&body[4..4 + len]).ok()
}

// ─── Wire helpers ────────────────────────────────────────────────────────────

/// The lane's `kind:"data"` payload (§19.2) for a successful `data_open`: the per-column
/// schema (name / display_name / type / levels / all_integer — the frontend's view of the
/// dataset) plus the row count. The orchestrator fills `dataset_id` when it forwards the
/// terminal result; the lane leaves identity fields empty (producers fill content, the
/// orchestrator fills identity).
fn data_payload(out: &csv2arrow::ConvertOutput) -> messages::DataResult {
    let columns: Vec<serde_json::Value> = out
        .columns
        .iter()
        .map(|c| {
            let mut col = serde_json::Map::new();
            col.insert("name".into(), json!(c.name));
            col.insert("display_name".into(), json!(c.display_name));
            col.insert("type".into(), json!(c.level));
            if c.level == "scale" {
                col.insert("all_integer".into(), json!(c.all_integer));
            }
            if let Some(levels) = &c.levels {
                col.insert("levels".into(), json!(levels));
            }
            // Constraint-check stats (data-model-design.md §2): value_count = non-empty cell
            // count (zero IS a value); distinct_count = distinct values, exact for categoricals
            // and exact up to ~1k for scale (the cap value means "at least that many"). This
            // serves every form constraint in the module registries — minLevels/maxLevels,
            // minNumericLevels/maxNumericLevels, and the "maximum levels for scale" gate.
            col.insert("value_count".into(), json!(c.value_count));
            col.insert("distinct_count".into(), json!(c.distinct_count));
            if let Some(nl) = c.numeric_levels {
                col.insert("numeric_levels".into(), json!(nl));
            }
            serde_json::Value::Object(col)
        })
        .collect();
    messages::DataResult {
        dataset_id: None,
        dataset_revision: None,
        rows: Some(out.rows),
        schema: Some(serde_json::Value::Array(columns)),
        error_message: None,
        row_offset: None,
        row_count: None,
        truncated: None,
    }
}

/// The lane's failure payload (§19.2): `error_message` on the Data payload.
fn data_error(message: String) -> messages::DataResult {
    messages::DataResult {
        dataset_id: None,
        dataset_revision: None,
        rows: None,
        schema: None,
        error_message: Some(message),
        row_offset: None,
        row_count: None,
        truncated: None,
    }
}

fn result_env(
    session_id: Option<String>,
    work_id: &str,
    revision: u64,
    status: Status,
    payload: messages::DataResult,
) -> Envelope {
    Envelope {
        v: 1,
        id: format!("data-result-{work_id}"),
        reply_to: None,
        session_id,
        format: None,
        ts: None,
        body: Message::Result(messages::ResultMsg {
            work_id: work_id.to_string(),
            revision,
            status,
            payload: messages::ResultPayload::Data(payload),
            module_version: None,
            message: None,
        }),
    }
}

// ─── main ────────────────────────────────────────────────────────────────────

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let control_url =
        std::env::var("JASP_ORCH_URL").unwrap_or_else(|_| "tcp://127.0.0.1:9555".into());

    // 1. Handshake on the control endpoint (REQ/REP): register the data capability.
    let req = Socket::new(Protocol::Req0)?;
    req.set_opt::<RecvTimeout>(Some(Duration::from_secs(10)))?;
    req.dial(&control_url)?;
    let register = Envelope {
        v: 1,
        id: format!("data-reg-{}", std::process::id()),
        reply_to: None,
        session_id: None,
        format: None,
        ts: None,
        body: Message::Register(messages::Register {
            runner_id: Some("jasp-data-runner".to_string()),
            capabilities: vec![
                Capability::Data {
                    op: DataOp::Open,
                    formats: Some(vec!["csv".to_string()]),
                },
                // View is format-agnostic (it reads the cache) → no formats (§9.4).
                Capability::Data {
                    op: DataOp::View,
                    formats: None,
                },
            ],
            priority: 0,
            environment: json!(null),
        }),
    };
    req.send(frame_envelope(&register).as_slice())
        .map_err(|(_, e)| e)?;
    let ack_raw = req.recv().map_err(|e| format!("no register_ack: {e}"))?;
    let ack = deframe(&ack_raw[..]).ok_or("undecodable register_ack")?;
    let (runner_id, channel_url) = match ack.body {
        Message::RegisterAck(a) if a.ok => (
            a.runner_id.ok_or("register_ack missing runner_id")?,
            a.channel_url.ok_or("register_ack missing channel_url")?,
        ),
        Message::RegisterAck(a) => {
            return Err(format!("registration rejected: {}", a.reason.unwrap_or_default()).into());
        }
        other => return Err(format!("expected register_ack, got {other:?}").into()),
    };
    println!("[data] registered {runner_id} (data_open: csv, data_view) channel={channel_url}");

    // 2. Dial the dedicated PAIR data channel; all work/result flows here.
    let ch = Socket::new(Protocol::Pair1)?;
    ch.set_opt::<SendBufferSize>(64)?;
    ch.set_opt::<RecvBufferSize>(64)?;
    // §18.4/§4.4: libnng's ~1 MiB RecvMaxSize default silently discards larger messages;
    // view results (and any future bulk) ride this channel, so raise it to the shared ceiling.
    ch.set_opt::<RecvMaxSize>(messages::RECV_MAX_SIZE)?;
    ch.set_opt::<SendTimeout>(Some(Duration::from_secs(10)))?;
    // Liveness: a dialed PAIR does not error its recv when the listener dies, so without
    // this a crashed/restarted orchestrator would strand the lane on a dead channel (an
    // orphan). The pipe-removal callback flags the drop; the bounded recv checks it and
    // exits cleanly.
    static DROPPED: AtomicBool = AtomicBool::new(false);
    ch.pipe_notify(|_pipe: Pipe, ev: PipeEvent| {
        if matches!(ev, PipeEvent::RemovePost) {
            DROPPED.store(true, Ordering::Relaxed);
        }
    })?;
    ch.set_opt::<RecvTimeout>(Some(Duration::from_secs(1)))?;
    ch.dial(&channel_url)?;

    // 3. Serve data_open work, one conversion at a time (single lane).
    loop {
        let raw = match ch.recv() {
            Ok(raw) => raw,
            Err(nng::Error::TimedOut) => {
                if DROPPED.load(Ordering::Relaxed) {
                    println!("[data] orchestrator gone — exiting");
                    return Ok(());
                }
                continue;
            }
            Err(e) => {
                eprintln!("[data] channel closed: {e}");
                return Ok(());
            }
        };
        let Some(env) = deframe(&raw[..]) else {
            eprintln!("[data] undecodable frame — skipping");
            continue;
        };
        let Message::Work(w) = env.body else {
            println!("[data] ignoring non-work message {:?}", env.body);
            continue;
        };
        let WorkPayload::Data(d) = &w.payload else {
            eprintln!("[data] work_id={} is not data work — ignoring", w.work_id);
            continue;
        };
        if d.op != DataOp::Open && d.op != DataOp::View {
            let reply = result_env(
                env.session_id.clone(),
                &w.work_id,
                w.revision,
                Status::FatalError,
                data_error(format!(
                    "this lane only serves data_open and data_view (got {:?})",
                    d.op
                )),
            );
            let _ = ch.send(frame_envelope(&reply).as_slice());
            continue;
        }
        if d.op == DataOp::Open && d.format != "csv" {
            let reply = result_env(
                env.session_id.clone(),
                &w.work_id,
                w.revision,
                Status::FatalError,
                data_error(format!(
                    "this lane only serves data_open for csv (got '{}')",
                    d.format
                )),
            );
            let _ = ch.send(frame_envelope(&reply).as_slice());
            continue;
        }

        // ── data_view: slice the cache, render the window, ship the TSV as the binary tail ──
        if d.op == DataOp::View {
            println!(
                "[data] data_view work_id={} cache='{}' offset={} limit={:?}",
                w.work_id, d.cache_path, d.row_offset, d.row_limit
            );
            let view = arrowview::serve(
                &d.cache_path,
                &arrowview::ViewRequest {
                    row_offset: d.row_offset,
                    row_limit: d.row_limit,
                    columns: d.columns.as_deref(),
                    max_bytes: d.max_bytes,
                    render: d.render.as_ref(),
                },
            );
            let (reply, tsv) = match view {
                Ok(out) => {
                    println!(
                        "[data] work_id={} view: {} of {} rows, {} bytes{}",
                        w.work_id,
                        out.row_count,
                        out.rows_total,
                        out.tsv.len(),
                        if out.truncated { " (truncated)" } else { "" }
                    );
                    (
                        result_env(
                            env.session_id.clone(),
                            &w.work_id,
                            w.revision,
                            Status::Complete,
                            messages::DataResult {
                                dataset_id: None,
                                dataset_revision: None,
                                rows: Some(out.rows_total),
                                schema: None,
                                error_message: None,
                                row_offset: Some(out.row_offset),
                                row_count: Some(out.row_count),
                                truncated: Some(out.truncated),
                            },
                        ),
                        out.tsv,
                    )
                }
                Err(e) => {
                    eprintln!("[data] work_id={} view failed: {e}", w.work_id);
                    (
                        result_env(
                            env.session_id.clone(),
                            &w.work_id,
                            w.revision,
                            Status::FatalError,
                            data_error(e),
                        ),
                        Vec::new(),
                    )
                }
            };
            // §18.2: `format` names the binary encoding; required iff a binary part is present.
            let mut reply = reply;
            let frame = if tsv.is_empty() {
                frame_envelope(&reply)
            } else {
                reply.format = Some("text/tsv".to_string());
                frame_parts(
                    &serde_json::to_vec(&reply).expect("serialize envelope"),
                    &tsv,
                )
            };
            if let Err(e) = ch.send(frame.as_slice()).map_err(|(_, e)| e) {
                eprintln!("[data] result send failed: {e} — exiting");
                return Ok(());
            }
            continue;
        }

        println!(
            "[data] data_open work_id={} source='{}' -> {}",
            w.work_id, d.source, d.cache_path
        );
        // The orchestrator assigned the cache path (identity); the lane creates the dir and
        // writes the file (I/O).
        let converted: Result<csv2arrow::ConvertOutput, Box<dyn std::error::Error>> = (|| {
            if let Some(parent) = std::path::Path::new(&d.cache_path).parent()
                && !parent.as_os_str().is_empty()
            {
                std::fs::create_dir_all(parent)?;
            }
            csv2arrow::convert(&d.source, &d.cache_path, &d.ingest)
        })();
        let reply = match converted {
            Ok(out) => {
                println!(
                    "[data] work_id={} converted: {} rows, {} columns -> {}",
                    w.work_id,
                    out.rows,
                    out.columns.len(),
                    d.cache_path
                );
                result_env(
                    env.session_id.clone(),
                    &w.work_id,
                    w.revision,
                    Status::Complete,
                    data_payload(&out),
                )
            }
            Err(e) => {
                eprintln!("[data] work_id={} conversion failed: {e}", w.work_id);
                result_env(
                    env.session_id.clone(),
                    &w.work_id,
                    w.revision,
                    Status::FatalError,
                    data_error(format!("could not open '{}': {e}", d.source)),
                )
            }
        };
        if let Err(e) = ch
            .send(frame_envelope(&reply).as_slice())
            .map_err(|(_, e)| e)
        {
            eprintln!("[data] result send failed: {e} — exiting");
            return Ok(());
        }
    }
}
