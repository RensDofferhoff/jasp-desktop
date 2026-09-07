//! End-to-end VIEW flow over the REAL v2 binaries (orchestrator-v2 + data-runner).
//!
//! The §8 implied-build story, running: a stapled analysis work misses the view
//! book → the router orders `cache_fill` to the (provisioner-spawned) data worker →
//! the worker materializes the AV5 artifact (Feather V2 + LZ4, `<token>_<type>`
//! fields — the D11 storage token plus the cast type, which IS the R alias —
//! `__base_row`, `jasp:view` metadata) → `cache_filled` un-parks the work →
//! dispatch carries the resolved `view_refs` next to the untouched `dataset_paths`
//! migration bridge. The analysis runner is a mock (views are router/worker
//! business; the jaspBase read seam is migration step 3) — but the blob it is handed
//! is REAL, and this test opens it with Arrow and checks the §8.3 coercions actually
//! happened.
//!
//! Spawns `jasp-orchestrator-v2` (the data worker is auto-provisioned from its
//! binary sibling, the production flow). Mirrors classic's `dataset_e2e.rs` harness.

use arrow::array::{Array, DictionaryArray, Float64Array, Int32Array, StringArray};
use arrow::datatypes::Int32Type;
use arrow_ipc::reader::FileReader;
use nng::options::{Options, RecvBufferSize, RecvTimeout, SendBufferSize};
use nng::{Protocol, Socket};
use serde_json::{Value, json};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use wire::framing::{deframe, frame_envelope};
use wire::{
    AnalysisWork, Capability, Envelope, Message, Register, ResultMsg, ResultPayload, Settings,
    Status, ViewColumn, ViewLevel, ViewSpec, Work, WorkPayload,
};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The D11 storage token of a display name (the spec vocabulary + the blob field front
/// segment): `jasp_enc_hex_` + lowercase hex of the UTF-8 bytes.
fn tok(display: &str) -> String {
    let hex: String = display.bytes().map(|b| format!("{b:02x}")).collect();
    format!("jasp_enc_hex_{hex}")
}

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

fn envelope(body: Message) -> Envelope {
    Envelope {
        v: 1,
        id: format!("e2e-{}", SEQ.fetch_add(1, Ordering::SeqCst)),
        reply_to: None,
        session_id: None,
        format: None,
        ts: None,
        body,
    }
}

/// REQ dial with retries — the orchestrator child may not have bound yet.
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

fn pair_dial(url: &str) -> Socket {
    let ch = Socket::new(Protocol::Pair1).unwrap();
    ch.set_opt::<SendBufferSize>(64).unwrap();
    ch.set_opt::<RecvBufferSize>(64).unwrap();
    ch.set_opt::<RecvTimeout>(Some(Duration::from_secs(5)))
        .unwrap();
    ch.dial(url).unwrap();
    ch
}

/// Frontend handshake: REQ hello → welcome → PAIR dial; consumes the catalog push.
fn hello_frontend(url: &str) -> (Socket, String) {
    let req = dial_req(url);
    req.send(
        frame_envelope(&envelope(Message::Hello(wire::Hello {
            client_id: Some("views-e2e".into()),
            client_version: Some("0.0.0".into()),
        })))
        .as_slice(),
    )
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
    let ch = pair_dial(&channel_url);
    let first = deframe(&ch.recv().expect("initial modules")[..]).expect("valid");
    assert!(matches!(first.body, Message::Modules(_)));
    (ch, session_id)
}

/// Runner handshake: REQ register → register_ack → PAIR dial.
fn register_runner(url: &str, caps: Vec<Capability>) -> Socket {
    let req = dial_req(url);
    req.send(
        frame_envelope(&envelope(Message::Register(Register {
            runner_id: None,
            capabilities: caps,
            priority: 0,
            slots: 1,
            environment: Value::Null,
        })))
        .as_slice(),
    )
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
    pair_dial(&channel_url)
}

/// Recv until a frame satisfies `pred`, handing back (raw, envelope) so injected
/// non-typed fields (`view_refs`, `dataset_paths`) can be asserted from the JSON.
fn recv_until(
    ch: &Socket,
    timeout: Duration,
    mut pred: impl FnMut(&Envelope) -> bool,
) -> (Vec<u8>, Envelope) {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .unwrap_or(Duration::from_millis(1));
        ch.set_opt::<RecvTimeout>(Some(remaining)).unwrap();
        let raw = ch.recv().expect("frame within timeout");
        let env = deframe(&raw[..]).expect("valid frame");
        if pred(&env) {
            return (raw.to_vec(), env);
        }
        assert!(
            Instant::now() < deadline,
            "no matching frame before the deadline"
        );
    }
}

fn is_terminal(env: &Envelope) -> bool {
    matches!(
        &env.body,
        Message::Result(r) if r.status != Status::Running
    )
}

/// The raw JSON of a frame (injected fields live here, not on the typed `Work`).
fn frame_json(raw: &[u8]) -> Value {
    let len = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    serde_json::from_slice(&raw[4..4 + len]).unwrap()
}

fn analysis_result(session: &str, work_id: &str, revision: u64) -> Envelope {
    Envelope {
        v: 1,
        id: format!("done-{work_id}"),
        reply_to: None,
        session_id: Some(session.to_string()),
        format: None,
        ts: None,
        body: Message::Result(ResultMsg {
            work_id: work_id.to_string(),
            revision,
            status: Status::Complete,
            payload: ResultPayload::AnalysisRClassicJaspbase(wire::AnalysisResult {
                results: json!({"title": "ok"}),
                results_dir: None,
                images: None,
            }),
            module_version: None,
            message: None,
        }),
    }
}

/// **The whole §8 implied build over real processes.** The provisioner spawns the
/// real data worker; the real builder materializes the view; the mock analysis
/// runner is handed real `view_refs`; the blob passes an Arrow-level coercion check.
#[test]
fn stapled_view_fills_builds_and_dispatches_e2e() {
    let seq = SEQ.fetch_add(1, Ordering::SeqCst);
    let url = format!("ipc:///tmp/jasp-v2-views-{seq}-{}.sock", std::process::id());
    let dir_root = format!("/tmp/jasp-v2-views-root-{seq}-{}", std::process::id());
    let _ = std::fs::remove_dir_all(&dir_root);
    let _orch = Proc::spawn(
        env!("CARGO_BIN_EXE_jasp-orchestrator-v2"),
        &[
            ("JASP_ORCH_URL", url.clone()),
            ("JASP_ORCH_DIR_ROOT", dir_root.clone()),
        ],
    );

    let runner = register_runner(
        &url,
        vec![Capability::AnalysisRClassicJaspbase {
            name: "jaspTTests".into(),
            version: "0.1".into(),
            base_uri: None,
        }],
    );
    let (fe, session) = hello_frontend(&url);

    // 1. Open debug.csv (the provisioner spawns the real data worker on demand).
    let csv = format!(
        "{}/../../../test_data/debug.csv",
        env!("CARGO_MANIFEST_DIR")
    );
    let open = envelope(Message::Work(Work {
        work_id: "open-1".into(),
        revision: 0,
        base_revision: None,
        dataset_ids: vec![],
        views: None,
        payload: WorkPayload::Data(wire::DataWork {
            op: wire::DataOp::Open,
            source: csv,
            cache_path: String::new(),
            format: "csv".into(),
            ingest: wire::IngestParams::default(),
            row_offset: 0,
            row_limit: None,
            columns: None,
            max_bytes: wire::VIEW_CHUNK_BYTES,
            render: None,
            edit: None,
        }),
    }));
    fe.send(frame_envelope(&open).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let (_raw, opened) = recv_until(&fe, Duration::from_secs(30), is_terminal);
    let dataset_id = match &opened.body {
        Message::Result(r) => match &r.payload {
            ResultPayload::Data(d) => {
                assert_eq!(
                    r.status,
                    Status::Complete,
                    "open failed: {:?}",
                    d.error_message
                );
                assert_eq!(d.rows, Some(6), "debug.csv has 6 rows");
                d.dataset_id.clone().expect("dataset_id stamped")
            }
            other => panic!("expected data payload, got {other:?}"),
        },
        other => panic!("expected result, got {other:?}"),
    };

    // 2. Submit an analysis work with three stapled casts (by storage token, D11):
    //    dict→nominal (group), f64→scale (x), f64→nominal (z — the r_character
    //    level-string path).
    let spec = |columns: Vec<(&str, ViewLevel)>| ViewSpec {
        dataset_id: dataset_id.clone(),
        columns: Some(
            columns
                .into_iter()
                .map(|(display, as_type)| ViewColumn {
                    name: tok(display),
                    as_type,
                })
                .collect(),
        ),
        filter: None,
        all: false,
    };
    let submit = envelope(Message::Work(Work {
        work_id: "V".into(),
        revision: 1,
        base_revision: None,
        dataset_ids: vec![dataset_id.clone()],
        views: Some(vec![spec(vec![
            ("group", ViewLevel::Nominal),
            ("x", ViewLevel::Scale),
            ("z", ViewLevel::Nominal),
        ])]),
        payload: WorkPayload::AnalysisRClassicJaspbase(AnalysisWork {
            module: "jaspTTests".into(),
            module_version: "0.1".into(),
            analysis: "A".into(),
            options: Value::Null,
            preload_data: None,
            settings: Settings {
                ppi: 96,
                num_decimals: 3,
            },
        }),
    }));
    fe.send(frame_envelope(&submit).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();

    // 3. The mock analysis runner gets the work — but only AFTER the implied build
    //    (parked → real fill → real cache_filled → dispatch). The deadline covers
    //    the worker's first boot if the open didn't already pay it.
    let (w_raw, _w_env) = recv_until(
        &runner,
        Duration::from_secs(30),
        |env| matches!(&env.body, Message::Work(w) if w.work_id == "V"),
    );
    let w_json = frame_json(&w_raw);
    let refs = w_json["view_refs"].as_array().expect("view_refs injected");
    assert_eq!(
        refs.len(),
        1,
        "one ref per SPEC (the unit is the dataset input): {refs:?}"
    );
    assert_eq!(refs[0]["dataset_id"], dataset_id);
    for r in refs {
        assert!(
            r["view_id"].is_string(),
            "materialized refs carry their id: {r:?}"
        );
        let path = r["path"].as_str().unwrap();
        assert!(std::path::Path::new(path).exists(), "blob {path} exists");
    }
    // The migration bridge rides along untouched.
    assert!(
        w_json["dataset_paths"][&dataset_id].is_string(),
        "dataset_paths still injected"
    );

    // 4. Complete the work; the frontend sees the terminal.
    runner
        .send(frame_envelope(&analysis_result(&session, "V", 1)).as_slice())
        .map_err(|(_, e)| e)
        .unwrap();
    let (_d_raw, done) = recv_until(
        &fe,
        Duration::from_secs(10),
        |env| matches!(&env.body, Message::Result(r) if r.work_id == "V" && r.status != Status::Running),
    );
    assert!(matches!(&done.body, Message::Result(r) if r.status == Status::Complete));

    // 5. The blob is REAL and passes the §8.3 coercion check: find it under the
    //    session's views dir (content-addressed, one file).
    let views_dir = format!("{dir_root}/{session}/views");
    let mut blobs: Vec<std::path::PathBuf> = std::fs::read_dir(&views_dir)
        .expect("the session views dir exists")
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "arrow"))
        .collect();
    blobs.sort();
    assert_eq!(blobs.len(), 1, "exactly one view blob: {blobs:?}");
    let blob = &blobs[0];

    let reader = FileReader::try_new(std::fs::File::open(blob).unwrap(), None).unwrap();
    let schema = reader.schema();
    let batch = reader.into_iter().next().unwrap().unwrap();

    // Field names: `<token>_<type>` in spec order + the base-row index (D11 — the
    // token plus the cast type IS the R alias).
    let names: Vec<String> = schema
        .fields()
        .iter()
        .map(|f| f.name().to_string())
        .collect();
    assert_eq!(
        names,
        vec![
            format!("{}_nominal", tok("group")),
            format!("{}_scale", tok("x")),
            format!("{}_nominal", tok("z")),
            "__base_row".to_string()
        ]
    );

    // group (dict→nominal): dictionary A/B, keys passthrough.
    let group = batch
        .column(0)
        .as_any()
        .downcast_ref::<DictionaryArray<Int32Type>>()
        .unwrap();
    let gvals = group
        .values()
        .as_any()
        .downcast_ref::<StringArray>()
        .unwrap();
    let glevels: Vec<&str> = (0..gvals.len()).map(|i| gvals.value(i)).collect();
    assert_eq!(glevels, vec!["A", "B"]);
    let gkeys: Vec<Option<i32>> = (0..group.len())
        .map(|i| {
            if group.is_null(i) {
                None
            } else {
                Some(group.keys().value(i))
            }
        })
        .collect();
    assert_eq!(
        gkeys,
        vec![Some(0), Some(0), Some(0), Some(1), Some(1), Some(1)]
    );
    assert!(
        !schema.field(0).dict_is_ordered().unwrap_or(false),
        "nominal is unordered"
    );

    // x (f64→scale): verbatim copies.
    let x = batch
        .column(1)
        .as_any()
        .downcast_ref::<Float64Array>()
        .unwrap();
    let xs: Vec<f64> = (0..x.len()).map(|i| x.value(i)).collect();
    assert_eq!(xs, vec![1.5, 2.1, 1.8, 5.2, 5.8, 4.9]);

    // z (f64→nominal): levels NUMERICALLY sorted → "98","100","105","195","200","203".
    let z = batch
        .column(2)
        .as_any()
        .downcast_ref::<DictionaryArray<Int32Type>>()
        .unwrap();
    let zvals = z.values().as_any().downcast_ref::<StringArray>().unwrap();
    let zlevels: Vec<&str> = (0..zvals.len()).map(|i| zvals.value(i)).collect();
    assert_eq!(zlevels, vec!["98", "100", "105", "195", "200", "203"]);
    let zkeys: Vec<i32> = (0..z.len()).map(|i| z.keys().value(i)).collect();
    assert_eq!(zkeys, vec![1, 2, 0, 4, 3, 5]); // 100→"100", 105→"105", 98→"98", …

    // The base-row index: dense 0-based.
    let base_row = batch
        .column(3)
        .as_any()
        .downcast_ref::<Int32Array>()
        .unwrap();
    let rows: Vec<i32> = (0..base_row.len()).map(|i| base_row.value(i)).collect();
    assert_eq!(rows, vec![0, 1, 2, 3, 4, 5]);

    // The `jasp:view` metadata: the decode authority.
    let meta: Value = serde_json::from_str(schema.metadata().get("jasp:view").unwrap()).unwrap();
    assert_eq!(meta["format_version"], wire::VIEW_FORMAT_VERSION);
    assert_eq!(meta["base_revision"], 0);
    assert_eq!(meta["dataset_id"], dataset_id);
    assert_eq!(meta["rows"], 6);
    assert_eq!(meta["base_row_column"], "__base_row");
    // The blob's file name IS the view_id, and the metadata agrees.
    let file_id = blob.file_stem().unwrap().to_string_lossy().into_owned();
    assert_eq!(meta["view_id"], file_id.as_str());
    // The token map: display_name IS the decode (D11) — token → display.
    assert_eq!(meta["token_map"][tok("group")], "group");
    assert_eq!(meta["token_map"][tok("group")].as_str().unwrap(), "group");
    let _ = std::fs::remove_dir_all(&dir_root);
}
