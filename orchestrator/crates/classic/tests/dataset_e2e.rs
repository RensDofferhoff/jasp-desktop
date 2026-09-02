//! End-to-end dataset flow over the REAL binaries (orchestrator + data-runner).
//!
//! Spawns `jasp-orchestrator` and `jasp-data-runner` as child processes on a unique ipc
//! control endpoint, then drives the full settled flow as a mock frontend + mock analysis
//! runner (dataset-manager-design §5.1/§5.3):
//!
//!   dataset_open{debug.csv, format_hint: "csv", ingest}
//!     → data-runner converts CSV → Feather at the assigned cache path
//!   dataset_open work {kind:data, op:data_open, source, format, ingest}
//!     → data-runner converts CSV → Feather at the assigned cache path
//!     → terminal result {dataset_id, schema, rows}
//!     → work {dataset_ids: [dataset_id]}
//!     → analysis runner receives dataset_paths[id] = cache path
//!     → runner reads the converted Feather (arrow) — proving the file is real
//!
//! Plus the lane-error path: a `dataset_open` of a missing source fails the open with a
//! correlated `dataset_open_failed` error.

use arrow::array::{Array, AsArray, DictionaryArray};
use arrow::datatypes::{DataType, Int32Type};
use arrow_ipc::reader::FileReader;
use nng::options::{Options, RecvBufferSize, RecvTimeout, SendBufferSize};
use nng::{Protocol, Socket};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use wire as messages;
use wire::framing::{deframe, deframe_parts, frame_envelope};
use wire::{
    AnalysisResult, AnalysisWork, Capability, DataResult, Envelope, Message, Register, ResultMsg,
    ResultPayload, Settings, Status, Work, WorkPayload,
};

// ─── Framing (§18.1) — shared in `wire::framing` ──────────────────────────

// ─── Child-process guard ─────────────────────────────────────────────────────

static SEQ: AtomicU64 = AtomicU64::new(0);

/// A spawned child killed on drop (also on test panic).
struct Proc {
    child: Option<Child>,
}

impl Proc {
    fn spawn(bin: &str, envs: &[(&str, String)]) -> Proc {
        let mut cmd = Command::new(bin);
        for (k, v) in envs {
            cmd.env(k, v);
        }
        cmd.stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        Proc {
            child: Some(cmd.spawn().expect("spawn child")),
        }
    }
}

impl Drop for Proc {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn envelope(id: &str, body: Message) -> Envelope {
    Envelope {
        v: 1,
        id: id.to_string(),
        reply_to: None,
        session_id: None,
        format: None,
        ts: None,
        body,
    }
}

/// REQ dial with retries — the orchestrator child may not have bound its control endpoint
/// yet when we first try.
fn dial_req(url: &str) -> Socket {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let req = Socket::new(Protocol::Req0).unwrap();
        req.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
            .unwrap();
        match req.dial(url) {
            Ok(()) => return req,
            Err(e) => {
                assert!(
                    Instant::now() < deadline,
                    "orchestrator never came up on {url}: {e}"
                );
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Frontend handshake: REQ hello → welcome{session_id, channel_url} → PAIR dial. The first
/// data-channel frame is the connect-time modules push; consume it here.
fn hello_frontend(url: &str) -> (Socket, String) {
    let req = dial_req(url);
    let hello = envelope(
        "fe-hello",
        Message::Hello(messages::Hello {
            client_id: Some("e2e-frontend".into()),
            client_version: Some("0.0.0".into()),
        }),
    );
    req.send(frame_envelope(&hello).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let welcome = deframe(&req.recv().expect("welcome")[..]).expect("valid welcome");
    let session_id = welcome.session_id.clone().expect("session_id");
    let channel_url = match welcome.body {
        Message::Welcome(w) => {
            assert!(w.ok, "hello accepted: {:?}", w.error);
            w.channel_url.expect("channel_url")
        }
        other => panic!("expected welcome, got {other:?}"),
    };
    drop(req);
    let ch = Socket::new(Protocol::Pair1).unwrap();
    ch.set_opt::<SendBufferSize>(64).unwrap();
    ch.set_opt::<RecvBufferSize>(64).unwrap();
    ch.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
        .unwrap();
    ch.dial(&channel_url).unwrap();
    // Consume the connect-time catalog push.
    let first = deframe(&ch.recv().expect("initial modules")[..]).expect("valid modules");
    assert!(
        matches!(first.body, Message::Modules(_)),
        "first frame is the catalog push, got {:?}",
        first.body
    );
    (ch, session_id)
}

/// Runner handshake: REQ register → register_ack{channel_url} → PAIR dial.
fn register_runner(url: &str, caps: Vec<Capability>) -> Socket {
    let req = dial_req(url);
    let reg = envelope(
        "rn-reg",
        Message::Register(Register {
            runner_id: None,
            capabilities: caps,
            priority: 0,
            slots: 1,
            environment: Value::Null,
        }),
    );
    req.send(frame_envelope(&reg).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let channel_url = match deframe(&req.recv().expect("register_ack")[..])
        .expect("valid ack")
        .body
    {
        Message::RegisterAck(a) => {
            assert!(a.ok, "registration accepted: {:?}", a.reason);
            a.channel_url.expect("channel_url")
        }
        other => panic!("expected register_ack, got {other:?}"),
    };
    drop(req);
    let ch = Socket::new(Protocol::Pair1).unwrap();
    ch.set_opt::<SendBufferSize>(64).unwrap();
    ch.set_opt::<RecvBufferSize>(64).unwrap();
    ch.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
        .unwrap();
    ch.dial(&channel_url).unwrap();
    ch
}

/// Recv frames on `ch` until one satisfies `pred` (skipping unrelated frames such as
/// modules pushes), within `timeout`.
fn recv_until(ch: &Socket, timeout: Duration, mut pred: impl FnMut(&Envelope) -> bool) -> Envelope {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_millis(1));
        ch.set_opt::<RecvTimeout>(Some(remaining)).unwrap();
        let raw = ch.recv().expect("frame within timeout");
        let env = deframe(&raw[..]).expect("valid frame");
        if pred(&env) {
            return env;
        }
        assert!(
            Instant::now() < deadline,
            "no matching frame before the deadline"
        );
    }
}

/// Spawn the orchestrator on a fresh ipc endpoint; the data lane is AUTO-PROVISIONED by its
/// provisioner (the `jasp-data-runner` sibling of the orchestrator binary), which is exactly
/// the production flow under test. Returns the guard, the URL, and the orchestrator's dir
/// root.
fn spawn_orch(name: &str) -> (Proc, String, String) {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let url = format!(
        "ipc:///tmp/jasp-e2e-{name}-{}-{seq}.sock",
        std::process::id()
    );
    let dir_root = format!("/tmp/jasp-e2e-root-{name}-{}-{seq}", std::process::id());
    let _ = std::fs::remove_dir_all(&dir_root);
    let orch = Proc::spawn(
        env!("CARGO_BIN_EXE_jasp-orchestrator"),
        &[
            ("JASP_ORCH_URL", url.clone()),
            ("JASP_ORCH_DIR_ROOT", dir_root.clone()),
        ],
    );
    (orch, url, dir_root)
}

/// Submit a `data_open` work and wait for its TERMINAL result, skipping `Running` park
/// markers (the open parks while the provisioner-spawned lane boots and registers). Returns
/// `(status, data payload)`; the Data payload carries `dataset_id`/`schema`/`rows` on success.
fn open_dataset(fe: &Socket, path: &str, work_id: &str) -> (Status, DataResult) {
    let open = envelope(
        work_id,
        Message::Work(Work {
            work_id: work_id.to_string(),
            revision: 0,
            base_revision: None,
            dataset_ids: Vec::new(),
            payload: WorkPayload::Data(messages::DataWork {
                op: messages::DataOp::Open,
                source: path.to_string(),
                cache_path: String::new(), // orchestrator assigns it at dispatch
                format: "csv".to_string(),
                ingest: messages::IngestParams::default(),
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: messages::VIEW_CHUNK_BYTES,
                render: None,
                edit: None,
            }),
        }),
    );
    fe.send(frame_envelope(&open).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let answer = recv_until(fe, Duration::from_secs(60), |e| {
        matches!(&e.body, Message::Result(r)
            if r.work_id == work_id && !matches!(r.status, Status::Running))
    });
    let Message::Result(r) = answer.body else {
        panic!("expected result");
    };
    let ResultPayload::Data(d) = r.payload else {
        panic!("expected data result payload");
    };
    (r.status, d)
}

// ─── The full loop ───────────────────────────────────────────────────────────

#[test]
fn csv_open_work_and_feather_read() {
    let (_orch, url, dir_root) = spawn_orch("full");
    let csv = format!(
        "{}/../../../test_data/debug.csv",
        env!("CARGO_MANIFEST_DIR")
    );

    // 1. data_open work → the orchestrator provisions the data-runner, which converts
    //    debug.csv; the terminal result is a kind:"data" result: the orchestrator-filled
    //    dataset_id plus the lane's {schema, rows} on the Data payload. (A `Running` park
    //    marker may precede it while the lane boots.)
    let (fe, session) = hello_frontend(&url);
    let (status, data) = open_dataset(&fe, &csv, "w-open");
    assert!(matches!(status, Status::Complete), "open failed: {data:?}");
    let dataset_id = data
        .dataset_id
        .expect("dataset_id filled into the Data payload");
    assert_eq!(data.rows, Some(6), "debug.csv has 6 data rows");
    let schema = data.schema.expect("schema on the Data payload");
    let cols = schema.as_array().expect("schema is a column array");
    assert_eq!(cols.len(), 4, "group, x, y, z");
    let col = |name: &str| -> Value {
        cols.iter()
            .find(|c| c["name"] == name)
            .unwrap_or_else(|| panic!("column {name} in schema: {cols:?}"))
            .clone()
    };
    assert_eq!(col("group")["type"], "nominal");
    assert_eq!(col("group")["levels"], json!(["A", "B"]));
    assert_eq!(col("x")["type"], "scale");
    assert_eq!(col("y")["type"], "scale");
    assert_eq!(col("z")["type"], "ordinal");
    assert_eq!(
        col("z")["levels"],
        json!(["98", "100", "105", "195", "200", "203"]),
        "value-ordered dictionary levels"
    );
    // Constraint-check stats (data-model-design.md §2): value_count = non-empty cells (zero
    // IS a value); distinct_count exact (≤ cap); numeric_levels for categoricals only.
    assert_eq!(col("group")["value_count"], 6);
    assert_eq!(col("group")["distinct_count"], 2);
    assert_eq!(col("group")["numeric_levels"], 0, "A/B are not numeric");
    assert_eq!(col("x")["value_count"], 6);
    assert_eq!(col("x")["distinct_count"], 6);
    assert!(
        col("x").get("numeric_levels").is_none(),
        "scale columns carry no numeric_levels"
    );
    assert_eq!(col("z")["value_count"], 6);
    assert_eq!(col("z")["distinct_count"], 6);
    assert_eq!(col("z")["numeric_levels"], 6, "all z levels are numeric");

    // 2. The cache file exists where the orchestrator assigned it.
    let cache_path = std::path::Path::new(&dir_root)
        .join(&session)
        .join("datasets")
        .join(format!("{}_0.arrow", dataset_id));
    assert!(cache_path.exists(), "cache file {}", cache_path.display());

    // 3. Register an analysis runner; submit work referencing the dataset_id.
    let runner = register_runner(
        &url,
        vec![Capability::AnalysisRClassicJaspbase {
            name: "jaspE2E".into(),
            version: "0.1".into(),
            base_uri: None,
        }],
    );
    let work = envelope(
        "fe-work-1",
        Message::Work(Work {
            work_id: "w-e2e".into(),
            revision: 0,
            base_revision: None,
            dataset_ids: vec![dataset_id.clone()],
            payload: WorkPayload::AnalysisRClassicJaspbase(AnalysisWork {
                module: "jaspE2E".into(),
                module_version: "0.1".into(),
                analysis: "E2E".into(),
                options: Value::Null,
                // Pruning wire contract (§3.1): the flag must survive the orchestrator's
                // typed round-trip and reach the runner exactly as sent.
                preload_data: Some(false),
                settings: Settings {
                    ppi: 96,
                    num_decimals: 3,
                },
            }),
        }),
    );
    fe.send(frame_envelope(&work).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();

    // 4. The runner receives the work with the dataset resolved into dataset_paths.
    let raw = runner.recv().expect("runner recv work");
    let work_env = deframe(&raw[..]).expect("valid work");
    let work_session = work_env.session_id.clone().expect("session stamped");
    assert_eq!(work_session, session);
    let injected: Value = serde_json::from_slice(&raw[4..]).unwrap();
    assert_eq!(
        injected["dataset_paths"][&dataset_id],
        json!(cache_path.to_string_lossy()),
        "dataset id resolved to the converted cache path"
    );
    assert!(injected.get("output_dir").is_some());
    assert_eq!(
        injected["payload"]["preloadData"],
        json!(false),
        "preloadData rides the wire to the runner (HANDOVER-runner-data-pruning.md §3.1/§8.5a); \
         the walk/pruning/alias behavior itself is covered by refactor_design/tests/walk_test.R \
         (R level) and the release-lane GUI validation"
    );

    // 5. The runner reads the converted Feather — it is a real Arrow file.
    let file = std::fs::File::open(&cache_path).unwrap();
    let reader = FileReader::try_new(file, None).expect("open Feather");
    let schema = reader.schema();
    assert_eq!(schema.fields().len(), 4);
    let by_name = |n: &str| {
        schema
            .field_with_name(n)
            .unwrap_or_else(|_| panic!("field {n}"))
            .clone()
    };
    assert!(matches!(by_name("x").data_type(), DataType::Float64));
    assert!(matches!(by_name("y").data_type(), DataType::Float64));
    let group = by_name("group");
    assert!(matches!(
        group.data_type(),
        DataType::Dictionary(k, v) if **k == DataType::Int32 && **v == DataType::Utf8
    ));
    assert_eq!(group.dict_is_ordered(), Some(false), "nominal is unordered");
    assert_eq!(
        group
            .metadata()
            .get("jasp:auto_sort_by_value")
            .map(String::as_str),
        Some("true")
    );
    let z = by_name("z");
    assert_eq!(z.dict_is_ordered(), Some(true), "ordinal is ordered");
    assert_eq!(
        z.metadata().get("jasp:display_name").map(String::as_str),
        Some("z")
    );
    let mut rows_read = 0usize;
    let mut first_group: Option<String> = None;
    for batch in reader.flatten() {
        rows_read += batch.num_rows();
        if first_group.is_none() {
            let dict = batch
                .column(0)
                .as_any()
                .downcast_ref::<DictionaryArray<Int32Type>>()
                .expect("group is dictionary-encoded");
            let keys = dict.keys();
            let values = dict.values();
            let sa = values.as_string::<i32>();
            let key = keys.value(0);
            assert!(dict.is_valid(0));
            first_group = Some(sa.value(key as usize).to_string());
        }
    }
    assert_eq!(rows_read, 6, "Feather holds all rows");
    assert_eq!(first_group.as_deref(), Some("A"));

    // 6. Terminal result round-trips to the frontend.
    let result = Envelope {
        v: 1,
        id: "rn-result-1".into(),
        reply_to: None,
        session_id: Some(work_session),
        format: None,
        ts: None,
        body: Message::Result(ResultMsg {
            work_id: "w-e2e".into(),
            revision: 0,
            status: Status::Complete,
            payload: ResultPayload::AnalysisRClassicJaspbase(AnalysisResult {
                results: json!({"title": "ok"}),
                results_dir: None,
                images: None,
            }),
            module_version: None,
            message: None,
        }),
    };
    runner
        .send(frame_envelope(&result).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let final_env = recv_until(
        &fe,
        Duration::from_secs(10),
        |e| matches!(&e.body, Message::Result(r) if r.work_id == "w-e2e"),
    );
    match final_env.body {
        Message::Result(r) => assert!(matches!(r.status, Status::Complete)),
        other => panic!("expected result, got {other:?}"),
    }
}

// ─── The lane-error path ─────────────────────────────────────────────────────

#[test]
fn csv_open_of_a_missing_source_fails_the_open() {
    let (_orch, url, _dir_root) = spawn_orch("fail");
    let (fe, _session) = hello_frontend(&url);

    // A data_open work for a missing source: the provisioned lane reports a fatalError.
    // open_dataset skips the `Running` park marker while the lane boots/registers.
    let (status, data) = open_dataset(&fe, "/nonexistent/no-such-file.csv", "w-open-missing");
    assert!(
        matches!(status, Status::FatalError),
        "expected fatalError, got {status:?}: {data:?}"
    );
    let msg = data.error_message.unwrap_or_default();
    assert!(
        msg.contains("no-such-file.csv"),
        "error names the source: {msg}"
    );
}

// ─── data_view: chunked windowed views of the converted cache ───────────────

/// Recv the RAW frame on `ch` whose envelope satisfies `pred` (skipping unrelated frames),
/// within `timeout` — returns the whole frame so the caller can inspect the binary tail.
fn recv_until_raw(
    ch: &Socket,
    timeout: Duration,
    mut pred: impl FnMut(&Envelope) -> bool,
) -> Vec<u8> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_millis(1));
        ch.set_opt::<RecvTimeout>(Some(remaining)).unwrap();
        let raw = ch.recv().expect("frame within timeout");
        let (env, _) = deframe_parts(&raw[..]).expect("valid frame");
        if pred(&env) {
            return raw.to_vec();
        }
        assert!(
            Instant::now() < deadline,
            "no matching frame before the deadline"
        );
    }
}

/// Submit a `data_view` work and wait for its TERMINAL result (skipping `Running` park
/// markers). Returns `(envelope, status, Data payload, binary tail)` — the tail is the
/// frame's binary part, the escaped TSV cells.
fn view_dataset(
    fe: &Socket,
    dataset_id: &str,
    work_id: &str,
    row_offset: u64,
    max_bytes: u64,
    render: Option<messages::ViewRender>,
) -> (Envelope, Status, DataResult, Vec<u8>) {
    let view = envelope(
        work_id,
        Message::Work(Work {
            work_id: work_id.to_string(),
            revision: 0,
            base_revision: None,
            dataset_ids: vec![dataset_id.to_string()],
            payload: WorkPayload::Data(messages::DataWork {
                op: messages::DataOp::View,
                source: String::new(),
                cache_path: String::new(), // orchestrator injects the dataset's current path
                format: String::new(),     // view is format-agnostic
                ingest: messages::IngestParams::default(),
                row_offset,
                row_limit: None,
                columns: None,
                max_bytes,
                render,
                edit: None,
            }),
        }),
    );
    fe.send(frame_envelope(&view).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let raw = recv_until_raw(fe, Duration::from_secs(60), |e| {
        matches!(&e.body, Message::Result(r)
            if r.work_id == work_id && !matches!(r.status, Status::Running))
    });
    let (env, tail) = deframe_parts(&raw[..]).expect("valid frame");
    let body = env.body.clone();
    let Message::Result(r) = body else {
        panic!("expected result");
    };
    let status = r.status;
    let ResultPayload::Data(d) = r.payload else {
        panic!("expected data result payload");
    };
    (env, status, d, tail.to_vec())
}

/// Open CSV → chunked view matches the source values byte-exact. The whole dataset fits in
/// one chunk; lane-default render ('.' decimal, no grouping, 'g' at 10 — legacy parity:
/// integral values render without a decimal point); the envelope names the binary encoding
/// and the orchestrator stamps identity + revision.
#[test]
fn view_matches_source_values_byte_exact() {
    let (_orch, url, _dir_root) = spawn_orch("view");
    let csv = format!(
        "{}/../../../test_data/debug.csv",
        env!("CARGO_MANIFEST_DIR")
    );
    let (fe, _session) = hello_frontend(&url);
    let (status, data) = open_dataset(&fe, &csv, "w-open");
    assert!(matches!(status, Status::Complete), "open failed: {data:?}");
    let dataset_id = data.dataset_id.expect("dataset_id");

    // Full view — one chunk, no truncation, binary tail byte-exact against the CSV.
    let (env, status, view, tsv) =
        view_dataset(&fe, &dataset_id, "w-view-full", 0, 1_000_000, None);
    assert!(matches!(status, Status::Complete), "view failed: {view:?}");
    assert_eq!(
        env.format.as_deref(),
        Some("text/tsv"),
        "envelope names the binary encoding"
    );
    assert_eq!(view.dataset_id.as_deref(), Some(dataset_id.as_str()));
    assert_eq!(
        view.dataset_revision,
        Some(0),
        "stamped with the revision at dispatch"
    );
    assert_eq!(view.rows, Some(6), "TOTAL rows at serve time");
    assert_eq!(view.row_offset, Some(0));
    assert_eq!(view.row_count, Some(6));
    assert_eq!(view.truncated, Some(false));
    let expected = concat!(
        "A\t1.5\t10.2\t100\n",
        "A\t2.1\t11.3\t105\n",
        "A\t1.8\t10.8\t98\n",
        "B\t5.2\t20.1\t200\n",
        "B\t5.8\t19.7\t195\n",
        "B\t4.9\t21\t203\n",
    );
    assert_eq!(
        tsv,
        expected.as_bytes(),
        "cells match the source CSV byte-exact"
    );

    // Locale render spec rides the request: nl/de-style separators change the bytes.
    let render = messages::ViewRender {
        decimal: ",".into(),
        thousands: ".".into(),
        precision: 10,
    };
    let (_env, status, view, tsv) = view_dataset(
        &fe,
        &dataset_id,
        "w-view-locale",
        0,
        1_000_000,
        Some(render),
    );
    assert!(matches!(status, Status::Complete), "view failed: {view:?}");
    assert!(
        tsv.starts_with(b"A\t1,5\t10,2\t100\n"),
        "locale separators applied by the lane: {:?}",
        &tsv[..20.min(tsv.len())]
    );
}

/// The chunk budget truncates at a ROW boundary (whole rows, `truncated` set) and the next
/// request continues at the frontier — the frontend-driven chunked fill contract.
#[test]
fn view_budget_truncates_at_row_boundary_and_continues() {
    let (_orch, url, _dir_root) = spawn_orch("viewtrunc");
    let csv = format!(
        "{}/../../../test_data/debug.csv",
        env!("CARGO_MANIFEST_DIR")
    );
    let (fe, _session) = hello_frontend(&url);
    let (status, data) = open_dataset(&fe, &csv, "w-open");
    assert!(matches!(status, Status::Complete), "open failed: {data:?}");
    let dataset_id = data.dataset_id.expect("dataset_id");

    // Row 0 renders to exactly 15 bytes ("A\t1.5\t10.2\t100\n"); a 15-byte budget fits
    // exactly one row — row 1 would exceed it, so the chunk stops at the boundary.
    let (_env, status, view, tsv) = view_dataset(&fe, &dataset_id, "w-view-1", 0, 15, None);
    assert!(matches!(status, Status::Complete), "view failed: {view:?}");
    assert_eq!(view.row_count, Some(1));
    assert_eq!(view.truncated, Some(true));
    assert_eq!(tsv, b"A\t1.5\t10.2\t100\n");

    // The next chunk continues at row_offset 1 and finishes the dataset.
    let (_env, status, view, tsv) = view_dataset(&fe, &dataset_id, "w-view-2", 1, 1_000_000, None);
    assert!(matches!(status, Status::Complete), "view failed: {view:?}");
    assert_eq!(view.row_offset, Some(1));
    assert_eq!(view.row_count, Some(5));
    assert_eq!(view.truncated, Some(false));
    assert!(tsv.starts_with(b"A\t2.1\t11.3\t105\n"));
    assert!(tsv.ends_with(b"B\t4.9\t21\t203\n"));
}

/// The encoding-torture dataset (CJK/keyword/empty column names, null cells) views
/// correctly: VALUES match the source byte-exact, nulls are whole-cell `\N`, and the
/// grammar stays rectangular (12 separators per row for 13 columns).
#[test]
fn view_of_encoding_torture_matches_source_values() {
    let (_orch, url, _dir_root) = spawn_orch("viewtorture");
    let csv = format!(
        "{}/../../../test_data/encoding_torture.csv",
        env!("CARGO_MANIFEST_DIR")
    );
    let (fe, _session) = hello_frontend(&url);
    let (status, data) = open_dataset(&fe, &csv, "w-open");
    assert!(matches!(status, Status::Complete), "open failed: {data:?}");
    let dataset_id = data.dataset_id.expect("dataset_id");
    assert_eq!(data.rows, Some(24));

    let (_env, status, view, tsv) =
        view_dataset(&fe, &dataset_id, "w-view-torture", 0, 1_000_000, None);
    assert!(matches!(status, Status::Complete), "view failed: {view:?}");
    assert_eq!(view.rows, Some(24));
    assert_eq!(view.row_count, Some(24));
    let text = std::str::from_utf8(&tsv).expect("TSV is UTF-8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 24, "one LF-terminated row per dataset row");
    // First row: every value byte-exact from the source CSV.
    assert_eq!(
        lines[0],
        "S01\t412.3\t78\t71.2\t0\tyes\t12.1\tcontrol\t1\t3.2\t5\t88\tA"
    );
    // Row S04: the missing reaction time is null — whole-cell `\N`.
    assert_eq!(
        lines[3],
        "S04\t\\N\t85\t69.1\t1\tno\t10.8\ttreated\t3\t2.7\t6\t95\tA"
    );
    // Rectangular: every row has exactly schema.len()-1 = 12 separators.
    for line in &lines {
        assert_eq!(line.matches('\t').count(), 12, "rectangular row: {line:?}");
    }
}

/// A view of an unknown dataset fails statelessly (`dataset_not_ready`) — the same guard
/// analysis work rides; nothing reaches a lane.
#[test]
fn view_of_unknown_dataset_errors() {
    let (_orch, url, _dir_root) = spawn_orch("viewunknown");
    let (fe, _session) = hello_frontend(&url);
    let view = envelope(
        "w-view-nope",
        Message::Work(Work {
            work_id: "w-view-nope".to_string(),
            revision: 0,
            base_revision: None,
            dataset_ids: vec!["ds-nope".to_string()],
            payload: WorkPayload::Data(messages::DataWork {
                op: messages::DataOp::View,
                source: String::new(),
                cache_path: String::new(),
                format: String::new(),
                ingest: messages::IngestParams::default(),
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: messages::VIEW_CHUNK_BYTES,
                render: None,
                edit: None,
            }),
        }),
    );
    fe.send(frame_envelope(&view).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let env = recv_until(&fe, Duration::from_secs(10), |e| {
        matches!(&e.body, Message::Error(_))
    });
    match env.body {
        Message::Error(e) => {
            assert_eq!(e.code, "dataset_not_ready");
            assert_eq!(e.work_id.as_deref(), Some("w-view-nope"));
        }
        other => panic!("expected error, got {other:?}"),
    }
}

// ─── The edit crown over the real lane (d8) ─────────────────────────────────

/// Submit an edit (any `EditOp`) with its §1.2 TSV / inverse-IPC tail; returns the
/// terminal DataResult (D6-stripped: identity + inverse only) and the result frame's
/// own tail — the inverse's Arrow-IPC bytes, stored VERBATIM for undo (D10: the
/// frontend never interprets them).
#[allow(clippy::too_many_arguments)]
fn edit_dataset(
    fe: &Socket,
    dataset_id: &str,
    work_id: &str,
    base_revision: u64,
    edit: messages::EditOp,
    tail: &[u8],
) -> (Status, DataResult, Vec<u8>) {
    let env = envelope(
        work_id,
        Message::Work(Work {
            work_id: work_id.to_string(),
            revision: base_revision, // D11: the CURRENT dataset revision (echo-only)
            base_revision: None,
            dataset_ids: vec![dataset_id.to_string()],
            payload: WorkPayload::Data(messages::DataWork {
                op: messages::DataOp::Edit,
                source: String::new(), // the orchestrator injects the pre-edit cache
                cache_path: String::new(), // …and assigns the NEXT revision's path
                format: String::new(),
                ingest: messages::IngestParams::default(),
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: messages::VIEW_CHUNK_BYTES,
                render: None,
                edit: Some(Box::new(edit)),
            }),
        }),
    );
    let mut frame = frame_envelope(&env);
    frame.extend_from_slice(tail);
    fe.send(&frame[..]).map_err(|(_, e)| e).unwrap();
    let raw = recv_until_raw(fe, Duration::from_secs(60), |e| {
        matches!(&e.body, Message::Result(r)
            if r.work_id == work_id && !matches!(r.status, Status::Running))
    });
    let (res_env, res_tail) = deframe_parts(&raw[..]).expect("valid frame");
    let Message::Result(r) = res_env.body else {
        panic!("expected result");
    };
    let status = r.status;
    let ResultPayload::Data(d) = r.payload else {
        panic!("expected data result payload");
    };
    (status, d, res_tail.to_vec())
}

/// The `data_changed` push that follows every edit's terminal result (same channel,
/// after it — §6 ordering).
fn recv_data_changed(fe: &Socket, dataset_id: &str) -> messages::DataChanged {
    let env = recv_until(
        fe,
        Duration::from_secs(30),
        |e| matches!(&e.body, Message::DataChanged(dc) if dc.dataset_id == dataset_id),
    );
    match env.body {
        Message::DataChanged(dc) => dc,
        other => panic!("expected data_changed, got {other:?}"),
    }
}

/// Row 0 of a view as a string (all columns, one chunk).
fn view_row0(fe: &Socket, dataset_id: &str, work_id: &str) -> String {
    let (_e, st, d, tsv) = view_dataset(fe, dataset_id, work_id, 0, 1_000_000, None);
    assert!(matches!(st, Status::Complete), "view failed: {d:?}");
    String::from_utf8_lossy(&tsv)
        .lines()
        .next()
        .expect("a rendered row")
        .to_string()
}

/// THE d8 crown, over the real binaries: open → edit (a DECLARED-schema paste, P13) →
/// edit (`schema_change`) → `data_changed` after every result (revision, rows, schema,
/// invalidation — D6's split) → views prove the cache → undo × 2 (apply_inverse with
/// the stored (meta, bytes) verbatim) → the ORIGINAL, view-verified → redo → the edited
/// state again. Both algebraic identities (undo∘edit = id, redo∘undo∘edit = edit) plus
/// the D11 revision ladder 0→5, all on real sockets with the now-advertised lane.
#[test]
fn edit_undo_redo_crown_over_the_real_lane() {
    let (_orch, url, _dir_root) = spawn_orch("crown");
    let csv = format!(
        "{}/../../../test_data/debug.csv",
        env!("CARGO_MANIFEST_DIR")
    );
    let (fe, _session) = hello_frontend(&url);
    let (status, data) = open_dataset(&fe, &csv, "w-crown-open");
    assert!(matches!(status, Status::Complete), "open failed: {data:?}");
    let id = data.dataset_id.expect("dataset_id");

    // HOP 1 — a DECLARED-schema paste (P13): x overwritten under a declared scale
    // postcondition (echo + adherence), y + z left to the auto path (null entries —
    // z is ordinal), and a new column Q4 (nominal, declared levels) created by the
    // same edit.
    let (status, res, inv1_bytes) = edit_dataset(
        &fe,
        &id,
        "w-crown-e1",
        0,
        messages::EditOp::InsertBlock {
            row: 0,
            col: 1,
            target_schema: Some(json!([
                { "name": "x", "type": "scale" },
                null,
                null,
                { "name": "Q4", "type": "nominal", "levels": ["P", "Q"] },
            ])),
        },
        b"9.75\t10.5\t100\tP\n",
    );
    assert!(matches!(status, Status::Complete), "e1 failed: {res:?}");
    assert_eq!(res.dataset_id.as_deref(), Some(id.as_str()));
    assert_eq!(
        res.dataset_revision,
        Some(1),
        "revision bumped (stamped post-apply)"
    );
    assert!(
        res.rows.is_none() && res.schema.is_none() && res.invalidation.is_none(),
        "D6 strip: the forwarded result carries identity + inverse only"
    );
    let inv1 = res.inverse.clone().expect("every edit carries its inverse");
    assert!(!inv1_bytes.is_empty(), "the paste's inverse has IPC bytes");
    let dc1 = recv_data_changed(&fe, &id);
    assert_eq!(dc1.dataset_revision, 1);
    assert_eq!(dc1.rows, Some(6));
    let s1 = dc1
        .schema
        .as_ref()
        .expect("schema changed (a column was created)");
    let cols1 = s1.as_array().unwrap();
    assert_eq!(cols1.len(), 5, "Q4 created");
    let q4 = cols1.iter().find(|c| c["name"] == "Q4").expect("Q4");
    assert_eq!(q4["levels"], json!(["P", "Q"]), "declared levels verbatim");
    assert_eq!(
        dc1.invalidation.all,
        Some(true),
        "a column-set change → all"
    );
    assert_eq!(
        view_row0(&fe, &id, "w-crown-v1"),
        "A\t9.75\t10.5\t100\tP",
        "the view shows the edited cache"
    );

    // HOP 2 — schema_change (the d6/d7b bonus hop): rename + relabel `group`. A
    // Keep-class change — JSON-only inverse, `{}` invalidation.
    let (status, res, inv2_bytes) = edit_dataset(
        &fe,
        &id,
        "w-crown-e2",
        1,
        messages::EditOp::SchemaChange {
            target_schema: json!([
                { "name": "group", "display_name": "Condition",
                  "labels": { "A": "alpha", "B": "beta" } },
                { "name": "x" }, { "name": "y" }, { "name": "z" }, { "name": "Q4" },
            ]),
        },
        b"",
    );
    assert!(matches!(status, Status::Complete), "e2 failed: {res:?}");
    let inv2 = res.inverse.clone().expect("inverse");
    assert!(inv2_bytes.is_empty(), "a Keep-class change is JSON-only");
    let dc2 = recv_data_changed(&fe, &id);
    assert_eq!(dc2.dataset_revision, 2);
    let s2 = dc2.schema.as_ref().unwrap().as_array().unwrap();
    let cond = s2
        .iter()
        .find(|c| c["display_name"] == "Condition")
        .expect("renamed column");
    assert_eq!(cond["labels"], json!({ "A": "alpha", "B": "beta" }));
    assert!(
        dc2.invalidation.all.is_none() && dc2.invalidation.rows_from.is_none(),
        "rename+labels-only → {{}}"
    );

    // UNDO HOP 2 — apply_inverse with the stored (meta, bytes) VERBATIM (LIFO).
    let (status, res, _redo2) = edit_dataset(
        &fe,
        &id,
        "w-crown-u2",
        2,
        messages::EditOp::ApplyInverse { inverse: inv2 },
        &inv2_bytes,
    );
    assert!(matches!(status, Status::Complete), "u2 failed: {res:?}");
    let dc3 = recv_data_changed(&fe, &id);
    assert_eq!(dc3.dataset_revision, 3);
    assert_eq!(
        dc3.invalidation.all,
        Some(true),
        "undo always all:true (P12)"
    );
    let s3 = dc3.schema.as_ref().unwrap().as_array().unwrap();
    assert!(
        s3.iter().all(|c| c["display_name"] != "Condition"),
        "the rename is undone"
    );

    // UNDO HOP 1 — the paste's restore_block (the IPC bytes ride the tail).
    let (status, res, redo1_bytes) = edit_dataset(
        &fe,
        &id,
        "w-crown-u1",
        3,
        messages::EditOp::ApplyInverse { inverse: inv1 },
        &inv1_bytes,
    );
    assert!(matches!(status, Status::Complete), "u1 failed: {res:?}");
    let redo1 = res
        .inverse
        .clone()
        .expect("the undo carries its own inverse (the redo)");
    let dc4 = recv_data_changed(&fe, &id);
    assert_eq!(dc4.dataset_revision, 4);
    assert_eq!(dc4.rows, Some(6));
    let s4 = dc4.schema.as_ref().unwrap().as_array().unwrap();
    assert_eq!(s4.len(), 4, "Q4 trimmed — the original schema restored");

    // The ORIGINAL, view-verified: undo ∘ edit = id over the real rail.
    assert_eq!(
        view_row0(&fe, &id, "w-crown-v2"),
        "A\t1.5\t10.2\t100",
        "UNDO IDENTITY FAILED over the real lane"
    );

    // REDO — the undo's own inverse re-pastes (overflow columns included):
    // redo ∘ undo ∘ edit = edit over the real rail.
    let (status, res, _rb) = edit_dataset(
        &fe,
        &id,
        "w-crown-r1",
        4,
        messages::EditOp::ApplyInverse { inverse: redo1 },
        &redo1_bytes,
    );
    assert!(matches!(status, Status::Complete), "redo failed: {res:?}");
    let dc5 = recv_data_changed(&fe, &id);
    assert_eq!(dc5.dataset_revision, 5, "revision only climbs");
    let s5 = dc5.schema.as_ref().unwrap().as_array().unwrap();
    assert_eq!(s5.len(), 5, "Q4 re-created by the redo");
    assert_eq!(
        view_row0(&fe, &id, "w-crown-v3"),
        "A\t9.75\t10.5\t100\tP",
        "REDO IDENTITY FAILED over the real lane"
    );
}
