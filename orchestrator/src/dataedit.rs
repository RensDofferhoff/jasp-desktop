//! NEO JASP data-edit engine — the lane side of the `data_edit` family
//! (refactor_design/data-edit-design.md §2–§6).
//!
//! Streaming shape (ladder rung 2 — memory O(one batch + edit + inverse), IO O(dataset)
//! per edit): the engine reads the pre-edit cache (`payload.source`) batch by batch — the
//! same `FileReader` machinery `data_view` rides — rewrites ONLY what an op touches
//! (untouched columns pass through as Arc clones), and writes the new revision's cache
//! (`payload.cache_path`) as batches are produced. The inverse is captured on the same
//! pass: the OLD cells of exactly what the edit alters, serialized as Arrow-IPC
//! (LZ4_FRAME) — machine-exact, never rendered through the lossy display grammar (D10).
//!
//! Atomicity (§3): validate everything first (anchors, ranges, every cell parseable under
//! the resolved type, dictionary membership under a declared schema), then apply. A
//! validation failure writes nothing — `validationError`, revision unchanged, no
//! `data_changed` (the orchestrator sweeps any partial file).
//!
//! No code copies (the §0 discipline): locale parsing, inference, naming, dictionary
//! building, field decoration, footer reading, the §1.2 grammar, numeric-level counting,
//! null spellings, and the wire-schema JSON shape are IMPORTED from
//! `csv2arrow`/`arrowview` — never re-implemented here. Review guard: this module must
//! not contain a `parse::<f64>` on a user string or a dictionary-build loop that bypasses
//! those helpers — drift there is exactly the "two inference paths" bug class the design
//! forbids.
//!
//! Status: d3–d7 — `insert_block`, the row ops, the column ops, `schema_change` and
//! `apply_inverse` (every restore program) are served end to end through ONE row-merge
//! rewrite engine (segments + a sequential batch cursor); the round-trip crown holds
//! both identities bit-exact for every family. A declared `insert_block` `target_schema`
//! is served per P13 (a window-scoped positional array of nullable entries); the lane
//! advertises `data_edit` since d8 and the real-lane e2e crown (edit → undo ×2 → redo,
//! revisions 0→5) proves the whole rail.

use crate::arrowview;
use crate::csv2arrow::{self, CatDict, Level, Locale};
use crate::messages;

use arrow::array::{
    Array, ArrayRef, AsArray, DictionaryArray, Float64Builder, Int32Builder, new_null_array,
};
use arrow::datatypes::{Field, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use arrow_ipc::reader::FileReader;
use indexmap::IndexSet;
use serde_json::json;
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

/// Output batch size for null-filled ranges (mirrors the conversion's reader batch — a
/// tuning constant, not a semantic).
const OUT_BATCH: u64 = 65_536;

/// The v1 inverse-blob format tag (D10) — the only field the frontend may observe.
const INVERSE_FORMAT_V1: &str = "arrow_ipc_v1";

// ─── The job and its outcomes ─────────────────────────────────────────────────

/// One `data_edit` work unit as dispatched: `source` = the pre-edit cache to READ,
/// `cache_path` = the new revision's file to WRITE (orchestrator-assigned), `revision` =
/// the work revision (the D11-checked current dataset revision at dispatch — the state
/// this edit is computed against, and the inverse blob's `base_revision`).
pub struct EditJob {
    pub source: String,
    pub cache_path: String,
    pub ingest: messages::IngestParams,
    pub revision: u64,
    pub edit: messages::EditOp,
    /// The frame's binary tail: §1.2 TSV cells for `insert_block`, Arrow-IPC bytes for
    /// `apply_inverse`, empty otherwise.
    pub tail: Vec<u8>,
}

/// The lane's answer to a completed edit (D6: rows/schema/invalidation are the
/// view-consistency material the orchestrator moves into `data_changed`; the inverse is
/// the undo material the frontend stores verbatim).
#[derive(Debug)]
pub struct EditOutput {
    pub rows: u64,
    /// The FULL canonical post-edit wire schema — present IFF the schema changed (an
    /// `insert_block` always recomputes value counts, so it always ships).
    pub schema: Option<serde_json::Value>,
    pub invalidation: messages::Invalidation,
    pub inverse_meta: Option<messages::InverseMeta>,
    /// The inverse's Arrow-IPC payload — rides the result frame's binary tail.
    pub inverse_bytes: Vec<u8>,
}

/// Why an edit did not land. `Validation` is the §3 all-or-nothing refusal (structured
/// detail, nothing applied, no file written); `Fatal` is infrastructure (missing/corrupt
/// cache, an unsupported cache shape, an unimplementable request).
pub enum EditFailure {
    Validation(Vec<messages::ValidationIssue>),
    Fatal(String),
}

impl EditFailure {
    fn fatal(msg: impl Into<String>) -> Self {
        EditFailure::Fatal(msg.into())
    }
}

impl std::fmt::Debug for EditFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EditFailure::Validation(issues) => write!(f, "Validation({issues:?})"),
            EditFailure::Fatal(m) => write!(f, "Fatal({m:?})"),
        }
    }
}

fn issue(code: &str, message: String) -> messages::ValidationIssue {
    messages::ValidationIssue {
        column: None,
        code: code.to_string(),
        message,
        count: None,
        rows: None,
    }
}

// ─── The forward block: §1.2 parse-back ───────────────────────────────────────

/// A parsed forward block: a rectangular grid of display cells `(text, is_null)` — the
/// §1.2 grammar's parse-back of what the frontend authored (clipboard text / a cell
/// editor string). TYPING happens in resolution, under the resolved column types — the
/// parse here recovers display strings only (the frontend never parses values; the lane
/// is the only converter, in both directions).
#[allow(dead_code)] // `cells` readers: resolution (d3 live) + the d5–d7 ops
#[derive(Debug)]
pub struct ParsedBlock {
    pub cells: Vec<Vec<(String, bool)>>,
}

/// Parse the frame tail of an `insert_block`. Shape rules (P2: block-shape problems are
/// `anchor`): the tail must be non-empty, every row LF-terminated (including the last),
/// and rectangular. An empty string IS a value (`"\n"` = a 1×1 block holding an
/// empty-string cell); an empty ROW (`"\n\n"` splitting to two 1-cell rows) is fine too —
/// only differing cell counts are ragged.
pub fn parse_block(tail: &[u8]) -> Result<ParsedBlock, EditFailure> {
    if tail.is_empty() {
        return Err(EditFailure::Validation(vec![issue(
            "anchor",
            "the edit block is empty — nothing to insert".to_string(),
        )]));
    }
    if tail.last() != Some(&b'\n') {
        return Err(EditFailure::Validation(vec![issue(
            "anchor",
            "the edit block's last row is not LF-terminated (§1.2)".to_string(),
        )]));
    }
    let body = &tail[..tail.len() - 1];
    let mut cells: Vec<Vec<(String, bool)>> = Vec::new();
    for (r, line) in body.split(|&b| b == b'\n').enumerate() {
        let row: Vec<(String, bool)> = arrowview::split_row(line)
            .into_iter()
            .map(arrowview::unescape_cell)
            .collect();
        if let Some(first) = cells.first()
            && first.len() != row.len()
        {
            return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                column: None,
                code: "anchor".to_string(),
                message: format!(
                    "the edit block is ragged: row {} carries {} cells, row 1 carries {}",
                    r + 1,
                    row.len(),
                    first.len()
                ),
                count: None,
                rows: Some(vec![r as u64]),
            }]));
        }
        cells.push(row);
    }
    Ok(ParsedBlock { cells })
}

// ─── The pre-edit cache ───────────────────────────────────────────────────────

/// The pre-edit cache's schema essentials — a footer read (schema + per-block row
/// counts; no batch bodies decoded), O(blocks) tiny reads regardless of file size.
#[allow(dead_code)] // `block_rows` feeds the d5–d7 ops' range reads
pub struct CacheSchema {
    pub schema: Schema,
    pub rows_total: u64,
    pub block_rows: Vec<u64>,
}

pub fn read_cache_schema(source: &str) -> Result<CacheSchema, EditFailure> {
    let mut file = File::open(source).map_err(|e| {
        EditFailure::fatal(format!("cannot open the pre-edit cache '{source}': {e}"))
    })?;
    let (schema, block_rows) = arrowview::footer_info(&mut file)
        .map_err(|e| EditFailure::fatal(format!("corrupt pre-edit cache '{source}': {e}")))?;
    Ok(CacheSchema {
        schema,
        rows_total: block_rows.iter().sum(),
        block_rows,
    })
}

/// Stream the cache's record batches (optionally projected to `cols`, by INPUT index)
/// from row 0 through `to_row`, invoking `on_batch(batch, base_row)`. Sequential, the
/// `data_view` machinery — never a full materialization.
fn read_range(
    source: &str,
    cols: Option<&[usize]>,
    to_row: u64,
    mut on_batch: impl FnMut(&RecordBatch, u64),
) -> Result<(), EditFailure> {
    let cache_err = |e: String| EditFailure::fatal(format!("corrupt cache '{source}': {e}"));
    let projection = cols.map(|c| c.to_vec());
    let reader = FileReader::try_new(
        File::open(source)
            .map_err(|e| EditFailure::fatal(format!("cannot open the cache '{source}': {e}")))?,
        projection,
    )
    .map_err(|e| cache_err(format!("cannot decode: {e}")))?;
    let mut base = 0u64;
    for batch in reader {
        let batch = batch.map_err(|e| cache_err(format!("read failed at row {base}: {e}")))?;
        let bend = base + batch.num_rows() as u64;
        if base < to_row {
            on_batch(&batch, base); // this batch intersects [0, to_row)
        }
        base = bend;
        if base >= to_row {
            break;
        }
    }
    Ok(())
}

// ─── Row segments: where an op's output rows come from ─────────────────────

/// An output row range's source. `Copy` draws `len` output rows from old rows
/// `[old, old+len)` under a fixed offset — identity for `insert_block`, shifted for the
/// row ops (the d4 generalization: old segments are RANGES, not identity). `Block` is
/// `insert_block`'s overwrite window: build columns take block cells while PASS columns
/// still read the old rows underneath (a paste overwrites only its rectangle). `Null`
/// is a null-filled range — `insert_block`'s holes beyond the extent and `insert_rows`'
/// fill alike. `Restore` (d7) is `Null` with data: build columns take restore cells
/// (the inverse blob's IPC arrays) while pass columns stay null — the re-inserted rows
/// of an undone `delete_rows`. Segments tile the output rows, and for every op the
/// old-row order across segments is also stream order, so one sequential cursor serves
/// the whole walk.
#[derive(Debug, PartialEq)]
enum Seg {
    Copy { out: u64, old: u64, len: u64 },
    Block { out: u64, len: u64 },
    Null { len: u64 },
    Restore { len: u64 },
}

fn block_segments(n_old: u64, row: u64, r_len: u64) -> Vec<Seg> {
    let pre = row.min(n_old);
    let mut segs = Vec::with_capacity(4);
    if pre > 0 {
        segs.push(Seg::Copy {
            out: 0,
            old: 0,
            len: pre,
        });
    }
    if row > n_old {
        segs.push(Seg::Null {
            len: row - n_old, // the anchor named rows beyond the extent: holes
        });
    }
    segs.push(Seg::Block {
        out: row,
        len: r_len,
    });
    if row + r_len < n_old {
        segs.push(Seg::Copy {
            out: row + r_len,
            old: row + r_len,
            len: n_old - (row + r_len),
        });
    }
    segs
}

fn insert_rows_segments(n_old: u64, at: u64, count: u64) -> Vec<Seg> {
    vec![
        Seg::Copy {
            out: 0,
            old: 0,
            len: at,
        },
        Seg::Null { len: count },
        Seg::Copy {
            out: at + count,
            old: at,
            len: n_old - at,
        },
    ]
}

fn delete_rows_segments(n_old: u64, at: u64, count: u64) -> Vec<Seg> {
    vec![
        Seg::Copy {
            out: 0,
            old: 0,
            len: at,
        },
        Seg::Copy {
            out: at,
            old: at + count,
            len: n_old - at - count,
        },
    ]
}

// ─── The row-merge engine (pass B, shared by every op) ───────────────────────

/// Sequential cursor over the pre-edit cache's record batches — the rewrite's input
/// side. Batches load one at a time (never held together); `skip_to` walks past removed
/// windows (decoded + dropped — rung-2 IO: O(dataset) per edit is the contract).
struct OldRows {
    reader: FileReader<File>,
    cur: Option<RecordBatch>,
    /// Old-row index of `cur`'s first row.
    base: u64,
    /// The next old row the caller will read (monotonic across the whole walk).
    pos: u64,
    /// The footer's promised row count (integrity checks only).
    n: u64,
}

impl OldRows {
    fn open(source: &str, n: u64) -> Result<Self, EditFailure> {
        let reader = FileReader::try_new(
            File::open(source).map_err(|e| {
                EditFailure::fatal(format!("cannot reopen the cache '{source}': {e}"))
            })?,
            None,
        )
        .map_err(|e| EditFailure::fatal(format!("cannot decode '{source}': {e}")))?;
        Ok(OldRows {
            reader,
            cur: None,
            base: 0,
            pos: 0,
            n,
        })
    }

    /// Load batches until the current one covers the read position (or the stream
    /// ends — `current` decides whether that is fatal).
    fn seek(&mut self) -> Result<(), EditFailure> {
        while !self
            .cur
            .as_ref()
            .is_some_and(|b| self.pos < self.base + b.num_rows() as u64)
        {
            match self.reader.next() {
                Some(Ok(b)) => {
                    if let Some(prev) = self.cur.take() {
                        self.base += prev.num_rows() as u64;
                    }
                    self.cur = Some(b);
                }
                Some(Err(e)) => {
                    return Err(EditFailure::fatal(format!(
                        "rewrite read failed at row {}: {e}",
                        self.base
                    )));
                }
                None => return Ok(()), // stream complete
            }
        }
        Ok(())
    }

    /// The input batch covering the read position, with its base old-row index. Fatal
    /// when the stream ends early — the walk only asks for rows a segment maps.
    fn current(&mut self) -> Result<(&RecordBatch, u64), EditFailure> {
        self.seek()?;
        match &self.cur {
            Some(b) if self.pos < self.base + b.num_rows() as u64 => Ok((b, self.base)),
            _ => Err(EditFailure::fatal(format!(
                "the pre-edit cache ended before old row {} (its footer promised {})",
                self.pos, self.n
            ))),
        }
    }

    fn peek(&self) -> Option<&RecordBatch> {
        self.cur.as_ref()
    }

    fn advance(&mut self, to: u64) {
        debug_assert!(to >= self.pos, "the walk is monotonic");
        self.pos = to;
    }

    /// Jump the read position forward (rows removed between segments are never read).
    fn skip_to(&mut self, target: u64) {
        self.pos = self.pos.max(target);
    }
}

/// One output batch in the making: `len` rows drawn from the current input batch's old
/// rows `[old_off, old_off+len)` and/or the block's rows `[block, block+len)` — a piece
/// carrying BOTH is `insert_block`'s in-extent window (pass columns underneath, block
/// cells on top); a piece carrying NEITHER is a null range.
struct Win {
    len: u64,
    old_off: Option<u64>,
    block: Option<u64>,
}

/// The shared streaming rewrite: walk the segment table in output order, pulling old
/// rows through a sequential batch cursor, writing one output batch per piece. Untouched
/// columns pass through as zero-copy slices (an Arrow slice shares its buffers — the
/// dictionary with them); touched columns rebuild per row; null ranges emit typed nulls
/// with the column's own dictionary on the values slot (the writer rejects a mid-file
/// dictionary replacement). Memory O(one batch + edit + inverse); IO O(dataset).
fn stream_rewrite(
    job: &EditJob,
    out_schema: &SchemaRef,
    out_cols: &mut [OutCol],
    segs: &[Seg],
    total_out: u64,
    n_old: u64,
    locale: Locale,
) -> Result<(), EditFailure> {
    let mut writer = csv2arrow::make_feather_writer(&job.cache_path, out_schema).map_err(|e| {
        EditFailure::fatal(format!(
            "cannot create the new cache '{}': {e}",
            job.cache_path
        ))
    })?;
    let mut cursor = OldRows::open(&job.source, n_old)?;

    // Capture every Pass categorical's dictionary BEFORE any piece is emitted: the v1
    // contract puts each field's dictionary on the first batch, and a null piece can
    // come first (insert_rows at 0) — emitting it with placeholder values would change
    // the field's dictionary mid-file, which the writer rejects.
    cursor.seek()?;
    if let Some(b) = cursor.peek() {
        for oc in out_cols.iter_mut() {
            if matches!(oc.build, ColBuild::Pass)
                && oc.cat.is_none()
                && csv2arrow::level_of_field(&oc.field) != Some(Level::Scale)
            {
                let values = Arc::clone(
                    b.column(oc.old.expect("pass columns have an input index"))
                        .as_dictionary::<arrow::datatypes::Int32Type>()
                        .values(),
                );
                oc.cat = Some(csv2arrow::cat_dict_from_values(values, locale));
            }
        }
    }

    let mut emitted = 0u64;
    for seg in segs {
        match *seg {
            Seg::Copy { out: _, old, len } => {
                cursor.skip_to(old);
                let mut done = 0u64;
                while done < len {
                    let (b, base) = cursor.current()?;
                    let next = old + done;
                    let take = (len - done).min(base + b.num_rows() as u64 - next);
                    emit_piece(
                        &mut writer,
                        out_schema,
                        out_cols,
                        Some(b),
                        Win {
                            len: take,
                            old_off: Some(next - base),
                            block: None,
                        },
                        locale,
                    )?;
                    cursor.advance(next + take);
                    done += take;
                }
                emitted += len;
            }
            Seg::Block { out, len } => {
                // The overwrite window: build columns take block cells; pass columns read
                // the old rows underneath while the window is inside the old extent.
                let under = len.min(n_old.saturating_sub(out));
                let mut done = 0u64;
                while done < under {
                    let (b, base) = cursor.current()?;
                    let next = out + done;
                    let take = (under - done).min(base + b.num_rows() as u64 - next);
                    emit_piece(
                        &mut writer,
                        out_schema,
                        out_cols,
                        Some(b),
                        Win {
                            len: take,
                            old_off: Some(next - base),
                            block: Some(done),
                        },
                        locale,
                    )?;
                    cursor.advance(next + take);
                    done += take;
                }
                let mut start = under;
                while start < len {
                    // The grown part: block cells on build columns, nulls elsewhere.
                    let take = OUT_BATCH.min(len - start);
                    emit_piece(
                        &mut writer,
                        out_schema,
                        out_cols,
                        None,
                        Win {
                            len: take,
                            old_off: None,
                            block: Some(start),
                        },
                        locale,
                    )?;
                    start += take;
                }
                emitted += len;
            }
            Seg::Null { len } => {
                let mut done = 0u64;
                while done < len {
                    let take = OUT_BATCH.min(len - done);
                    emit_piece(
                        &mut writer,
                        out_schema,
                        out_cols,
                        None,
                        Win {
                            len: take,
                            old_off: None,
                            block: None,
                        },
                        locale,
                    )?;
                    done += take;
                }
                emitted += len;
            }
            Seg::Restore { len } => {
                // Re-inserted rows (d7): pass columns null (the rows are new), build
                // columns replay the inverse blob's cells — the same block mechanics.
                let mut done = 0u64;
                while done < len {
                    let take = OUT_BATCH.min(len - done);
                    emit_piece(
                        &mut writer,
                        out_schema,
                        out_cols,
                        None,
                        Win {
                            len: take,
                            old_off: None,
                            block: Some(done),
                        },
                        locale,
                    )?;
                    done += take;
                }
                emitted += len;
            }
        }
    }
    if emitted != total_out {
        return Err(EditFailure::fatal(format!(
            "the rewrite plan covered {emitted} of {total_out} output rows — a segment-table bug"
        )));
    }
    writer
        .finish()
        .map_err(|e| EditFailure::fatal(format!("finishing the new cache failed: {e}")))?;
    Ok(())
}

/// Emit one output batch: `len` rows drawn from the input batch (offset `old_off`)
/// and/or the block (row `block` onward), per the column plan. Block cells take
/// precedence over old rows on build columns (the overwrite); pass columns read the old
/// rows underneath and never see block cells at all.
fn emit_piece(
    writer: &mut arrow_ipc::writer::FileWriter<File>,
    out_schema: &SchemaRef,
    out_cols: &mut [OutCol],
    batch: Option<&RecordBatch>,
    win: Win,
    locale: Locale,
) -> Result<(), EditFailure> {
    let Win {
        len,
        old_off,
        block,
    } = win;
    let mut cols: Vec<ArrayRef> = Vec::with_capacity(out_cols.len());
    for oc in out_cols.iter_mut() {
        let old_arr = batch
            .zip(old_off)
            .and_then(|(b, _)| oc.old.map(|oi| b.column(oi)));
        match &oc.build {
            ColBuild::Pass => {
                let arr = match (batch, old_off) {
                    (Some(b), Some(off)) => {
                        let a =
                            Arc::clone(b.column(oc.old.expect("pass columns have an input index")))
                                .slice(off as usize, len as usize);
                        fold_stats(oc, &a, locale);
                        a
                    }
                    _ => null_pass_array(oc, len as usize),
                };
                cols.push(arr);
            }
            ColBuild::BuildFloat { cells } => {
                // The cast is PHYSICAL-TYPE-GUARDED: a restore plan can pair an f64 build
                // with a dictionary source column (undoing a promotion) — there the old
                // values are simply not consulted (the window supplies the cells).
                let old_f = old_arr
                    .filter(|a| matches!(a.data_type(), arrow::datatypes::DataType::Float64))
                    .map(|a| a.as_primitive::<arrow::datatypes::Float64Type>());
                let mut b = Float64Builder::with_capacity(len as usize);
                for i in 0..len {
                    let v = if let Some(bs) = block {
                        cells[(bs + i) as usize]
                    } else if let (Some(f), Some(off)) = (old_f, old_off) {
                        let ix = (off + i) as usize;
                        f.is_valid(ix).then(|| f.value(ix))
                    } else {
                        None
                    };
                    oc.scale_stats.observe(v);
                    match v {
                        Some(x) => b.append_value(x),
                        None => b.append_null(),
                    }
                }
                cols.push(Arc::new(b.finish()));
            }
            ColBuild::BuildDict {
                dict,
                keys,
                promote_from_float,
            } => {
                let old_d = (!*promote_from_float)
                    .then(|| {
                        old_arr.filter(|a| {
                            matches!(a.data_type(), arrow::datatypes::DataType::Dictionary(_, _))
                        })
                    })
                    .flatten()
                    .map(|a| a.as_dictionary::<arrow::datatypes::Int32Type>());
                let old_f = (*promote_from_float)
                    .then(|| {
                        old_arr.filter(|a| {
                            matches!(a.data_type(), arrow::datatypes::DataType::Float64)
                        })
                    })
                    .flatten()
                    .map(|a| a.as_primitive::<arrow::datatypes::Float64Type>());
                let mut b = Int32Builder::with_capacity(len as usize);
                for i in 0..len {
                    let k = if let Some(bs) = block {
                        keys[(bs + i) as usize]
                    } else if let (Some(d), Some(off)) = (old_d, old_off) {
                        let ix = (off + i) as usize;
                        d.is_valid(ix).then(|| d.keys().value(ix))
                    } else if let (Some(f), Some(off)) = (old_f, old_off) {
                        // Promotion: old value → canonical level string (P1).
                        let ix = (off + i) as usize;
                        f.is_valid(ix)
                            .then(|| {
                                let s = arrowview::render_double(f.value(ix), ".", "", 10);
                                dict.index.get(&s).copied()
                            })
                            .flatten()
                    } else {
                        None
                    };
                    if k.is_some() {
                        oc.value_count += 1;
                    }
                    match k {
                        Some(ix) => b.append_value(ix),
                        None => b.append_null(),
                    }
                }
                let arr = DictionaryArray::<arrow::datatypes::Int32Type>::try_new(
                    b.finish(),
                    Arc::clone(&dict.values),
                )
                .expect("keys reference the shared dictionary");
                cols.push(Arc::new(arr));
            }
            ColBuild::RemapKeys {
                dict,
                key_map,
                keys,
            } => {
                let old_d = old_arr.map(|a| a.as_dictionary::<arrow::datatypes::Int32Type>());
                let mut b = Int32Builder::with_capacity(len as usize);
                for i in 0..len {
                    let k = if let Some(bs) = block.filter(|_| !keys.is_empty()) {
                        // A paste window (d6b): the block's cells overwrite, exactly like
                        // BuildDict; the Copy segments below still remap underneath.
                        keys[(bs + i) as usize]
                    } else if let (Some(d), Some(off)) = (old_d, old_off) {
                        let ix = (off + i) as usize;
                        d.is_valid(ix)
                            .then(|| {
                                let old = d.keys().value(ix);
                                key_map.get(old as usize).copied().filter(|&n| n >= 0)
                            })
                            .flatten()
                    } else {
                        None
                    };
                    if k.is_some() {
                        oc.value_count += 1;
                    }
                    match k {
                        Some(ix) => b.append_value(ix),
                        None => b.append_null(),
                    }
                }
                let arr = DictionaryArray::<arrow::datatypes::Int32Type>::try_new(
                    b.finish(),
                    Arc::clone(&dict.values),
                )
                .expect("keys reference the shared dictionary");
                cols.push(Arc::new(arr));
            }
            ColBuild::FloatFromDict { old_values } => {
                let old_d = old_arr.map(|a| a.as_dictionary::<arrow::datatypes::Int32Type>());
                let vals = old_values.as_string::<i32>();
                let mut b = Float64Builder::with_capacity(len as usize);
                for i in 0..len {
                    let v = match (old_d, old_off) {
                        (Some(d), Some(off)) => {
                            let ix = (off + i) as usize;
                            d.is_valid(ix)
                                .then(|| {
                                    let s = vals.value(d.keys().value(ix) as usize);
                                    locale.parse_num(s)
                                })
                                .flatten()
                        }
                        _ => None,
                    };
                    oc.scale_stats.observe(v);
                    match v {
                        Some(x) => b.append_value(x),
                        None => b.append_null(),
                    }
                }
                cols.push(Arc::new(b.finish()));
            }
        }
    }
    let rb = RecordBatch::try_new(Arc::clone(out_schema), cols)
        .map_err(|e| EditFailure::fatal(format!("output batch assembly failed: {e}")))?;
    writer
        .write(&rb)
        .map_err(|e| EditFailure::fatal(format!("output write failed: {e}")))?;
    Ok(())
}

/// Typed nulls for a Pass column with no old rows in the piece: a dictionary column
/// carries its captured values on the values slot (the writer rejects a mid-file
/// dictionary replacement); a never-seen dictionary (a zero-row cache) degenerates to
/// empty values — still one dictionary per field, which is the invariant that matters.
fn null_pass_array(oc: &OutCol, len: usize) -> ArrayRef {
    if csv2arrow::level_of_field(&oc.field) == Some(Level::Scale) {
        return new_null_array(oc.field.data_type(), len);
    }
    let values = oc
        .cat
        .as_ref()
        .map(|d| Arc::clone(&d.values))
        .unwrap_or_else(|| Arc::new(arrow::array::StringArray::from(Vec::<String>::new())));
    let mut b = Int32Builder::with_capacity(len);
    for _ in 0..len {
        b.append_null();
    }
    Arc::new(
        DictionaryArray::<arrow::datatypes::Int32Type>::try_new(b.finish(), values)
            .expect("null keys reference any dictionary"),
    )
}

/// The §6 invalidation descriptor for an `insert_block` (normative table): a column-set,
/// type, or value-levels change wins (`all`), then row growth (`rows_from` to the end,
/// new rows included), else the covered range.
fn block_invalidation(
    n_old: usize,
    new_cols: usize,
    schema_changed: bool,
    row: u64,
    r_len: u64,
    n_rows_old: u64,
) -> messages::Invalidation {
    if new_cols > n_old || schema_changed {
        messages::Invalidation {
            all: Some(true),
            rows_from: None,
            rows_to: None,
        }
    } else if row + r_len > n_rows_old {
        messages::Invalidation {
            all: None,
            rows_from: Some(row),
            rows_to: None,
        }
    } else {
        messages::Invalidation {
            all: None,
            rows_from: Some(row),
            rows_to: Some(row + r_len),
        }
    }
}

// ─── Stats (write-pass accumulators) ──────────────────────────────────────────

/// Numeric stats over ARROW Float64 values — the Arrow-native twin of `ColStats`' capped
/// distinct tracking. Semantics match `count_distinct_numbers` (dedupe on the parsed
/// number, ±0 unified, non-finite excluded from distinct) and the conversion's cap: exact
/// up to `DISTINCT_CAP`, "at least that many" beyond.
struct ScaleStats {
    count: u64,
    only_ints: bool,
    seen: std::collections::HashSet<u64>,
    capped: bool,
}

impl ScaleStats {
    fn new() -> Self {
        ScaleStats {
            count: 0,
            only_ints: true,
            seen: std::collections::HashSet::new(),
            capped: false,
        }
    }
    fn observe(&mut self, v: Option<f64>) {
        let Some(v) = v else { return };
        self.count += 1;
        if !(v.is_finite() && v.fract() == 0.0) {
            self.only_ints = false;
        }
        if !self.capped && v.is_finite() {
            self.seen.insert(if v == 0.0 { 0 } else { v.to_bits() });
            if self.seen.len() > csv2arrow::DISTINCT_CAP {
                self.capped = true;
            }
        }
    }
    fn distinct(&self) -> u64 {
        if self.capped {
            csv2arrow::DISTINCT_CAP as u64
        } else {
            self.seen.len() as u64
        }
    }
}

// ─── The column plan ──────────────────────────────────────────────────────────

/// How one OUTPUT column is produced batch by batch.
enum ColBuild {
    /// Untouched existing column: Arc-clone through input batches; nulls in a grown tail.
    Pass,
    /// (Re)built as Float64 — absorption on a scale column, or a new inferred-scale
    /// column. `cells` = the typed block column (row-indexed).
    BuildFloat { cells: Vec<Option<f64>> },
    /// (Re)built as Dictionary — absorption on a categorical (old keys pass through), a
    /// promotion (old Float64 values re-encoded via canonical strings), or a new
    /// inferred/empty categorical.
    BuildDict {
        dict: CatDict,
        keys: Vec<Option<i32>>,
        promote_from_float: bool,
    },
    /// Rebuilt against a REPLACED dictionary (schema_change's set/reorder levels, d6):
    /// the row→VALUE mapping is preserved, so every old key translates through
    /// `key_map` to its index in the new `dict`. In-use levels always map (validated);
    /// an unmapped entry (dropped-unused, defensive) emits null.
    ///
    /// `keys` (d6b) carries a PASTE window's cells: non-empty ⇒ block cells take
    /// precedence inside a Block segment (BuildDict's overwrite rule) while Copy
    /// segments still remap the current rows underneath — the insert_block composition
    /// of "paste + declared level list". Empty (schema_change/restore_schema) keeps the
    /// underneath-remap semantics even inside a Block segment — restore_schema's
    /// whole-extent window depends on it.
    RemapKeys {
        dict: CatDict,
        key_map: Vec<i32>,
        keys: Vec<Option<i32>>,
    },
    /// Retyped categorical → scale (d6): every row's old key → old value string →
    /// parsed under the locale (in-use values validated to parse beforehand — the
    /// coercion refusal). `old_values` is the old dictionary's values array.
    FloatFromDict { old_values: ArrayRef },
}

struct OutCol {
    field: Field,
    /// Input column index; `None` = a column this edit creates.
    old: Option<usize>,
    build: ColBuild,
    // Write-pass stats.
    scale_stats: ScaleStats,
    value_count: u64,
    /// A categorical's final dictionary handle — the Build one from resolution, the Pass
    /// one captured from the first batch seen.
    cat: Option<CatDict>,
}

impl OutCol {
    fn name(&self) -> &str {
        self.field.name()
    }
    fn display_name(&self) -> String {
        self.field
            .metadata()
            .get("jasp:display_name")
            .cloned()
            .unwrap_or_else(|| self.field.name().to_string())
    }
    fn level(&self) -> Level {
        csv2arrow::level_of_field(&self.field).expect("plan fields are v1 shapes")
    }
}

// ─── insert_block ─────────────────────────────────────────────────────────

/// One entry of an `insert_block` `target_schema` (P13): window-scoped, positional,
/// NULLABLE. `null` ≡ undeclared for that column (exactly the no-schema behavior);
/// a non-null entry is a strict PER-FIELD postcondition — declared fields adhere-or-
/// error, absent fields take the auto path. `name` is REQUIRED on overflow entries
/// (declaring a new column's name is the point) and optional-but-echo-checked on
/// covered entries; `labels` and covered `display_name` are schema_change's job.
#[derive(Debug)]
struct WindowEntry {
    name: Option<String>,
    display_name: Option<String>,
    column_type: Option<Level>,
    levels: Option<Vec<String>>,
}

/// The wire's type string → Level (the one-home mapping; `Level::as_str` is its
/// inverse). Unknown strings are the CALLER's schema_mismatch.
fn level_of_str(s: &str) -> Option<Level> {
    match s {
        "scale" => Some(Level::Scale),
        "ordinal" => Some(Level::Ordinal),
        "nominal" => Some(Level::Nominal),
        _ => None,
    }
}

/// Parse + shape-validate an `insert_block` `target_schema` (P13): a full positional
/// array over the paste's columns (entry i ↔ output column col+i; length must equal
/// the paste width). Everything here is decidable from the footer alone; the
/// data-dependent checks (cap rule, level_in_use, cell adherence) run in pass A and
/// resolution, still before any write (atomicity).
fn parse_window_schema(
    v: &serde_json::Value,
    fields: &[arrow::datatypes::FieldRef],
    col: u64,
    c_len: u64,
) -> Result<Vec<Option<WindowEntry>>, EditFailure> {
    let mismatch = |m: String| {
        EditFailure::Validation(vec![messages::ValidationIssue {
            column: None,
            code: "schema_mismatch".to_string(),
            message: m,
            count: None,
            rows: None,
        }])
    };
    let n = fields.len() as u64;
    let arr = v.as_array().ok_or_else(|| {
        mismatch("target_schema must be an ARRAY of nullable column entries".into())
    })?;
    if arr.len() as u64 != c_len {
        return Err(mismatch(format!(
            "target_schema declares {} entries for a paste {} columns wide — \
             the array is positional over the paste's columns",
            arr.len(),
            c_len
        )));
    }
    let mut declared_names: Vec<String> = Vec::new();
    let mut out: Vec<Option<WindowEntry>> = Vec::with_capacity(c_len as usize);
    for (i, e) in arr.iter().enumerate() {
        if e.is_null() {
            out.push(None); // undeclared: the lane's auto path for this column
            continue;
        }
        let j = col + i as u64;
        let covered = j < n;
        let obj = e.as_object().ok_or_else(|| {
            mismatch(format!(
                "target_schema entry {i} must be an object or null (P13's nullable array)"
            ))
        })?;
        if obj.contains_key("labels") {
            return Err(mismatch(
                "target_schema 'labels' is not served on insert_block — relabeling is \
                 schema_change's job"
                    .into(),
            ));
        }
        let name = match obj.get("name") {
            None | Some(serde_json::Value::Null) => None,
            Some(s) => Some(
                s.as_str()
                    .ok_or_else(|| {
                        mismatch(format!("target_schema entry {i}: 'name' must be a string"))
                    })?
                    .to_string(),
            ),
        };
        let display_name = match obj.get("display_name") {
            None | Some(serde_json::Value::Null) => None,
            Some(s) => Some(
                s.as_str()
                    .ok_or_else(|| {
                        mismatch(format!(
                            "target_schema entry {i}: 'display_name' must be a string"
                        ))
                    })?
                    .to_string(),
            ),
        };
        let column_type = match obj.get("type") {
            None | Some(serde_json::Value::Null) => None,
            Some(s) => {
                let t = s.as_str().ok_or_else(|| {
                    mismatch(format!("target_schema entry {i}: 'type' must be a string"))
                })?;
                Some(level_of_str(t).ok_or_else(|| {
                    mismatch(format!(
                        "target_schema entry {i}: unknown type '{t}' \
                         (scale | ordinal | nominal)"
                    ))
                })?)
            }
        };
        let levels = match obj.get("levels") {
            None | Some(serde_json::Value::Null) => None,
            Some(a) => {
                let list = a.as_array().ok_or_else(|| {
                    mismatch(format!(
                        "target_schema entry {i}: 'levels' must be an array of strings"
                    ))
                })?;
                let mut out = Vec::with_capacity(list.len());
                for l in list {
                    out.push(
                        l.as_str()
                            .ok_or_else(|| {
                                mismatch(format!(
                                    "target_schema entry {i}: 'levels' entries must be strings"
                                ))
                            })?
                            .to_string(),
                    );
                }
                Some(out)
            }
        };
        if covered {
            let f = &fields[j as usize];
            if let Some(nm) = name.as_deref().filter(|nm| nm != f.name()) {
                return Err(mismatch(format!(
                    "target_schema entry {i} names column '{nm}' but position {i} \
                     is column '{}' — the array is positional over the paste (echo \
                     the current name, or use null)",
                    f.name()
                )));
            }
            if display_name.is_some() {
                return Err(mismatch(
                    "renaming a covered column is schema_change's job — insert_block \
                     entries may declare type/levels adherence only"
                        .into(),
                ));
            }
            let cur_level = csv2arrow::level_of_field(f);
            if let Some(t) = column_type.filter(|t| Some(*t) != cur_level) {
                return Err(mismatch(format!(
                    "target_schema declares type '{}' for column '{}' but it is '{}' \
                     — retyping is schema_change's job (P13 v1)",
                    t.as_str(),
                    f.name(),
                    cur_level.map(|l| l.as_str()).unwrap_or("?")
                )));
            }
            if levels.is_some() && cur_level == Some(Level::Scale) {
                return Err(mismatch(format!(
                    "target_schema declares levels for scale column '{}' — a scale \
                     column has no level list",
                    f.name()
                )));
            }
        } else {
            // An overflow entry: declaring a NEW column's name. The name must be free
            // against the live columns and the other declarations (a postcondition is
            // never silently uniquified — insert_cols' P6 differs because its names
            // are inputs, not assertions).
            if let Some(nm) = &name {
                if fields.iter().any(|f| f.name() == nm) {
                    return Err(mismatch(format!(
                        "target_schema names a new column '{}' but the dataset already \
                         has one — declared names must be free",
                        nm
                    )));
                }
                if declared_names.contains(nm) {
                    return Err(mismatch(format!(
                        "target_schema declares the name '{}' twice",
                        nm
                    )));
                }
                declared_names.push(nm.clone());
            }
            if matches!(column_type, Some(Level::Scale)) && levels.is_some() {
                return Err(mismatch("a scale column cannot declare levels (P8)".into()));
            }
        }
        if let Some(levels) = &levels {
            let mut seen = std::collections::HashSet::new();
            for l in levels {
                if !seen.insert(l.as_str()) {
                    return Err(mismatch(format!(
                        "target_schema declares the level '{}' twice",
                        l
                    )));
                }
            }
        }
        out.push(Some(WindowEntry {
            name,
            display_name,
            column_type,
            levels,
        }));
    }
    Ok(out)
}

/// Serve an `insert_block`: resolve → read old state → stream the rewrite → assemble the
/// result + inverse. Every refusal before the writer opens leaves nothing on disk.
#[allow(clippy::too_many_arguments)]
fn apply_insert_block(
    job: &EditJob,
    cache: &CacheSchema,
    row: u64,
    col: u64,
    block: &ParsedBlock,
    window: Option<&[Option<WindowEntry>]>,
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let nulls = csv2arrow::null_spellings(&job.ingest);
    let fields = cache.schema.fields();
    let n = fields.len();
    let n_rows = cache.rows_total;
    let r_len = block.cells.len() as u64;
    let c_len = block.cells[0].len() as u64;
    let entry_of = |c: usize| -> Option<&WindowEntry> {
        window.and_then(|w| w.get(c).and_then(|e| e.as_ref()))
    };

    // Anchor sanity: the extent math must not overflow (§3 `anchor`).
    let new_rows = row
        .checked_add(r_len)
        .map(|r| r.max(n_rows))
        .filter(|&r| r <= u64::MAX / 4)
        .ok_or_else(|| {
            EditFailure::Validation(vec![issue(
                "anchor",
                format!("anchor row {row} + {r_len} rows overflows the extent"),
            )])
        })?;
    let col_end = col.checked_add(c_len).ok_or_else(|| {
        EditFailure::Validation(vec![issue(
            "anchor",
            format!("anchor col {col} + {c_len} cols overflows the extent"),
        )])
    })?;
    let new_cols = (col_end.max(n as u64)) as usize;

    // ── Classification: per block column, what does the target column need? ──
    // (Decidable without reading old data: promotion = a scale column receiving a cell
    // that does not parse under the ingest locale.)
    let mut promote: Vec<bool> = vec![false; c_len as usize]; // per block column
    let mut parse_ok: Vec<bool> = vec![true; c_len as usize];
    for c in 0..c_len as usize {
        let j = col as usize + c;
        if j < n && csv2arrow::level_of_field(&fields[j]) == Some(Level::Scale) {
            let ok = block.cells.iter().all(|r| {
                cell_text(&r[c], &nulls)
                    .map(|t| locale.parse_num(t).is_some())
                    .unwrap_or(true)
            });
            parse_ok[c] = ok;
            promote[c] = !ok;
            // P13: a declared scale postcondition turns unparseable cells into a
            // refusal — no automatic promotion under a declaration (adhere totally).
            if !ok && entry_of(c).is_some_and(|e| e.column_type == Some(Level::Scale)) {
                let bad = block
                    .cells
                    .iter()
                    .find_map(|r| {
                        cell_text(&r[c], &nulls).filter(|t| locale.parse_num(t).is_none())
                    })
                    .unwrap_or("");
                return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                    column: Some(fields[j].name().to_string()),
                    code: "schema_mismatch".to_string(),
                    message: format!(
                        "declared scale column '{}' cannot take the cell '{bad}' \
                         (omit the declaration to promote it)",
                        fields[j].name()
                    ),
                    count: None,
                    rows: None,
                }]));
            }
        }
    }
    let any_promote = promote.iter().any(|&p| p);

    // Touched EXISTING input columns (block range ∩ [0, n)).
    let touched: Vec<usize> = (col as usize..col_end as usize)
        .filter(|j| *j < n)
        .collect();

    // P13's covered Remap entries: a declared levels list REPLACES a covered column's
    // dictionary (d6's machinery). Every such column re-encodes (I4 widened from
    // "promotes" to "re-encodes": promotion OR declared Remap) — the inverse captures
    // it full-width, and pass A must scan it whole (minus the window) for usage.
    let remap_at: HashMap<usize, Vec<String>> = (0..c_len as usize)
        .filter_map(|c| {
            let j = col as usize + c;
            (j < n)
                .then_some(j)
                .zip(entry_of(c).and_then(|e| e.levels.clone()))
        })
        .collect();
    let any_remap = !remap_at.is_empty();

    // ── Pass A: read old state (dictionaries, promotion distinct sets, the inverse's
    // captured old cells). Projection = touched columns only. ──
    let full_capture = any_promote || any_remap; // a re-encode is a whole-column change (I4, P13)
    let rect_rows = row
        .saturating_sub(n_rows)
        .min(r_len)
        .min(n_rows.saturating_sub(row).min(r_len));
    // (rows of the rectangle that intersect the old extent:)
    let rect_len = n_rows.saturating_sub(row).min(r_len);
    let _ = rect_rows;
    let to_row = if any_promote || any_remap {
        n_rows
    } else {
        (row + r_len).min(n_rows)
    };

    let mut old_dicts: HashMap<usize, CatDict> = HashMap::new(); // input idx -> dict
    let mut pre_edit_values: HashMap<usize, ArrayRef> = HashMap::new(); // input idx -> the file's values Arc
    let mut promote_distinct: HashMap<usize, IndexSet<String>> = HashMap::new();
    let mut promote_block_inserted: HashMap<usize, bool> = HashMap::new();
    for (c, &p) in promote.iter().enumerate() {
        if p {
            let j = col as usize + c;
            promote_distinct.insert(j, IndexSet::new());
            promote_block_inserted.insert(j, false);
        }
    }
    let mut remap_usage: HashMap<usize, Vec<u64>> =
        remap_at.keys().map(|&j| (j, Vec::new())).collect();
    let mut captured: HashMap<usize, Vec<ArrayRef>> = HashMap::new(); // input idx -> slices
    let mut first_batch = true;

    if n_rows > 0 {
        read_range(
            &job.source,
            if touched.is_empty() {
                None
            } else {
                Some(&touched)
            },
            to_row,
            |batch, base| {
                let bend = base + batch.num_rows() as u64;
                for (pi, &oi) in touched.iter().enumerate() {
                    let arr = batch.column(pi);
                    // Dictionaries ride the first batch (our writer emits each dictionary
                    // once — the v1 cache contract).
                    if first_batch && csv2arrow::level_of_field(&fields[oi]) != Some(Level::Scale) {
                        let values =
                            Arc::clone(arr.as_dictionary::<arrow::datatypes::Int32Type>().values());
                        // Stash the raw values Arc too: a capture with zero rect rows
                        // still carries the PRE-EDIT dictionary on its values slot (a
                        // 0-row slice of the column), so undo restores the dictionary
                        // exactly — not the absorption-grown one.
                        pre_edit_values.insert(oi, Arc::clone(&values));
                        old_dicts.insert(oi, csv2arrow::cat_dict_from_values(values, locale));
                    }
                    // Promotion distinct: post-edit first-appearance order — old[0..row),
                    // the block's strings, old[row+R..N) — so skip the window.
                    if let Some(set) = promote_distinct.get_mut(&oi) {
                        let fa = arr.as_primitive::<arrow::datatypes::Float64Type>();
                        for i in 0..batch.num_rows() as u64 {
                            let r = base + i;
                            if r >= row && r < row + r_len {
                                continue; // overwritten by the block
                            }
                            if !promote_block_inserted[&oi] && r >= row + r_len {
                                for cell in block.cells.iter().map(|r| r[pi].clone()) {
                                    if let Some(t) = cell_text(&cell, &nulls) {
                                        set.insert(t.to_string());
                                    }
                                }
                                promote_block_inserted.insert(oi, true);
                            }
                            if fa.is_valid(i as usize) {
                                set.insert(arrowview::render_double(
                                    fa.value(i as usize),
                                    ".",
                                    "",
                                    10,
                                ));
                            }
                        }
                    }
                    // Covered-Remap usage (P13): which levels stay IN USE after the
                    // paste — the old rows OUTSIDE the window (the window itself is
                    // overwritten, so its levels may legitimately drop).
                    if remap_at.contains_key(&oi) {
                        let d = arr.as_dictionary::<arrow::datatypes::Int32Type>();
                        // Lazy sizing: the dictionary length is only known once the
                        // first batch rides in (the v1 contract).
                        let counts = remap_usage.entry(oi).or_default();
                        if counts.is_empty() {
                            counts.resize(d.values().len(), 0);
                        }
                        for i in 0..batch.num_rows() as u64 {
                            let r = base + i;
                            if r >= row && r < row + r_len {
                                continue; // overwritten by the paste
                            }
                            if d.is_valid(i as usize) {
                                counts[d.keys().value(i as usize) as usize] += 1;
                            }
                        }
                    }
                    // Capture: the rectangle for rect mode; the whole column for full mode.
                    let (cs, ce) = if full_capture {
                        (0u64, batch.num_rows() as u64)
                    } else {
                        (
                            row.saturating_sub(base),
                            (row + rect_len).min(bend).saturating_sub(base),
                        )
                    };
                    if ce > cs {
                        captured
                            .entry(oi)
                            .or_default()
                            .push(arr.slice(cs as usize, (ce - cs) as usize));
                    }
                }
                first_batch = false;
            },
        )?;
        // Block strings still pending (the window sat at the end of the old extent).
        for (&oi, set) in promote_distinct.iter_mut() {
            if !promote_block_inserted[&oi] {
                let pi = touched.iter().position(|&t| t == oi).expect("projected");
                for cell in block.cells.iter().map(|r| r[pi].clone()) {
                    if let Some(t) = cell_text(&cell, &nulls) {
                        set.insert(t.to_string());
                    }
                }
            }
        }
    }

    // ── Covered-Remap validation (P13; d6's rules verbatim, against the POST-edit
    // in-use set): the cap rule — you may only rewrite the full label list of a column
    // whose labels you could have seen in full — and level_in_use. ──
    for (&j, levels) in &remap_at {
        let Some(old_dict) = old_dicts.get(&j) else {
            continue; // a zero-row cache holds nothing in use
        };
        let old_distinct = old_dict.index.len() as u64;
        if old_distinct > csv2arrow::WIRE_LEVELS_CAP as u64 {
            return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                column: Some(fields[j].name().to_string()),
                code: "schema_mismatch".to_string(),
                message: format!(
                    "distinct_count {old_distinct} exceeds the wire cap {} — the \
                     full label list of column '{}' cannot be rewritten",
                    csv2arrow::WIRE_LEVELS_CAP,
                    fields[j].name()
                ),
                count: None,
                rows: None,
            }]));
        }
        let counts = remap_usage.get(&j).cloned().unwrap_or_default();
        let vals = old_dict.values.as_string::<i32>();
        let mut missing: Vec<String> = Vec::new();
        for (idx, cnt) in counts.iter().enumerate() {
            if *cnt > 0 {
                let v = vals.value(idx).to_string();
                if !levels.contains(&v) {
                    missing.push(v);
                }
            }
        }
        if !missing.is_empty() {
            missing.truncate(10);
            return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                column: Some(fields[j].name().to_string()),
                code: "level_in_use".to_string(),
                message: format!(
                    "level(s) in use cannot be deleted from '{}': {}",
                    fields[j].name(),
                    missing.join(", ")
                ),
                count: None,
                rows: None,
            }]));
        }
    }

    // ── Resolution: build the output column plan + typed block cells. ──
    // Schema-change detection for the §6 descriptor: a promotion (type change) or an
    // absorption that appended levels (a value-levels change) invalidates everything.
    let mut levels_grew = false;
    let mut out_cols: Vec<OutCol> = Vec::with_capacity(new_cols);
    let mut used_names: Vec<String> = Vec::with_capacity(new_cols);
    // Derived dictionary from a pasted column of cells (D5's nominal path — stats,
    // first-appearance set, canonicalization when the locale is active and the cells
    // are all numeric, prebuild_dict's value-sort within sort_limit). Shared by pure
    // inference and a declared categorical WITHOUT levels — one derivation, one home.
    let derive_dict =
        |cells: &[(String, bool)]| -> (csv2arrow::ColStats, CatDict, Vec<Option<i32>>) {
            let mut stats = csv2arrow::ColStats::new();
            let cap = job.ingest.threshold.max(csv2arrow::DISTINCT_CAP) + 1;
            for c in cells {
                if let Some(t) = cell_text(c, &nulls) {
                    stats.observe(t, cap, locale);
                }
            }
            let canonicalize = locale.active && (stats.only_ints || stats.only_doubles);
            let key_of = |t: &str| -> String {
                if canonicalize {
                    locale.canonicalize(t).unwrap_or_else(|| t.to_string())
                } else {
                    t.to_string()
                }
            };
            let mut set: IndexSet<String> = IndexSet::new();
            for c in cells {
                if let Some(t) = cell_text(c, &nulls) {
                    set.insert(key_of(t));
                }
            }
            let dict = csv2arrow::prebuild_dict(set, canonicalize, locale, job.ingest.sort_limit);
            let keys = cells
                .iter()
                .map(|c| cell_text(c, &nulls).and_then(|t| dict.index.get(&key_of(t)).copied()))
                .collect();
            (stats, dict, keys)
        };
    // P13 adherence: every pasted cell must exist in a declared list (verbatim — never
    // canonicalized; the declaration IS the post-dictionary). Shared by covered Remaps
    // and declared new columns.
    let declared_keys =
        |cells: &[(String, bool)], dict: &CatDict| -> Result<Vec<Option<i32>>, EditFailure> {
            let mut keys = Vec::with_capacity(cells.len());
            for c in cells {
                let t = cell_text(c, &nulls);
                let k = t.and_then(|t| dict.index.get(t).copied());
                if let (Some(t), None) = (t, k) {
                    return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                        column: None,
                        code: "schema_mismatch".to_string(),
                        message: format!(
                            "the pasted cell '{t}' is not in the declared levels — \
                         a declared list is the post-edit dictionary (adhere-or-error)"
                        ),
                        count: None,
                        rows: None,
                    }]));
                }
                keys.push(k);
            }
            Ok(keys)
        };
    for j in 0..new_cols {
        let in_block = j >= col as usize && j < col_end as usize;
        let bc = if in_block {
            Some(j - col as usize)
        } else {
            None
        };
        let field: Field = if j < n {
            fields[j].as_ref().clone()
        } else {
            // A column this edit creates. P13: a DECLARED name (which must be free —
            // a postcondition is never silently uniquified; insert_cols' P6 differs
            // because its names are inputs, not assertions) or the importer convention
            // (§3); the display rides the placeholder for the branches below. The
            // placeholder type is nominal — resolution decorates the REAL field via
            // `jasp_field`; only name and display matter from here.
            let entry = bc.and_then(&entry_of);
            let (name, display) = match entry.and_then(|e| e.name.clone()) {
                Some(nm) => {
                    if used_names.iter().any(|u| u == &nm) {
                        return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                            column: None,
                            code: "schema_mismatch".to_string(),
                            message: format!(
                                "target_schema names a new column '{nm}' but the name \
                                 is already taken — declared names must be free"
                            ),
                            count: None,
                            rows: None,
                        }]));
                    }
                    let disp = entry
                        .and_then(|e| e.display_name.clone())
                        .unwrap_or_else(|| nm.clone());
                    (nm, disp)
                }
                None => {
                    let nm = csv2arrow::unique_new_name(&used_names, "", j + 1);
                    (nm.clone(), nm)
                }
            };
            used_names.push(name.clone());
            csv2arrow::jasp_field(&name, &display, Level::Nominal, false)
        };
        if j < n {
            used_names.push(field.name().to_string());
        }

        let (build, field) = if j < n && !in_block {
            (ColBuild::Pass, field) // untouched existing
        } else if let Some(bc) = bc {
            let cells = block
                .cells
                .iter()
                .map(|r| r[bc].clone())
                .collect::<Vec<_>>();
            if j < n {
                // Existing column: absorption or promotion (§4) — or a declared Remap
                // (P13): the entry's levels list REPLACES the dictionary (d6's class,
                // validated above; identity — name, display, level — is unchanged).
                if let Some(levels) = remap_at.get(&j) {
                    let old_dict = old_dicts.remove(&j).unwrap_or_else(empty_cat_dict);
                    let dict = csv2arrow::dict_from_list(levels);
                    let key_map: Vec<i32> = (0..old_dict.values.len())
                        .map(|i| {
                            let v = old_dict.values.as_string::<i32>().value(i);
                            dict.index.get(v).copied().unwrap_or(-1)
                        })
                        .collect();
                    let keys = declared_keys(&cells, &dict)?;
                    levels_grew = true; // a value-levels change → all:true (I6)
                    (
                        ColBuild::RemapKeys {
                            dict,
                            key_map,
                            keys,
                        },
                        field,
                    )
                } else {
                    // Existing column: absorption or promotion (§4).
                    let old_level = csv2arrow::level_of_field(&field).expect("guarded");
                    match old_level {
                        Level::Scale if !promote[bc] => {
                            // Absorption keeps the type; all_integer narrows honestly.
                            let old_all_int = field
                                .metadata()
                                .get("jasp:all_integer")
                                .is_some_and(|v| v == "true");
                            let typed: Vec<Option<f64>> = cells
                                .iter()
                                .map(|c| cell_text(c, &nulls).and_then(|t| locale.parse_num(t)))
                                .collect();
                            let all_int = old_all_int
                                && typed
                                    .iter()
                                    .all(|v| v.is_none_or(|v| v.is_finite() && v.fract() == 0.0));
                            let f = csv2arrow::jasp_field(
                                field.name(),
                                &display_of(&field),
                                Level::Scale,
                                all_int,
                            );
                            (ColBuild::BuildFloat { cells: typed }, f)
                        }
                        Level::Scale => {
                            // Promotion: the merged column re-inferred csv2arrow-style —
                            // old f64s become canonical level strings, the dictionary
                            // value-sorts when within sort_limit.
                            let set = promote_distinct.remove(&j).expect("scanned");
                            let dict =
                                csv2arrow::prebuild_dict(set, false, locale, job.ingest.sort_limit);
                            let keys: Vec<Option<i32>> = cells
                                .iter()
                                .map(|c| {
                                    cell_text(c, &nulls).and_then(|t| dict.index.get(t).copied())
                                })
                                .collect();
                            let f = csv2arrow::jasp_field(
                                field.name(),
                                &display_of(&field),
                                Level::Nominal,
                                false,
                            );
                            (
                                ColBuild::BuildDict {
                                    dict,
                                    keys,
                                    promote_from_float: true,
                                },
                                f,
                            )
                        }
                        _ => {
                            // Categorical absorption: any value fits; new levels append in
                            // first-appearance order (§4 rule 1), old keys stay valid. Two
                            // passes — collect the unseen (canonicalized) values, append them
                            // once, then every key is a plain lookup.
                            let old_dict = old_dicts.remove(&j).unwrap_or_else(empty_cat_dict);
                            let old_canon = old_dict.canonicalize;
                            let canon = |t: &str| -> String {
                                if old_canon {
                                    locale.canonicalize(t).unwrap_or_else(|| t.to_string())
                                } else {
                                    t.to_string()
                                }
                            };
                            let mut fresh: Vec<String> = Vec::new();
                            for c in &cells {
                                if let Some(t) = cell_text(c, &nulls) {
                                    let key = canon(t);
                                    if !old_dict.index.contains_key(&key) && !fresh.contains(&key) {
                                        fresh.push(key);
                                    }
                                }
                            }
                            let dict = if fresh.is_empty() {
                                old_dict
                            } else {
                                levels_grew = true;
                                csv2arrow::append_levels(&old_dict, fresh.into_iter())
                            };
                            let keys: Vec<Option<i32>> = cells
                                .iter()
                                .map(|c| {
                                    cell_text(c, &nulls).and_then(|t| {
                                        let key = canon(t);
                                        dict.index.get(&key).copied()
                                    })
                                })
                                .collect();
                            (
                                ColBuild::BuildDict {
                                    dict,
                                    keys,
                                    promote_from_float: false,
                                },
                                field,
                            )
                        }
                    }
                }
            } else {
                // New column (j >= n, inside the paste window): DECLARED per P13 or
                // inferred per D5 (the csv2arrow path) from the block cells.
                let entry = entry_of(bc);
                match entry.and_then(|e| e.column_type) {
                    Some(Level::Scale) => {
                        // Adherence: every non-null cell must parse (a declared scale
                        // postcondition never promotes).
                        let typed: Vec<Option<f64>> = cells
                            .iter()
                            .map(|c| cell_text(c, &nulls).and_then(|t| locale.parse_num(t)))
                            .collect();
                        if let Some(bad) = cells.iter().zip(typed.iter()).find_map(|(c, v)| {
                            cell_text(c, &nulls)
                                .filter(|_| v.is_none())
                                .map(|t| t.to_string())
                        }) {
                            return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                                column: Some(field.name().to_string()),
                                code: "schema_mismatch".to_string(),
                                message: format!(
                                    "declared scale column '{}' cannot take the cell \
                                     '{bad}' — omit the type to let the lane infer",
                                    field.name()
                                ),
                                count: None,
                                rows: None,
                            }]));
                        }
                        let all_int = typed.iter().any(|v| v.is_some())
                            && typed
                                .iter()
                                .all(|v| v.is_none_or(|v| v.is_finite() && v.fract() == 0.0));
                        let f = csv2arrow::jasp_field(
                            field.name(),
                            &display_of(&field),
                            Level::Scale,
                            all_int,
                        );
                        (ColBuild::BuildFloat { cells: typed }, f)
                    }
                    Some(cat @ (Level::Nominal | Level::Ordinal)) => {
                        match entry.and_then(|e| e.levels.as_ref()) {
                            Some(levels) => {
                                // Declared levels: VERBATIM (P8 — spec order IS the level
                                // order; never sorted, never canonicalized).
                                let dict = csv2arrow::dict_from_list(levels);
                                let keys = declared_keys(&cells, &dict)?;
                                let f = csv2arrow::jasp_field(
                                    field.name(),
                                    &display_of(&field),
                                    cat,
                                    false,
                                );
                                (
                                    ColBuild::BuildDict {
                                        dict,
                                        keys,
                                        promote_from_float: false,
                                    },
                                    f,
                                )
                            }
                            None => {
                                // Declared categorical, levels derived from the cells (the
                                // shared derivation — only the level is pinned).
                                let (_stats, dict, keys) = derive_dict(&cells);
                                let f = csv2arrow::jasp_field(
                                    field.name(),
                                    &display_of(&field),
                                    cat,
                                    false,
                                );
                                (
                                    ColBuild::BuildDict {
                                        dict,
                                        keys,
                                        promote_from_float: false,
                                    },
                                    f,
                                )
                            }
                        }
                    }
                    None => {
                        // D5 inference over the block cells.
                        let (stats, dict, keys) = derive_dict(&cells);
                        let level = csv2arrow::decide(&stats, job.ingest.threshold);
                        match level {
                            Level::Scale => {
                                let typed: Vec<Option<f64>> = cells
                                    .iter()
                                    .map(|c| cell_text(c, &nulls).and_then(|t| locale.parse_num(t)))
                                    .collect();
                                let all_int = stats.only_ints && stats.count > 0;
                                let f = csv2arrow::jasp_field(
                                    field.name(),
                                    &display_of(&field),
                                    Level::Scale,
                                    all_int,
                                );
                                (ColBuild::BuildFloat { cells: typed }, f)
                            }
                            _ => {
                                let f = csv2arrow::jasp_field(
                                    field.name(),
                                    &display_of(&field),
                                    level,
                                    false,
                                );
                                (
                                    ColBuild::BuildDict {
                                        dict,
                                        keys,
                                        promote_from_float: false,
                                    },
                                    f,
                                )
                            }
                        }
                    }
                }
            }
        } else {
            // j >= n && !in_block: an empty hole column (anchor beyond the column
            // extent) — nominal, all null (inference over zero observed cells).
            let f = csv2arrow::jasp_field(field.name(), field.name(), Level::Nominal, false);
            let dict = empty_cat_dict();
            (
                ColBuild::BuildDict {
                    dict,
                    keys: Vec::new(),
                    promote_from_float: false,
                },
                f,
            )
        };

        out_cols.push(OutCol {
            field,
            old: (j < n).then_some(j),
            build,
            scale_stats: ScaleStats::new(),
            value_count: 0,
            cat: None,
        });
    }

    // The plan's schema (all decoration decided upfront — dictionaries, promotions,
    // inferred levels, and scale all_integer are all block-derivable).
    let out_schema: SchemaRef = Arc::new(Schema::new(
        out_cols
            .iter()
            .map(|c| c.field.clone())
            .collect::<Vec<Field>>(),
    ));

    // ── The inverse: the OLD cells of exactly what changed (D10) ──
    let mut inverse_bytes = Vec::new();
    let inverse_meta =
        if touched.is_empty() {
            None // pure growth: undo is a trim, and there are no old cells to restore
        } else {
            let inv_schema = Arc::new(Schema::new(
                touched
                    .iter()
                    .map(|&oi| fields[oi].clone())
                    .collect::<Vec<_>>(),
            ));
            let mut arrays: Vec<ArrayRef> = Vec::with_capacity(touched.len());
            for &oi in &touched {
                let mut slices = captured.remove(&oi).unwrap_or_default();
                if slices.is_empty() {
                    // The rectangle missed the old extent entirely (anchor beyond rows).
                    // A CATEGORICAL capture still carries the pre-edit dictionary on its
                    // values slot — a plain typed-null array would orphan the old levels
                    // and undo would leave the grown dictionary behind.
                    let arr = match pre_edit_values.get(&oi) {
                        Some(values) => {
                            let mut b = Int32Builder::with_capacity(0);
                            Arc::new(
                                DictionaryArray::<arrow::datatypes::Int32Type>::try_new(
                                    b.finish(),
                                    Arc::clone(values),
                                )
                                .expect("null keys reference the column's dictionary"),
                            ) as ArrayRef
                        }
                        None => new_null_array(fields[oi].data_type(), 0),
                    };
                    slices.push(arr);
                }
                let refs: Vec<&dyn Array> = slices.iter().map(|a| a.as_ref()).collect();
                arrays.push(arrow::compute::concat(&refs).map_err(|e| {
                    EditFailure::fatal(format!("inverse capture concat failed: {e}"))
                })?);
            }
            let batch = RecordBatch::try_new(inv_schema.clone(), arrays)
                .map_err(|e| EditFailure::fatal(format!("inverse capture batch failed: {e}")))?;
            let mut w = csv2arrow::make_ipc_writer(&mut inverse_bytes, &inv_schema)
                .map_err(|e| EditFailure::fatal(format!("inverse IPC writer failed: {e}")))?;
            w.write(&batch)
                .map_err(|e| EditFailure::fatal(format!("inverse IPC write failed: {e}")))?;
            w.finish()
                .map_err(|e| EditFailure::fatal(format!("inverse IPC finish failed: {e}")))?;

            // P5: an inverse that cannot fit one frame is a visible failure, never a truncation.
            if inverse_bytes.len() > messages::MAX_INLINE_PAYLOAD - 64 * 1024 {
                return Err(EditFailure::fatal(format!(
                    "the inverse is {} bytes — too large to transport (ceiling {}); \
                 the change itself is too large to undo over the wire",
                    inverse_bytes.len(),
                    messages::MAX_INLINE_PAYLOAD - 64 * 1024
                )));
            }

            Some(messages::InverseMeta {
                format: INVERSE_FORMAT_V1.to_string(),
                base_revision: job.revision,
                ops: json!({
                    "v": 1,
                    "op": "restore_block",
                    "anchor": { "row": row, "col": col, "rows": r_len, "cols": c_len },
                    "old_rows": n_rows,
                    "old_cols": n,
                    "capture": if full_capture { "full" } else { "rect" },
                    "capture_rows": if full_capture { n_rows } else { rect_len },
                    "columns": touched.iter().map(|&oi| json!({
                        "old_index": oi,
                        "name": fields[oi].name(),
                        "type": csv2arrow::level_of_field(&fields[oi]).map(|l| l.as_str()),
                    })).collect::<Vec<_>>(),
                    "trim_rows_from": (new_rows > n_rows).then_some(n_rows),
                    "trim_cols_from": (new_cols > n).then_some(n),
                }),
            })
        };

    // ── Pass B: the streaming rewrite — the shared row-merge engine (segments + the
    // sequential batch cursor; untouched columns pass through as zero-copy slices). ──
    let segs = block_segments(n_rows, row, r_len);
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        new_rows,
        n_rows,
        locale,
    )?;

    // ── The wire schema (always for insert_block — value counts changed) ──
    let schema_json = wire_schema(&mut out_cols, locale);

    let schema_changed = levels_grew
        || out_cols.iter().any(|oc| {
            oc.old
                .zip(Some(&oc.field))
                .is_some_and(|(oi, nf)| fields[oi].data_type() != nf.data_type())
        });

    Ok(EditOutput {
        rows: new_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: block_invalidation(n, new_cols, schema_changed, row, r_len, n_rows),
        inverse_meta,
        inverse_bytes,
    })
}

/// A parsed cell's text: `None` when the cell is the null marker OR a null spelling
/// (conversion parity — the same spellings that nulled at open null again here; the
/// empty string is a spelling, so an empty pasted cell reads as null, exactly like a
/// blank CSV cell did).
fn cell_text<'a>(cell: &'a (String, bool), nulls: &[String]) -> Option<&'a str> {
    if cell.1 || nulls.iter().any(|n| n == &cell.0) {
        None
    } else {
        Some(cell.0.as_str())
    }
}

fn display_of(f: &Field) -> String {
    f.metadata()
        .get("jasp:display_name")
        .cloned()
        .unwrap_or_else(|| f.name().to_string())
}

fn empty_cat_dict() -> CatDict {
    CatDict {
        values: Arc::new(arrow::array::StringArray::from(Vec::<String>::new())),
        index: HashMap::new(),
        canonicalize: false,
    }
}

/// Fold a Pass column's stats from one input batch (the full-schema recompute rides the
/// rewrite for free).
fn fold_stats(oc: &mut OutCol, arr: &ArrayRef, locale: Locale) {
    match csv2arrow::level_of_field(&oc.field) {
        Some(Level::Scale) => {
            let f = arr.as_primitive::<arrow::datatypes::Float64Type>();
            for i in 0..f.len() {
                oc.scale_stats.observe(f.is_valid(i).then(|| f.value(i)));
            }
        }
        _ => {
            let d = arr.as_dictionary::<arrow::datatypes::Int32Type>();
            if oc.cat.is_none() {
                oc.cat = Some(csv2arrow::cat_dict_from_values(
                    Arc::clone(d.values()),
                    locale,
                ));
            }
            for i in 0..d.len() {
                if d.is_valid(i) {
                    oc.value_count += 1;
                }
            }
        }
    }
}

/// Adopt the build dictionaries as their columns' stats handles, then render the full
/// wire schema — always through the one-home `column_info_json` (never re-shaped here).
/// A Pass categorical that never saw a batch (a zero-row cache) reports an empty
/// dictionary honestly rather than panicking: the column exists, it just holds nothing.
fn wire_schema(out_cols: &mut [OutCol], locale: Locale) -> Vec<serde_json::Value> {
    for oc in out_cols.iter_mut() {
        match &oc.build {
            ColBuild::BuildDict { dict, .. } | ColBuild::RemapKeys { dict, .. }
                if oc.cat.is_none() =>
            {
                oc.cat = Some(CatDict {
                    values: Arc::clone(&dict.values),
                    index: dict.index.clone(),
                    canonicalize: dict.canonicalize,
                });
            }
            _ => {}
        }
    }
    let empty = empty_cat_dict();
    out_cols
        .iter()
        .map(|oc| {
            let info = match oc.level() {
                Level::Scale => csv2arrow::ColumnInfo {
                    name: oc.name().to_string(),
                    display_name: oc.display_name(),
                    level: "scale",
                    all_integer: oc.scale_stats.only_ints,
                    levels: None,
                    value_count: oc.scale_stats.count,
                    distinct_count: oc.scale_stats.distinct(),
                    numeric_levels: None,
                    labels: None,
                },
                _ => {
                    let cd = oc.cat.as_ref().unwrap_or(&empty);
                    let levels: Vec<String> = cd
                        .values
                        .as_string::<i32>()
                        .iter()
                        .flatten()
                        .take(csv2arrow::WIRE_LEVELS_CAP)
                        .map(|s| s.to_string())
                        .collect();
                    csv2arrow::ColumnInfo {
                        name: oc.name().to_string(),
                        display_name: oc.display_name(),
                        level: oc.level().as_str(),
                        all_integer: false,
                        levels: Some(levels),
                        value_count: oc.value_count,
                        distinct_count: cd.index.len() as u64,
                        numeric_levels: Some(csv2arrow::numeric_levels_of(cd, locale)),
                        labels: csv2arrow::labels_of_field(&oc.field),
                    }
                }
            };
            csv2arrow::column_info_json(&info)
        })
        .collect()
}

// ─── insert_rows / delete_rows ───────────────────────────────────────────────

/// One pass-through output column: field cloned verbatim, stats recomputed on the walk.
fn pass_col(field: Field, old: usize) -> OutCol {
    OutCol {
        field,
        old: Some(old),
        build: ColBuild::Pass,
        scale_stats: ScaleStats::new(),
        value_count: 0,
        cat: None,
    }
}

/// The all-Pass column plan shared by the row ops: no column is touched, so every field
/// passes through unchanged and the stats recompute on the walk.
fn pass_through_plan(cache: &CacheSchema) -> (SchemaRef, Vec<OutCol>) {
    let out_cols: Vec<OutCol> = cache
        .schema
        .fields()
        .iter()
        .enumerate()
        .map(|(j, f)| pass_col(f.as_ref().clone(), j))
        .collect();
    let out_schema: SchemaRef = Arc::new(Schema::new(
        out_cols
            .iter()
            .map(|c| c.field.clone())
            .collect::<Vec<Field>>(),
    ));
    (out_schema, out_cols)
}

/// Serve `insert_rows {at, count}` (§3): shift down + null fill. No column is touched —
/// pure pass-through — and nulls move nothing in the schema (every count is per-value,
/// not per-row), so no schema rides `data_changed` (§4: it ships when it moves). The
/// inverse is metadata-only: undo deletes exactly the inserted window (§5's honest
/// bound — a structural no-data change has no old cells to restore).
fn apply_insert_rows(
    job: &EditJob,
    cache: &CacheSchema,
    at: u64,
    count: u64,
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let n_rows = cache.rows_total;
    if count == 0 || at > n_rows {
        return Err(EditFailure::Validation(vec![issue(
            "range",
            format!(
                "insert_rows at {at} count {count}: 'at' must name a row inside the \
                 {n_rows}-row extent and 'count' be positive"
            ),
        )]));
    }
    let new_rows = n_rows
        .checked_add(count)
        .filter(|&r| r <= u64::MAX / 4)
        .ok_or_else(|| {
            EditFailure::Validation(vec![issue(
                "range",
                format!("insert_rows at {at} count {count} overflows the extent"),
            )])
        })?;

    let (out_schema, mut out_cols) = pass_through_plan(cache);
    let segs = insert_rows_segments(n_rows, at, count);
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        new_rows,
        n_rows,
        locale,
    )?;

    Ok(EditOutput {
        rows: new_rows,
        schema: None, // nulls change no count — nothing in the schema moved
        invalidation: messages::Invalidation {
            all: None,
            rows_from: Some(at), // §6: everything below the anchor shifts
            rows_to: None,
        },
        inverse_meta: Some(messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: job.revision,
            ops: json!({
                "v": 1,
                "op": "delete_rows",
                "at": at,
                "count": count,
                "old_rows": n_rows,
            }),
        }),
        inverse_bytes: Vec::new(),
    })
}

/// Serve `delete_rows {at, count}` (§3): shift up, `at+count ≤ rows` else `range`.
/// Removed rows remove values, so `value_count` drops and the schema ships (the
/// dictionary itself never prunes — unused levels stay, exactly like an overwritten
/// value's level does). The inverse carries the removed rows' cells — every column,
/// machine-exact (D10) — because undo is genuinely multi-step (§5): re-insert the
/// window, then restore the cells. Captured BEFORE any write and P5-checked, so a
/// refusal leaves nothing on disk.
fn apply_delete_rows(
    job: &EditJob,
    cache: &CacheSchema,
    at: u64,
    count: u64,
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let n_rows = cache.rows_total;
    let end = at
        .checked_add(count)
        .filter(|&e| e <= n_rows)
        .ok_or_else(|| {
            EditFailure::Validation(vec![issue(
                "range",
                format!(
                    "delete_rows at {at} count {count}: the window exceeds the {n_rows}-row extent"
                ),
            )])
        })?;
    if count == 0 {
        return Err(EditFailure::Validation(vec![issue(
            "range",
            format!("delete_rows at {at} count 0: nothing to delete"),
        )]));
    }

    // ── The inverse first: the removed rows, every column (a full-width capture — I4's
    // uniform-batch reasoning applies: the window spans all columns by definition).
    let fields = cache.schema.fields();
    let ncols = fields.len();
    let mut captured: Vec<Vec<ArrayRef>> = vec![Vec::new(); ncols];
    read_range(&job.source, None, end, |batch, base| {
        let bend = base + batch.num_rows() as u64;
        let from = at.max(base);
        let to = end.min(bend);
        if to > from {
            let (off, len) = ((from - base) as usize, (to - from) as usize);
            for (j, slices) in captured.iter_mut().enumerate() {
                slices.push(batch.column(j).slice(off, len));
            }
        }
    })?;
    let mut inverse_bytes = Vec::new();
    {
        let inv_schema: SchemaRef =
            Arc::new(Schema::new(fields.iter().cloned().collect::<Vec<_>>()));
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(ncols);
        for slices in captured.into_iter() {
            let refs: Vec<&dyn Array> = slices.iter().map(|a| a.as_ref()).collect();
            arrays.push(
                arrow::compute::concat(&refs).map_err(|e| {
                    EditFailure::fatal(format!("inverse capture concat failed: {e}"))
                })?,
            );
        }
        let batch = RecordBatch::try_new(inv_schema.clone(), arrays)
            .map_err(|e| EditFailure::fatal(format!("inverse capture batch failed: {e}")))?;
        let mut w = csv2arrow::make_ipc_writer(&mut inverse_bytes, &inv_schema)
            .map_err(|e| EditFailure::fatal(format!("inverse IPC writer failed: {e}")))?;
        w.write(&batch)
            .map_err(|e| EditFailure::fatal(format!("inverse IPC write failed: {e}")))?;
        w.finish()
            .map_err(|e| EditFailure::fatal(format!("inverse IPC finish failed: {e}")))?;
    }
    // P5: an inverse that cannot fit one frame is a visible failure, never a truncation.
    if inverse_bytes.len() > messages::MAX_INLINE_PAYLOAD - 64 * 1024 {
        return Err(EditFailure::fatal(format!(
            "the inverse is {} bytes — too large to transport (ceiling {}); \
             the change itself is too large to undo over the wire",
            inverse_bytes.len(),
            messages::MAX_INLINE_PAYLOAD - 64 * 1024
        )));
    }
    let inverse_meta = Some(messages::InverseMeta {
        format: INVERSE_FORMAT_V1.to_string(),
        base_revision: job.revision,
        ops: json!({
            "v": 1,
            "op": "restore_rows",
            "at": at,
            "count": count,
            "old_rows": n_rows,
            "columns": (0..ncols)
                .map(|j| json!({
                    "old_index": j,
                    "name": fields[j].name(),
                    "type": csv2arrow::level_of_field(&fields[j]).map(|l| l.as_str()),
                }))
                .collect::<Vec<_>>(),
        }),
    });

    let (out_schema, mut out_cols) = pass_through_plan(cache);
    let segs = delete_rows_segments(n_rows, at, count);
    let new_rows = n_rows - count;
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        new_rows,
        n_rows,
        locale,
    )?;

    // value_count dropped (removed rows remove values) → the schema ships; the
    // dictionary never prunes, so levels/distinct stay.
    let schema_json = wire_schema(&mut out_cols, locale);
    Ok(EditOutput {
        rows: new_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: messages::Invalidation {
            all: None,
            rows_from: Some(at), // §6: everything below the anchor shifts
            rows_to: None,
        },
        inverse_meta,
        inverse_bytes,
    })
}

// ─── insert_cols / delete_cols ───────────────────────────────────────────────

/// The schema the plan writes — from the planned fields, so writer and wire schema can
/// never disagree.
fn plan_schema(out_cols: &[OutCol]) -> SchemaRef {
    Arc::new(Schema::new(
        out_cols
            .iter()
            .map(|c| c.field.clone())
            .collect::<Vec<Field>>(),
    ))
}

/// The declared type of a `NewColumnSpec` (D5/P8). Absent/null means infer — over ZERO
/// observed cells, since an `insert_cols` carries none — which the csv2arrow decision
/// resolves to nominal (a fresh column has nothing to be ordinal or scale about). An
/// UNKNOWN type string is a malformed spec: adhere-or-error, never a silent default.
fn spec_level(spec: &messages::NewColumnSpec) -> Result<Level, EditFailure> {
    let spec_issue = |msg: String| {
        EditFailure::Validation(vec![messages::ValidationIssue {
            column: Some(spec.name.clone()),
            code: "schema_mismatch".to_string(),
            message: msg,
            count: None,
            rows: None,
        }])
    };
    match spec.column_type.as_deref() {
        None => Ok(Level::Nominal),
        Some("scale") => Ok(Level::Scale),
        Some("ordinal") => Ok(Level::Ordinal),
        Some("nominal") => Ok(Level::Nominal),
        Some(other) => Err(spec_issue(format!(
            "unknown column type '{other}' (scale | ordinal | nominal)"
        ))),
    }
}

/// Serve `insert_cols {at, columns}` (§3): shift columns right at `at`; the specs become
/// null-filled columns — no cells ride this op, so `cells`/`keys` stay empty and the
/// walk emits nulls for them (the engine never even knows they are new). Rows never
/// move: the segment table is ONE identity copy. The inverse is metadata-only — undo
/// deletes exactly the inserted window (nothing was destroyed; old columns only
/// shifted). Invalidation `{all:true}` (§6 column-set change); the schema always ships.
fn apply_insert_cols(
    job: &EditJob,
    cache: &CacheSchema,
    at: u64,
    columns: &[messages::NewColumnSpec],
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let fields = cache.schema.fields();
    let n = fields.len();
    let n_rows = cache.rows_total;
    let k = columns.len() as u64;

    if columns.is_empty() || at > n as u64 {
        return Err(EditFailure::Validation(vec![issue(
            "range",
            format!(
                "insert_cols at {at} with {} columns: 'at' must name a position inside \
                 the {n}-column extent and the spec list be non-empty",
                columns.len()
            ),
        )]));
    }
    n.checked_add(columns.len()).ok_or_else(|| {
        EditFailure::Validation(vec![issue(
            "range",
            format!("insert_cols at {at} would overflow the {n}-column extent"),
        )])
    })?;

    // Per-spec validation (P8), all BEFORE any planning: unknown types, `levels` on a
    // scale spec (a contradiction), duplicate entries in a declared list.
    for spec in columns {
        let level = spec_level(spec)?;
        if let Some(levels) = spec.levels.as_deref()
            && level == Level::Scale
            && !levels.is_empty()
        {
            return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                column: Some(spec.name.clone()),
                code: "schema_mismatch".to_string(),
                message: "a scale column has no label list".to_string(),
                count: None,
                rows: None,
            }]));
        }
        if let Some(levels) = spec.levels.as_deref() {
            let mut seen = IndexSet::new();
            for l in levels {
                if !seen.insert(l) {
                    return Err(EditFailure::Validation(vec![messages::ValidationIssue {
                        column: Some(spec.name.clone()),
                        code: "schema_mismatch".to_string(),
                        message: format!("duplicate level '{l}' in the declared list"),
                        count: None,
                        rows: None,
                    }]));
                }
            }
        }
    }

    // The plan: survivors pass through around the specs. Names uniquify against the
    // FULL post-edit list as each lands (P6) — an earlier insert may already own
    // `score_3`, so the generator loops until genuinely free.
    let mut used_names: Vec<String> = fields.iter().map(|f| f.name().to_string()).collect();
    let mut out_cols: Vec<OutCol> = Vec::with_capacity(n + columns.len());
    for (j, f) in fields.iter().enumerate().take(at as usize) {
        out_cols.push(pass_col(f.as_ref().clone(), j));
    }
    for (i, spec) in columns.iter().enumerate() {
        let position = at as usize + i + 1; // the 1-based output position
        let name = csv2arrow::unique_new_name(&used_names, &spec.name, position);
        let display = spec.display_name.clone().unwrap_or_else(|| name.clone());
        let level = spec_level(spec).expect("validated above");
        let (field, build) = match level {
            Level::Scale => (
                csv2arrow::jasp_field(&name, &display, Level::Scale, false),
                ColBuild::BuildFloat { cells: Vec::new() },
            ),
            lv => {
                // Declared levels pre-populate the dictionary VERBATIM (P8): spec order
                // IS the level order (an ordinal's meaning); [] ≡ absent → empty dict.
                let dict = match spec.levels.as_deref() {
                    Some(levels) if !levels.is_empty() => csv2arrow::dict_from_list(levels),
                    _ => empty_cat_dict(),
                };
                (
                    csv2arrow::jasp_field(&name, &display, lv, false),
                    ColBuild::BuildDict {
                        dict,
                        keys: Vec::new(),
                        promote_from_float: false,
                    },
                )
            }
        };
        used_names.push(name);
        out_cols.push(OutCol {
            field,
            old: None,
            build,
            scale_stats: ScaleStats::new(),
            value_count: 0,
            cat: None,
        });
    }
    for (j, f) in fields.iter().enumerate().skip(at as usize) {
        out_cols.push(pass_col(f.as_ref().clone(), j));
    }

    let out_schema = plan_schema(&out_cols);
    let segs = vec![Seg::Copy {
        out: 0,
        old: 0,
        len: n_rows,
    }];
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        n_rows,
        n_rows,
        locale,
    )?;

    let schema_json = wire_schema(&mut out_cols, locale);
    Ok(EditOutput {
        rows: n_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: messages::Invalidation {
            all: Some(true), // §6: a column-set change invalidates everything
            rows_from: None,
            rows_to: None,
        },
        inverse_meta: Some(messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: job.revision,
            ops: json!({
                "v": 1,
                "op": "delete_cols",
                "at": at,
                "count": k,
                "old_cols": n,
            }),
        }),
        inverse_bytes: Vec::new(),
    })
}

/// Serve `delete_cols {at, count}` (§3): shift left over `[at, at+count)`, the window
/// inside the extent and at least one column kept (P7 — a zero-column dataset is
/// degenerate: no schema, no grid, nothing legacy or the frontend can render; deleting
/// every column in one action is a mistake atomic refusal serves better). The inverse
/// carries the removed columns FULL-WIDTH (a column's whole contents were destroyed —
/// §5's honest bound; dictionaries included, so restore is type-faithful), captured
/// before any write and P5-checked. Rows never move; `{all:true}`; schema ships.
fn apply_delete_cols(
    job: &EditJob,
    cache: &CacheSchema,
    at: u64,
    count: u64,
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let fields = cache.schema.fields();
    let n = fields.len();
    let n_rows = cache.rows_total;
    let end = at
        .checked_add(count)
        .filter(|&e| e <= n as u64)
        .ok_or_else(|| {
            EditFailure::Validation(vec![issue(
                "range",
                format!(
                    "delete_cols at {at} count {count}: the window exceeds the {n}-column extent"
                ),
            )])
        })?;
    if count == 0 {
        return Err(EditFailure::Validation(vec![issue(
            "range",
            format!("delete_cols at {at} count 0: nothing to delete"),
        )]));
    }
    if (n as u64) - count < 1 {
        return Err(EditFailure::Validation(vec![issue(
            "range",
            format!("delete_cols at {at} count {count}: a dataset must keep at least one column"),
        )]));
    }

    // ── The inverse first: the removed columns, full width — a projected read of just
    // those columns (the same economy as pass A), every batch sliced whole.
    let removed: Vec<usize> = (at as usize..end as usize).collect();
    let mut captured: Vec<Vec<ArrayRef>> = vec![Vec::new(); removed.len()];
    read_range(&job.source, Some(&removed), n_rows, |batch, _base| {
        for (pi, slices) in captured.iter_mut().enumerate() {
            slices.push(Arc::clone(batch.column(pi)));
        }
    })?;
    let mut inverse_bytes = Vec::new();
    {
        let inv_schema: SchemaRef = Arc::new(Schema::new(
            removed
                .iter()
                .map(|&j| fields[j].clone())
                .collect::<Vec<_>>(),
        ));
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(removed.len());
        for (slices, j) in captured.into_iter().zip(removed.iter()) {
            let slices = if slices.is_empty() {
                // a zero-row cache read nothing — an empty typed column keeps the
                // capture batch shaped
                vec![new_null_array(fields[*j].data_type(), 0)]
            } else {
                slices
            };
            let refs: Vec<&dyn Array> = slices.iter().map(|a| a.as_ref()).collect();
            arrays.push(
                arrow::compute::concat(&refs).map_err(|e| {
                    EditFailure::fatal(format!("inverse capture concat failed: {e}"))
                })?,
            );
        }
        let batch = RecordBatch::try_new(inv_schema.clone(), arrays)
            .map_err(|e| EditFailure::fatal(format!("inverse capture batch failed: {e}")))?;
        let mut w = csv2arrow::make_ipc_writer(&mut inverse_bytes, &inv_schema)
            .map_err(|e| EditFailure::fatal(format!("inverse IPC writer failed: {e}")))?;
        w.write(&batch)
            .map_err(|e| EditFailure::fatal(format!("inverse IPC write failed: {e}")))?;
        w.finish()
            .map_err(|e| EditFailure::fatal(format!("inverse IPC finish failed: {e}")))?;
    }
    // P5: an inverse that cannot fit one frame is a visible failure, never a truncation.
    if inverse_bytes.len() > messages::MAX_INLINE_PAYLOAD - 64 * 1024 {
        return Err(EditFailure::fatal(format!(
            "the inverse is {} bytes — too large to transport (ceiling {}); \
             the change itself is too large to undo over the wire",
            inverse_bytes.len(),
            messages::MAX_INLINE_PAYLOAD - 64 * 1024
        )));
    }
    let inverse_meta = Some(messages::InverseMeta {
        format: INVERSE_FORMAT_V1.to_string(),
        base_revision: job.revision,
        ops: json!({
            "v": 1,
            "op": "restore_cols",
            "at": at,
            "count": count,
            "old_cols": n,
            "columns": removed
                .iter()
                .map(|&j| json!({
                    "old_index": j,
                    "name": fields[j].name(),
                    "type": csv2arrow::level_of_field(&fields[j]).map(|l| l.as_str()),
                }))
                .collect::<Vec<_>>(),
        }),
    });

    // The plan: survivors pass through; rows never move (one identity copy).
    let keep = |j: usize| pass_col(fields[j].as_ref().clone(), j);
    let mut out_cols: Vec<OutCol> = (0..at as usize).map(keep).collect::<Vec<_>>();
    out_cols.extend((end as usize..n).map(keep));

    let out_schema = plan_schema(&out_cols);
    let segs = vec![Seg::Copy {
        out: 0,
        old: 0,
        len: n_rows,
    }];
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        n_rows,
        n_rows,
        locale,
    )?;

    let schema_json = wire_schema(&mut out_cols, locale);
    Ok(EditOutput {
        rows: n_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: messages::Invalidation {
            all: Some(true), // §6: a column-set change invalidates everything
            rows_from: None,
            rows_to: None,
        },
        inverse_meta,
        inverse_bytes,
    })
}

// ─── schema_change (d6) ──────────────────────────────────────────────────────

/// One declared entry of a `schema_change` target_schema. `name` is the IDENTITY (the
/// column's CURRENT field name — renames declare the new name via `display_name`, from
/// which the canonical field name derives per P4/P6); the array order is the NEW column
/// order (reorder). Optional fields are changes: absent = keep. `labels: {}` (an empty
/// map) DETACHES the overlay; absent keeps it.
#[derive(Debug)]
struct ChangeEntry {
    name: String,
    display_name: Option<String>,
    column_type: Option<String>,
    levels: Option<Vec<String>>,
    labels: Option<serde_json::Map<String, serde_json::Value>>,
}

fn parse_entry(v: &serde_json::Value) -> Result<ChangeEntry, EditFailure> {
    let bad = |m: String| {
        EditFailure::Validation(vec![messages::ValidationIssue {
            column: None,
            code: "schema_mismatch".to_string(),
            message: m,
            count: None,
            rows: None,
        }])
    };
    let Some(obj) = v.as_object() else {
        return Err(bad("each target_schema entry must be an object".into()));
    };
    let Some(name) = obj.get("name").and_then(|n| n.as_str()) else {
        return Err(bad(
            "each target_schema entry needs a string 'name' (the column's current \
             name — its identity)"
                .into(),
        ));
    };
    let str_field = |k: &str| -> Result<Option<String>, EditFailure> {
        match obj.get(k) {
            None | Some(serde_json::Value::Null) => Ok(None),
            Some(s) => s
                .as_str()
                .map(|s| Some(s.to_string()))
                .ok_or_else(|| bad(format!("target_schema '{name}': '{k}' must be a string"))),
        }
    };
    let levels = match obj.get("levels") {
        None | Some(serde_json::Value::Null) => None,
        Some(a) => {
            let arr = a.as_array().ok_or_else(|| {
                bad(format!(
                    "target_schema '{name}': 'levels' must be an array of strings"
                ))
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for l in arr {
                out.push(
                    l.as_str()
                        .ok_or_else(|| {
                            bad(format!(
                                "target_schema '{name}': 'levels' entries must be strings"
                            ))
                        })?
                        .to_string(),
                );
            }
            Some(out)
        }
    };
    let labels = match obj.get("labels") {
        None | Some(serde_json::Value::Null) => None,
        Some(m) => {
            let map = m.as_object().cloned().ok_or_else(|| {
                bad(format!(
                    "target_schema '{name}': 'labels' must be an object value→display-label"
                ))
            })?;
            for (v, l) in &map {
                if !l.is_string() {
                    return Err(bad(format!(
                        "target_schema '{name}': labels['{v}'] must be a string"
                    )));
                }
            }
            Some(map)
        }
    };
    Ok(ChangeEntry {
        name: name.to_string(),
        display_name: str_field("display_name")?,
        column_type: str_field("type")?,
        levels,
        labels,
    })
}

/// What one column's change does to its data.
enum Rebuild {
    /// Metadata only (rename, flag flip, relabel) — pure pass-through.
    Keep,
    /// The dictionary is REPLACED by a declared list; keys remap (the row→value mapping
    /// is preserved). Inverse: JSON-only (the old list).
    Remap { new_levels: Vec<String> },
    /// Categorical → scale. Inverse: full-column capture (re-encode class).
    ToScale,
    /// Scale → categorical. Inverse: full-column capture (re-encode class).
    ToCategorical { declared: Option<Vec<String>> },
}

struct Change {
    /// Input column index.
    j: usize,
    entry_idx: usize,
    rename_to: Option<String>,
    new_level: Level,
    rebuild: Rebuild,
    labels: Option<serde_json::Map<String, serde_json::Value>>,
}

/// Serve `schema_change {target_schema}` (§3): METADATA ONLY — rename, retype,
/// set/reorder levels, reorder columns, relabel. Never changes the column set (the
/// declared array must match the current columns one-to-one, matched by CURRENT name)
/// or the row count (one identity segment). Inverses follow P9's classes: metadata and
/// key-remap changes are JSON-only; physical re-encodes (scale↔categorical) capture the
/// old column full-width. Labels ride P11's two-mappings model — a sparse `jasp:labels`
/// overlay, so a relabel is O(k) and never touches data.
#[allow(clippy::too_many_lines)]
fn apply_schema_change(
    job: &EditJob,
    cache: &CacheSchema,
    target: &serde_json::Value,
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let fields = cache.schema.fields();
    let n = fields.len();
    let n_rows = cache.rows_total;
    let mismatch = |m: String| {
        EditFailure::Validation(vec![messages::ValidationIssue {
            column: None,
            code: "schema_mismatch".to_string(),
            message: m,
            count: None,
            rows: None,
        }])
    };
    let col_issue = |col: &str, code: &str, msg: String| {
        EditFailure::Validation(vec![messages::ValidationIssue {
            column: Some(col.to_string()),
            code: code.to_string(),
            message: msg,
            count: None,
            rows: None,
        }])
    };

    let arr = target.as_array().ok_or_else(|| {
        mismatch("schema_change: target_schema must be an ARRAY of column entries".into())
    })?;
    let entries: Vec<ChangeEntry> = arr.iter().map(parse_entry).collect::<Result<_, _>>()?;
    if entries.len() != n {
        return Err(mismatch(format!(
            "schema_change never changes the column set: {} entries for {n} columns \
             (structural changes go through insert_cols/delete_cols)",
            entries.len()
        )));
    }
    // Match by CURRENT name, each column exactly once; duplicates are ambiguous.
    let mut match_of: Vec<Option<usize>> = vec![None; entries.len()];
    for (ei, e) in entries.iter().enumerate() {
        let hits: Vec<usize> = (0..n)
            .filter(|&j| fields[j].name() == e.name.as_str())
            .collect();
        match hits.as_slice() {
            [j] => match_of[ei] = Some(*j),
            [] => {
                return Err(mismatch(format!(
                    "target_schema names column '{}' — the dataset has no such column",
                    e.name
                )));
            }
            _ => {
                return Err(mismatch(format!(
                    "target_schema entry '{}' is duplicated",
                    e.name
                )));
            }
        }
    }
    {
        let mut seen = vec![false; n];
        for j in match_of.iter().flatten() {
            if std::mem::replace(&mut seen[*j], true) {
                return Err(mismatch(format!(
                    "target_schema names column '{}' twice",
                    fields[*j].name()
                )));
            }
        }
    }

    // ── Classify each column's change; validate what needs no data. ──
    let mut used_names: Vec<String> = fields.iter().map(|f| f.name().to_string()).collect();
    let mut changes: Vec<Change> = Vec::with_capacity(n);
    for (ei, e) in entries.iter().enumerate() {
        let j = match_of[ei].expect("matched above");
        let old_level = csv2arrow::level_of_field(&fields[j]).expect("v1 shape guard");
        let new_level = match e.column_type.as_deref() {
            None => old_level,
            Some("scale") => Level::Scale,
            Some("ordinal") => Level::Ordinal,
            Some("nominal") => Level::Nominal,
            Some(other) => {
                return Err(col_issue(
                    &e.name,
                    "schema_mismatch",
                    format!("unknown type '{other}' (scale | ordinal | nominal)"),
                ));
            }
        };
        // scale has neither a label list nor an overlay — declaring either contradicts
        // (P8's rule carried to schema_change).
        if new_level == Level::Scale && (e.levels.is_some() || e.labels.is_some()) {
            return Err(col_issue(
                &e.name,
                "schema_mismatch",
                "a scale column has no levels and no labels".into(),
            ));
        }
        let rebuild = match (old_level, new_level) {
            (Level::Scale, Level::Scale) => Rebuild::Keep,
            (Level::Scale, _) => Rebuild::ToCategorical {
                declared: e.levels.clone(),
            },
            (_, Level::Scale) => Rebuild::ToScale,
            // categorical → categorical: flag flip, and/or a replaced level list.
            (_, _) => match &e.levels {
                Some(new_levels) => {
                    // Duplicates in a declared list are malformed (P8).
                    let mut seen_levels = IndexSet::new();
                    for l in new_levels {
                        if !seen_levels.insert(l) {
                            return Err(col_issue(
                                &e.name,
                                "schema_mismatch",
                                format!("duplicate level '{l}' in the declared list"),
                            ));
                        }
                    }
                    Rebuild::Remap {
                        new_levels: new_levels.clone(),
                    }
                }
                None => Rebuild::Keep, // flag flip / rename / relabel only
            },
        };
        // A rename's new field name derives from the new display name (P4) and uniquifies
        // against the evolving pool (P6's loop rule).
        let rename_to = e.display_name.as_ref().map(|d| {
            let new_name = csv2arrow::unique_new_name(&used_names, d, j + 1);
            used_names[j] = new_name.clone();
            new_name
        });
        changes.push(Change {
            j,
            entry_idx: ei,
            rename_to,
            new_level,
            rebuild,
            labels: e.labels.clone(),
        });
    }

    // ── Pass A (only when data is needed): rebuild old dictionaries from the first
    // batch, count per-level usage, collect scale→cat rendered sets, and capture the
    // re-encode columns full-width for the inverse. ──
    let needs_scan: Vec<usize> = changes
        .iter()
        .filter(|c| !matches!(c.rebuild, Rebuild::Keep))
        .map(|c| c.j)
        .collect();
    let reencode: Vec<usize> = changes
        .iter()
        .filter(|c| matches!(c.rebuild, Rebuild::ToScale | Rebuild::ToCategorical { .. }))
        .map(|c| c.j)
        .collect();
    let mut old_dicts: HashMap<usize, CatDict> = HashMap::new();
    let mut usage: HashMap<usize, Vec<u64>> = HashMap::new();
    let mut rendered: HashMap<usize, IndexSet<String>> = HashMap::new();
    let mut captured: HashMap<usize, Vec<ArrayRef>> = HashMap::new();
    // Labels-only Keep columns still need their dictionary for key validation — a
    // FIRST-BATCH-ONLY read (the dictionary rides batch one; one row suffices).
    let labels_only: Vec<usize> = changes
        .iter()
        .filter(|c| matches!(c.rebuild, Rebuild::Keep) && c.labels.is_some())
        .map(|c| c.j)
        .filter(|&j| csv2arrow::level_of_field(&fields[j]) != Some(Level::Scale))
        .collect();
    if !labels_only.is_empty() && n_rows > 0 {
        read_range(&job.source, Some(&labels_only), 1, |batch, _base| {
            for (pi, &j) in labels_only.iter().enumerate() {
                let d = batch
                    .column(pi)
                    .as_dictionary::<arrow::datatypes::Int32Type>();
                old_dicts.entry(j).or_insert_with(|| {
                    csv2arrow::cat_dict_from_values(Arc::clone(d.values()), locale)
                });
            }
        })?;
    }
    if !needs_scan.is_empty() && n_rows > 0 {
        let first = |j: usize| reencode.contains(&j);
        let _ = first;
        read_range(&job.source, Some(&needs_scan), n_rows, |batch, _base| {
            for (pi, &j) in needs_scan.iter().enumerate() {
                let arr = batch.column(pi);
                if csv2arrow::level_of_field(&fields[j]) == Some(Level::Scale) {
                    let f = arr.as_primitive::<arrow::datatypes::Float64Type>();
                    let set = rendered.entry(j).or_default();
                    for i in 0..f.len() {
                        if f.is_valid(i) {
                            set.insert(arrowview::render_double(f.value(i), ".", "", 10));
                        }
                    }
                } else {
                    let d = arr.as_dictionary::<arrow::datatypes::Int32Type>();
                    // The v1 contract: the first batch carries the dictionary.
                    old_dicts.entry(j).or_insert_with(|| {
                        csv2arrow::cat_dict_from_values(Arc::clone(d.values()), locale)
                    });
                    let counts = usage.entry(j).or_insert_with(|| vec![0; d.values().len()]);
                    for i in 0..d.len() {
                        if d.is_valid(i) {
                            counts[d.keys().value(i) as usize] += 1;
                        }
                    }
                }
                if reencode.contains(&j) {
                    captured.entry(j).or_default().push(Arc::clone(arr));
                }
            }
        })?;
    }
    let empty_dict = empty_cat_dict();
    let dict_of = |j: usize| -> &CatDict { old_dicts.get(&j).unwrap_or(&empty_dict) };

    // ── Data-dependent validation (§3/§4, all before any write). ──
    for c in &changes {
        let e = &entries[c.entry_idx];
        let old_dict = dict_of(c.j);
        let old_vals = |d: &CatDict| -> Vec<String> {
            (0..d.values.len())
                .map(|i| d.values.as_string::<i32>().value(i).to_string())
                .collect()
        };
        match &c.rebuild {
            Rebuild::Keep => {}
            Rebuild::Remap { new_levels } => {
                // The cap rule (§4): you may only rewrite the full label list of a column
                // whose labels you could have seen in full.
                let old_distinct = old_dict.index.len() as u64;
                if old_distinct > csv2arrow::WIRE_LEVELS_CAP as u64 {
                    return Err(col_issue(
                        &e.name,
                        "schema_mismatch",
                        format!(
                            "distinct_count {old_distinct} exceeds the wire cap {} — the \
                             full label list cannot be rewritten",
                            csv2arrow::WIRE_LEVELS_CAP
                        ),
                    ));
                }
                // In-use levels must survive; unused may drop (§4 / level_in_use).
                let counts = usage.get(&c.j).cloned().unwrap_or_default();
                let vals = old_vals(old_dict);
                let mut missing: Vec<String> = Vec::new();
                for (idx, cnt) in counts.iter().enumerate() {
                    if *cnt > 0 && !new_levels.contains(&vals[idx]) {
                        missing.push(vals[idx].clone());
                    }
                }
                if !missing.is_empty() {
                    missing.truncate(10);
                    return Err(col_issue(
                        &e.name,
                        "level_in_use",
                        format!("level(s) in use cannot be deleted: {}", missing.join(", ")),
                    ));
                }
            }
            Rebuild::ToScale => {
                // Every IN-USE value must parse (coerce-or-error); unused levels die with
                // the dictionary (undo restores them via the capture).
                let counts = usage.get(&c.j).cloned().unwrap_or_default();
                let vals = old_vals(old_dict);
                let mut bad: Vec<String> = Vec::new();
                for (idx, cnt) in counts.iter().enumerate() {
                    if *cnt > 0 && locale.parse_num(&vals[idx]).is_none() {
                        bad.push(vals[idx].clone());
                    }
                }
                if !bad.is_empty() {
                    bad.truncate(10);
                    return Err(col_issue(
                        &e.name,
                        "coercion",
                        format!(
                            "cannot retype to scale — level(s) in use are not numbers: {}",
                            bad.join(", ")
                        ),
                    ));
                }
            }
            Rebuild::ToCategorical { declared } => {
                let set = rendered.get(&c.j).cloned().unwrap_or_default();
                if let Some(declared) = declared {
                    // The cap rule on the rendered set (same spirit — you could only
                    // declare a list you could have seen in full).
                    if set.len() > csv2arrow::WIRE_LEVELS_CAP {
                        return Err(col_issue(
                            &e.name,
                            "schema_mismatch",
                            format!(
                                "{} distinct values exceed the wire cap {} — a full \
                                 label list cannot be declared for it",
                                set.len(),
                                csv2arrow::WIRE_LEVELS_CAP
                            ),
                        ));
                    }
                    // Every rendered value must have its declared level (level_unknown).
                    let mut missing: Vec<String> = set
                        .iter()
                        .filter(|v| !declared.contains(*v))
                        .cloned()
                        .collect();
                    if !missing.is_empty() {
                        missing.truncate(10);
                        return Err(col_issue(
                            &e.name,
                            "level_unknown",
                            format!(
                                "value(s) in the data have no declared level: {}",
                                missing.join(", ")
                            ),
                        ));
                    }
                }
            }
        }
        // Labels keys must reference levels that exist post-change.
        if let Some(labels) = &c.labels {
            let valid: Vec<String> = match &c.rebuild {
                Rebuild::Remap { new_levels } => new_levels.clone(),
                Rebuild::ToCategorical { declared: Some(d) } => d.clone(),
                Rebuild::ToCategorical { declared: None } => rendered
                    .get(&c.j)
                    .cloned()
                    .unwrap_or_default()
                    .into_iter()
                    .collect(),
                _ => old_vals(old_dict),
            };
            for v in labels.keys() {
                if !valid.iter().any(|l| l == v) {
                    return Err(col_issue(
                        &e.name,
                        "level_unknown",
                        format!("labels reference '{v}' — no such level in the column"),
                    ));
                }
            }
        }
    }

    // ── The inverse (P9): full-column IPC for re-encodes; JSON-only for the rest. ──
    let mut inverse_bytes = Vec::new();
    if !reencode.is_empty() {
        let inv_schema: SchemaRef = Arc::new(Schema::new(
            reencode
                .iter()
                .map(|&j| fields[j].clone())
                .collect::<Vec<_>>(),
        ));
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(reencode.len());
        for &j in &reencode {
            let mut slices = captured.remove(&j).unwrap_or_default();
            if slices.is_empty() {
                slices.push(new_null_array(fields[j].data_type(), 0));
            }
            let refs: Vec<&dyn Array> = slices.iter().map(|a| a.as_ref()).collect();
            arrays.push(
                arrow::compute::concat(&refs).map_err(|e| {
                    EditFailure::fatal(format!("inverse capture concat failed: {e}"))
                })?,
            );
        }
        let batch = RecordBatch::try_new(inv_schema.clone(), arrays)
            .map_err(|e| EditFailure::fatal(format!("inverse capture batch failed: {e}")))?;
        let mut w = csv2arrow::make_ipc_writer(&mut inverse_bytes, &inv_schema)
            .map_err(|e| EditFailure::fatal(format!("inverse IPC writer failed: {e}")))?;
        w.write(&batch)
            .map_err(|e| EditFailure::fatal(format!("inverse IPC write failed: {e}")))?;
        w.finish()
            .map_err(|e| EditFailure::fatal(format!("inverse IPC finish failed: {e}")))?;
    }
    // P5: a visible fatal, never a truncation.
    if inverse_bytes.len() > messages::MAX_INLINE_PAYLOAD - 64 * 1024 {
        return Err(EditFailure::fatal(format!(
            "the inverse is {} bytes — too large to transport (ceiling {}); \
             the change itself is too large to undo over the wire",
            inverse_bytes.len(),
            messages::MAX_INLINE_PAYLOAD - 64 * 1024
        )));
    }
    let inv_columns_meta: Vec<serde_json::Value> = changes
        .iter()
        .filter(|c| {
            c.rename_to.is_some()
                || csv2arrow::level_of_field(&fields[c.j]) != Some(c.new_level)
                || !matches!(c.rebuild, Rebuild::Keep)
                || c.labels.is_some()
        })
        .map(|c| {
            let old_level = csv2arrow::level_of_field(&fields[c.j]).expect("v1");
            let old_dict = dict_of(c.j);
            let old_levels: Option<Vec<String>> = (!matches!(old_level, Level::Scale))
                .then(|| {
                    (0..old_dict.values.len())
                        .map(|i| old_dict.values.as_string::<i32>().value(i).to_string())
                        .collect()
                })
                .filter(|l: &Vec<String>| !l.is_empty());
            let reencoded = matches!(c.rebuild, Rebuild::ToScale | Rebuild::ToCategorical { .. });
            let post_name = c
                .rename_to
                .clone()
                .unwrap_or_else(|| fields[c.j].name().to_string());
            json!({
                "current": post_name,
                "old_name": fields[c.j].name(),
                "old_display": display_of(&fields[c.j]),
                "old_type": old_level.as_str(),
                "old_levels": old_levels,
                "old_labels": csv2arrow::labels_of_field(&fields[c.j]),
                "capture": reencoded.then_some("full"),
                "capture_rows": reencoded.then_some(n_rows),
            })
        })
        .collect();
    let inverse_meta = Some(messages::InverseMeta {
        format: INVERSE_FORMAT_V1.to_string(),
        base_revision: job.revision,
        ops: json!({
            "v": 1,
            "op": "restore_schema",
            "old_order": fields.iter().map(|f| f.name().to_string()).collect::<Vec<_>>(),
            "old_cols": n,
            "old_rows": n_rows,
            "columns": inv_columns_meta,
        }),
    });

    // ── The plan (new order = entry order) + one identity-segment walk. ──
    // `all_integer` for cat→scale retypes derives from the rendered set (known now).
    let to_scale_all_int: HashMap<usize, bool> = changes
        .iter()
        .filter(|c| matches!(c.rebuild, Rebuild::ToScale))
        .map(|c| {
            let all_int = rendered.get(&c.j).is_some_and(|set| !set.is_empty())
                && rendered
                    .get(&c.j)
                    .unwrap()
                    .iter()
                    .all(|v| locale.parse_num(v).is_some_and(|x| x.fract() == 0.0));
            (c.j, all_int)
        })
        .collect();
    let mut out_cols: Vec<OutCol> = Vec::with_capacity(n);
    for c in &changes {
        let e = &entries[c.entry_idx];
        let base = fields[c.j].as_ref().clone();
        let old_all_int = base
            .metadata()
            .get("jasp:all_integer")
            .is_some_and(|v| v == "true");
        let (name, display) = match c.rename_to.as_deref() {
            Some(new_name) => (
                new_name.to_string(),
                e.display_name
                    .clone()
                    .unwrap_or_else(|| new_name.to_string()),
            ),
            None => (base.name().to_string(), display_of(&base)),
        };
        let all_int = if matches!(c.rebuild, Rebuild::ToScale) {
            to_scale_all_int.get(&c.j).copied().unwrap_or(false)
        } else {
            old_all_int
        };
        let mut field = csv2arrow::jasp_field(&name, &display, c.new_level, all_int);
        // The overlay: declared labels replace (`{}` detaches); absent keeps the old.
        let overlay = match &c.labels {
            Some(l) => Some(serde_json::Value::Object(l.clone())),
            None if c.new_level != Level::Scale => csv2arrow::labels_of_field(&base),
            None => None,
        };
        if let Some(l) = overlay {
            csv2arrow::attach_labels(&mut field, &l);
        }
        let build = match &c.rebuild {
            Rebuild::Keep => ColBuild::Pass,
            Rebuild::Remap { new_levels } => {
                let old_dict = dict_of(c.j);
                let dict = csv2arrow::dict_from_list(new_levels);
                let key_map: Vec<i32> = (0..old_dict.values.len())
                    .map(|i| {
                        let v = old_dict.values.as_string::<i32>().value(i);
                        dict.index.get(v).copied().unwrap_or(-1)
                    })
                    .collect();
                ColBuild::RemapKeys {
                    dict,
                    key_map,
                    keys: Vec::new(),
                }
            }
            Rebuild::ToScale => ColBuild::FloatFromDict {
                old_values: Arc::clone(&dict_of(c.j).values),
            },
            Rebuild::ToCategorical { declared } => {
                let dict = match declared {
                    Some(levels) => csv2arrow::dict_from_list(levels),
                    None => {
                        // Derived csv2arrow-style from the rendered set (P1's rule).
                        let set = rendered.remove(&c.j).unwrap_or_default();
                        csv2arrow::prebuild_dict(set, false, locale, job.ingest.sort_limit)
                    }
                };
                ColBuild::BuildDict {
                    dict,
                    keys: Vec::new(),
                    promote_from_float: true,
                }
            }
        };
        out_cols.push(OutCol {
            field,
            old: Some(c.j),
            build,
            scale_stats: ScaleStats::new(),
            value_count: 0,
            cat: None,
        });
    }

    let out_schema = plan_schema(&out_cols);
    let segs = vec![Seg::Copy {
        out: 0,
        old: 0,
        len: n_rows,
    }];
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        n_rows,
        n_rows,
        locale,
    )?;

    let schema_json = wire_schema(&mut out_cols, locale);

    // §6 invalidation: type / value-levels / column-order changes → all:true; rename-only
    // or labels-only → {} (headers and overlays ride the schema; no row is stale — the
    // view renders VALUES, and neither the values nor the row order moved).
    let order_changed = (0..n).any(|k| {
        changes
            .iter()
            .position(|c| c.entry_idx == k)
            .map(|ci| changes[ci].j != k)
            .unwrap_or(true)
    });
    let type_or_levels_changed = changes.iter().any(|c| {
        csv2arrow::level_of_field(&fields[c.j]) != Some(c.new_level)
            || !matches!(c.rebuild, Rebuild::Keep)
    });
    let invalidation = if type_or_levels_changed || order_changed {
        messages::Invalidation {
            all: Some(true),
            rows_from: None,
            rows_to: None,
        }
    } else {
        messages::Invalidation {
            all: None,
            rows_from: None,
            rows_to: None,
        }
    };

    Ok(EditOutput {
        rows: n_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation,
        inverse_meta,
        inverse_bytes,
    })
}

// ─── apply_inverse (d7) ──────────────────────────────────────────────────────

/// The decoded inverse blob: one IPC frame (the captures are single-batch), each column
/// as its OLD field — metadata included (Arrow persists field metadata, so names, display
/// names, ordered flags and `jasp:labels` ride the capture verbatim) — plus the
/// materialized array. Defensive throughout: the blob is round-tripped bytes; treat it
/// as untrusted input, never as an internal invariant.
struct InverseData {
    columns: Vec<(Field, ArrayRef)>,
}

fn decode_inverse(bytes: &[u8]) -> Result<InverseData, EditFailure> {
    let reader = FileReader::try_new(std::io::Cursor::new(bytes), None)
        .map_err(|e| EditFailure::fatal(format!("the inverse blob does not decode: {e}")))?;
    let fields: Vec<Field> = reader
        .schema()
        .fields()
        .iter()
        .map(|f| f.as_ref().clone())
        .collect();
    let batch = reader
        .into_iter()
        .next()
        .transpose()
        .map_err(|e| EditFailure::fatal(format!("the inverse blob does not decode: {e}")))?
        .ok_or_else(|| EditFailure::fatal("the inverse blob carries no batch"))?;
    if fields.len() != batch.num_columns() {
        return Err(EditFailure::fatal("the inverse blob is not rectangular"));
    }
    let columns = fields
        .into_iter()
        .zip(batch.columns().iter().cloned())
        .collect::<Vec<_>>();
    if columns.is_empty() {
        return Err(EditFailure::fatal("the inverse blob has no columns"));
    }
    for (f, _) in &columns {
        if csv2arrow::level_of_field(f).is_none() {
            return Err(EditFailure::fatal(format!(
                "inverse blob column '{}' has unsupported type {}",
                f.name(),
                f.data_type()
            )));
        }
    }
    Ok(InverseData { columns })
}

impl InverseData {
    fn rows(&self) -> u64 {
        self.columns
            .first()
            .map(|(_, a)| a.len() as u64)
            .unwrap_or(0)
    }
}

/// Materialize an IPC column into the block-cell form the engine builds from.
enum Restored {
    F64(Vec<Option<f64>>),
    Dict {
        dict: CatDict,
        keys: Vec<Option<i32>>,
    },
}

fn restored_of(field: &Field, arr: &ArrayRef, locale: Locale) -> Result<Restored, EditFailure> {
    match csv2arrow::level_of_field(field) {
        Some(Level::Scale) => {
            let f = arr.as_primitive::<arrow::datatypes::Float64Type>();
            Ok(Restored::F64(
                (0..f.len())
                    .map(|i| f.is_valid(i).then(|| f.value(i)))
                    .collect(),
            ))
        }
        Some(_) => {
            let d = arr.as_dictionary::<arrow::datatypes::Int32Type>();
            let dict = csv2arrow::cat_dict_from_values(Arc::clone(d.values()), locale);
            let keys = (0..d.len())
                .map(|i| d.is_valid(i).then(|| d.keys().value(i)))
                .collect();
            Ok(Restored::Dict { dict, keys })
        }
        None => Err(EditFailure::fatal("unsupported inverse column type")),
    }
}

fn restored_build(r: Restored) -> ColBuild {
    match r {
        Restored::F64(cells) => ColBuild::BuildFloat { cells },
        Restored::Dict { dict, keys } => ColBuild::BuildDict {
            dict,
            keys,
            promote_from_float: false,
        },
    }
}

/// One `columns` entry of a `restore_schema` program, defensively parsed (the blob is
/// round-tripped JSON+IPC — untrusted input, never an internal invariant).
struct SchemaProgCol {
    /// The POST-edit name — the column undo LOCATES (it still exists under this name).
    current: String,
    /// The pre-edit field name — what the undo restores.
    old_name: String,
    old_display: String,
    old_type: Level,
    /// The pre-edit dictionary, verbatim order (None for scale, or an empty dict).
    old_levels: Option<Vec<String>>,
    old_labels: Option<serde_json::Value>,
    /// `capture:"full"` — the column re-encoded, so the IPC blob carries its truth.
    capture: bool,
}

fn parse_schema_prog_col(v: &serde_json::Value) -> Result<SchemaProgCol, EditFailure> {
    let bad = |m: String| EditFailure::fatal(format!("restore_schema program: {m}"));
    let obj = v
        .as_object()
        .ok_or_else(|| bad("each column entry must be an object".into()))?;
    let string_of = |k: &str| -> Result<String, EditFailure> {
        obj.get(k)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .ok_or_else(|| bad(format!("each column entry needs a string '{k}'")))
    };
    let current = string_of("current")?;
    let old_name = string_of("old_name")?;
    let old_display = string_of("old_display")?;
    let old_type = match string_of("old_type")?.as_str() {
        "scale" => Level::Scale,
        "ordinal" => Level::Ordinal,
        "nominal" => Level::Nominal,
        other => return Err(bad(format!("unknown old_type '{other}'"))),
    };
    let old_levels = match obj.get("old_levels") {
        None | Some(serde_json::Value::Null) => None,
        Some(serde_json::Value::Array(a)) => Some(
            a.iter()
                .map(|v| v.as_str().map(|s| s.to_string()))
                .collect::<Option<_>>()
                .ok_or_else(|| bad("'old_levels' entries must be strings".into()))?,
        ),
        Some(_) => return Err(bad("'old_levels' must be an array of strings".into())),
    };
    let old_labels = match obj.get("old_labels") {
        None | Some(serde_json::Value::Null) => None,
        Some(v @ serde_json::Value::Object(_)) => Some(v.clone()),
        Some(_) => {
            return Err(bad(
                "'old_labels' must be an object value→display-label".into()
            ));
        }
    };
    let capture = obj.get("capture").and_then(|v| v.as_str()) == Some("full");
    Ok(SchemaProgCol {
        current,
        old_name,
        old_display,
        old_type,
        old_levels,
        old_labels,
        capture,
    })
}

/// Read one u64 parameter out of a program (defensive — a blob is never trusted).
fn u64_of(ops: &serde_json::Value, key: &str) -> Result<u64, EditFailure> {
    ops.get(key)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| EditFailure::fatal(format!("inverse program: missing u64 '{key}'")))
}

/// Serve `apply_inverse {inverse}` (§5): validate defensively, then execute the restore
/// program through the same engine every op rides. The result carries its OWN inverse —
/// the redo — symmetric by construction. Undo invalidates everything (`all:true`):
/// a restore can touch arbitrary regions; v1 honesty over descriptor refinement.
///
/// Program set: `delete_rows`/`delete_cols` (metadata-only — the undo of the inserts,
/// DELEGATED to the forward executors, whose own inverses are the redo),
/// `restore_rows`/`restore_cols`/`restore_block`/`restore_schema` (IPC-backed — the
/// last for the schema_change family: identity + order restore, key remaps for the
/// metadata classes, wholesale IPC replay for the re-encodes).
fn apply_inverse(
    job: &EditJob,
    cache: &CacheSchema,
    meta: &messages::InverseMeta,
    bytes: &[u8],
) -> Result<EditOutput, EditFailure> {
    if meta.format != INVERSE_FORMAT_V1 {
        return Err(EditFailure::fatal(format!(
            "unknown inverse format '{}' (this lane speaks {})",
            meta.format, INVERSE_FORMAT_V1
        )));
    }
    if meta.base_revision > job.revision {
        return Err(EditFailure::Validation(vec![issue(
            "stale_edit",
            format!(
                "the inverse was computed at revision {} but the dataset is at {} — \
                 the stack is no longer LIFO",
                meta.base_revision, job.revision
            ),
        )]));
    }
    let Some(op) = meta.ops.get("op").and_then(|v| v.as_str()) else {
        return Err(EditFailure::fatal("inverse program: no 'op'"));
    };
    match op {
        "delete_rows" => apply_delete_rows(
            job,
            cache,
            u64_of(&meta.ops, "at")?,
            u64_of(&meta.ops, "count")?,
        ),
        "delete_cols" => apply_delete_cols(
            job,
            cache,
            u64_of(&meta.ops, "at")?,
            u64_of(&meta.ops, "count")?,
        ),
        "restore_rows" => restore_rows_exec(job, cache, &meta.ops, bytes),
        "restore_cols" => restore_cols_exec(job, cache, &meta.ops, bytes),
        "restore_block" => restore_block_exec(job, cache, &meta.ops, bytes),
        "restore_schema" => restore_schema_exec(job, cache, &meta.ops, bytes),
        other => Err(EditFailure::fatal(format!(
            "unknown inverse program '{other}'"
        ))),
    }
}

/// `restore_rows` (undo of `delete_rows`): re-insert `count` rows at `at`, every column
/// replayed from the full-width capture. The window is a `Restore` segment (new rows —
/// nulls underneath, IPC cells for every column); outside it the Build arms read the
/// current file's own values, and the dictionaries match the capture's because
/// `delete_rows` passed them through untouched (never prunes).
fn restore_rows_exec(
    job: &EditJob,
    cache: &CacheSchema,
    ops: &serde_json::Value,
    bytes: &[u8],
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let at = u64_of(ops, "at")?;
    let count = u64_of(ops, "count")?;
    let old_rows = u64_of(ops, "old_rows")?;
    let n_rows = cache.rows_total;
    let fields = cache.schema.fields();
    let n = fields.len();
    if count == 0 || at > n_rows || n_rows + count != old_rows {
        return Err(EditFailure::fatal(format!(
            "restore_rows at {at} count {count}: inconsistent with the {n_rows}-row dataset \
             (the program expects {old_rows} after restore)"
        )));
    }
    let inv = decode_inverse(bytes)?;
    if inv.columns.len() != n || inv.rows() != count {
        return Err(EditFailure::fatal(
            "restore_rows: the blob does not match the program (a full-width capture \
             was expected)",
        ));
    }

    let mut out_cols: Vec<OutCol> = Vec::with_capacity(n);
    for (j, f) in fields.iter().enumerate() {
        let (inv_field, inv_arr) = &inv.columns[j];
        let build = restored_build(restored_of(inv_field, inv_arr, locale)?);
        out_cols.push(OutCol {
            field: f.as_ref().clone(),
            old: Some(j),
            build,
            scale_stats: ScaleStats::new(),
            value_count: 0,
            cat: None,
        });
    }
    let out_schema = plan_schema(&out_cols);
    let segs = vec![
        Seg::Copy {
            out: 0,
            old: 0,
            len: at,
        },
        Seg::Restore { len: count },
        Seg::Copy {
            out: at + count,
            old: at,
            len: n_rows - at,
        },
    ];
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        old_rows,
        n_rows,
        locale,
    )?;

    let schema_json = wire_schema(&mut out_cols, locale);
    Ok(EditOutput {
        rows: old_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: messages::Invalidation {
            all: Some(true),
            rows_from: None,
            rows_to: None,
        },
        // Redo: delete exactly the re-inserted window (metadata-only).
        inverse_meta: Some(messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: job.revision,
            ops: json!({
                "v": 1,
                "op": "delete_rows",
                "at": at,
                "count": count,
                "old_rows": n_rows,
            }),
        }),
        inverse_bytes: Vec::new(),
    })
}

/// `restore_cols` (undo of `delete_cols`): re-insert `count` columns at `at` with their
/// FULL identity from the capture — the IPC fields carry names, ordered flags and
/// `jasp:labels`; the arrays carry values + dictionaries. ONE `Block` window over all
/// rows: survivors pass through underneath while restored columns replay their cells
/// (build columns take block cells; the underneath is per-column via the plan).
/// Redo: metadata-only `delete_cols`.
fn restore_cols_exec(
    job: &EditJob,
    cache: &CacheSchema,
    ops: &serde_json::Value,
    bytes: &[u8],
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let at = u64_of(ops, "at")?;
    let count = u64_of(ops, "count")?;
    let old_cols = u64_of(ops, "old_cols")?;
    let fields = cache.schema.fields();
    let n = fields.len();
    let n_rows = cache.rows_total;
    if count == 0 || at > n as u64 || n as u64 + count != old_cols {
        return Err(EditFailure::fatal(format!(
            "restore_cols at {at} count {count}: inconsistent with the {n}-column dataset \
             (the program expects {old_cols} after restore)"
        )));
    }
    let inv = decode_inverse(bytes)?;
    if inv.columns.len() != count as usize || inv.rows() != n_rows {
        return Err(EditFailure::fatal(
            "restore_cols: the blob does not match the program (full-width columns \
             were expected)",
        ));
    }

    let mut out_cols: Vec<OutCol> = Vec::with_capacity(old_cols as usize);
    for (j, f) in fields.iter().enumerate().take(at as usize) {
        out_cols.push(pass_col(f.as_ref().clone(), j));
    }
    for (inv_field, inv_arr) in &inv.columns {
        let build = restored_build(restored_of(inv_field, inv_arr, locale)?);
        out_cols.push(OutCol {
            field: inv_field.clone(),
            old: None,
            build,
            scale_stats: ScaleStats::new(),
            value_count: 0,
            cat: None,
        });
    }
    for (j, f) in fields.iter().enumerate().skip(at as usize) {
        out_cols.push(pass_col(f.as_ref().clone(), j));
    }

    let out_schema = plan_schema(&out_cols);
    let segs = vec![Seg::Block {
        out: 0,
        len: n_rows,
    }];
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        n_rows,
        n_rows,
        locale,
    )?;

    let schema_json = wire_schema(&mut out_cols, locale);
    Ok(EditOutput {
        rows: n_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: messages::Invalidation {
            all: Some(true),
            rows_from: None,
            rows_to: None,
        },
        inverse_meta: Some(messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: job.revision,
            ops: json!({
                "v": 1,
                "op": "delete_cols",
                "at": at,
                "count": count,
                "old_cols": n,
            }),
        }),
        inverse_bytes: Vec::new(),
    })
}

/// `restore_block` (undo of `insert_block`): restore the covered rectangle's old cells
/// from the capture, and trim the extent back (`old_rows` × `old_cols`) — overflow
/// columns and grown rows disappear. The segment formula covers BOTH directions:
/// shrink-undo (window = `capture_rows`, trailing copy ends at `old_rows`) and the
/// grow-redo this executor's own inverse produces (Block's saturating underneath + the
/// trailing-length formula handle rows the current file does not have — they were grown
/// rows, null for pass columns, block cells for touched ones). Fields for touched
/// columns come from the IPC schema (their PRE-PASTE identity, metadata and all).
#[allow(clippy::too_many_lines)]
fn restore_block_exec(
    job: &EditJob,
    cache: &CacheSchema,
    ops: &serde_json::Value,
    bytes: &[u8],
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let anchor = ops
        .get("anchor")
        .ok_or_else(|| EditFailure::fatal("restore_block: no anchor".to_string()))?;
    let row = u64_of(anchor, "row")?;
    let r_len = u64_of(anchor, "rows")?;
    let old_rows = u64_of(ops, "old_rows")?;
    let old_cols = u64_of(ops, "old_cols")?;
    let capture_rows = u64_of(ops, "capture_rows")?;
    let full = ops.get("capture").and_then(|v| v.as_str()) == Some("full");
    let fields = cache.schema.fields();
    let n = fields.len();
    let n_rows = cache.rows_total;

    let Some(prog_cols) = ops.get("columns").and_then(|v| v.as_array()) else {
        return Err(EditFailure::fatal(
            "restore_block: no columns list".to_string(),
        ));
    };
    let prog_indices: Vec<usize> = prog_cols
        .iter()
        .filter_map(|c| c.get("old_index").and_then(|v| v.as_u64()))
        .map(|j| j as usize)
        .collect();
    let existing: Vec<usize> = prog_indices.iter().copied().filter(|&j| j < n).collect();
    // Indices at/beyond the current column count name columns to RECREATE (a redo that
    // re-pastes overflow columns the undo trimmed away) — they must cover exactly the
    // missing tail, or the program is corrupt.
    let recreate: Vec<usize> = prog_indices.iter().copied().filter(|&j| j >= n).collect();
    let expect_recreate: Vec<usize> = (n..old_cols as usize).collect();
    if prog_indices.is_empty()
        || existing.is_empty() && recreate.is_empty()
        || prog_indices.iter().any(|&j| j as u64 >= old_cols)
        || recreate != expect_recreate
    {
        return Err(EditFailure::fatal(
            "restore_block: the program is inconsistent with the dataset",
        ));
    }
    let inv = decode_inverse(bytes)?;
    if inv.columns.len() != prog_indices.len() {
        return Err(EditFailure::fatal(
            "restore_block: the blob does not match the program",
        ));
    }
    let window_out = if full { 0 } else { row };
    if inv.rows() != capture_rows && !(capture_rows == 0 && inv.rows() == 0) {
        return Err(EditFailure::fatal(
            "restore_block: the blob does not match the program",
        ));
    }

    // The redo (computed BEFORE the write): the program that re-pastes. Its capture is
    // the CURRENT window — full-width columns when any EXISTING touched column
    // re-encodes back (I4's uniformity), else the rect — PLUS the overflow columns the
    // undo trimmed (their window slice; every other row of a paste-created column is
    // null), so the redo can recreate them.
    //
    // Re-encoding (d6b's widening): a physical retype (the original check) OR a
    // dictionary REPLACEMENT — a declared Remap keeps DataType::Dictionary but breaks
    // the append-only prefix property that makes key-passthrough valid outside the
    // window (absorption only ever APPENDS, so indices survive). The current
    // dictionaries ride the first batch (the v1 contract) — a one-row sniff read.
    let dict_cols: Vec<usize> = existing
        .iter()
        .copied()
        .filter(|&j| {
            let pos = prog_indices.iter().position(|&t| t == j).unwrap();
            matches!(
                fields[j].data_type(),
                arrow::datatypes::DataType::Dictionary(_, _)
            ) && matches!(
                inv.columns[pos].1.data_type(),
                arrow::datatypes::DataType::Dictionary(_, _)
            )
        })
        .collect();
    let mut cur_values: HashMap<usize, ArrayRef> = HashMap::new();
    if n_rows > 0 && !dict_cols.is_empty() {
        read_range(&job.source, Some(&dict_cols), 1, |batch, _| {
            for (pi, &j) in dict_cols.iter().enumerate() {
                cur_values.entry(j).or_insert_with(|| {
                    Arc::clone(
                        batch
                            .column(pi)
                            .as_dictionary::<arrow::datatypes::Int32Type>()
                            .values(),
                    )
                });
            }
        })?;
    }
    let reencodes = existing.iter().any(|&j| {
        let pos = prog_indices.iter().position(|&t| t == j).unwrap();
        if fields[j].data_type() != inv.columns[pos].0.data_type() {
            return true;
        }
        if let Some(cur) = cur_values.get(&j) {
            let inv_d = inv.columns[pos]
                .1
                .as_dictionary::<arrow::datatypes::Int32Type>();
            let a = cur.as_string::<i32>();
            let b = inv_d.values().as_string::<i32>();
            let prefix = |x: &arrow::array::StringArray, y: &arrow::array::StringArray| {
                y.len() <= x.len() && (0..y.len()).all(|i| x.value(i) == y.value(i))
            };
            if !(prefix(a, b) || prefix(b, a)) {
                return true; // a replaced dictionary — every row's key meaning moved
            }
        }
        false
    });
    let redo_full = reencodes;
    let redo_window = if redo_full {
        n_rows
    } else {
        r_len.min(n_rows.saturating_sub(row))
    };
    // The redo program's column list: the existing touched + the overflow columns
    // [old_cols, n) that the undo trimmed (at redo time they sit beyond the column
    // count and are recreated from the capture).
    let redo_overflow: Vec<usize> = (old_cols as usize..n).collect();
    let redo_indices: Vec<usize> = existing
        .iter()
        .copied()
        .chain(redo_overflow.iter().copied())
        .collect();
    let mut redo_bytes = Vec::new();
    {
        let proj: Vec<usize> = redo_indices.clone();
        let mut captured: Vec<Vec<ArrayRef>> = vec![Vec::new(); proj.len()];
        if n_rows > 0 && redo_window > 0 {
            read_range(&job.source, Some(&proj), n_rows, |batch, base| {
                let bend = base + batch.num_rows() as u64;
                let from = if redo_full { base } else { row.max(base) };
                let to = if redo_full {
                    bend
                } else {
                    (redo_window + row).min(bend)
                };
                if to > from {
                    let (off, len) = ((from - base) as usize, (to - from) as usize);
                    for (pi, slices) in captured.iter_mut().enumerate() {
                        slices.push(batch.column(pi).slice(off, len));
                    }
                }
            })?;
        }
        let inv_schema: SchemaRef = Arc::new(Schema::new(
            proj.iter().map(|&j| fields[j].clone()).collect::<Vec<_>>(),
        ));
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(proj.len());
        for (slices, &j) in captured.into_iter().zip(proj.iter()) {
            let slices = if slices.is_empty() {
                vec![new_null_array(fields[j].data_type(), 0)]
            } else {
                slices
            };
            let refs: Vec<&dyn Array> = slices.iter().map(|a| a.as_ref()).collect();
            arrays.push(
                arrow::compute::concat(&refs)
                    .map_err(|e| EditFailure::fatal(format!("redo capture concat failed: {e}")))?,
            );
        }
        let batch = RecordBatch::try_new(inv_schema.clone(), arrays)
            .map_err(|e| EditFailure::fatal(format!("redo capture batch failed: {e}")))?;
        let mut w = csv2arrow::make_ipc_writer(&mut redo_bytes, &inv_schema)
            .map_err(|e| EditFailure::fatal(format!("redo IPC writer failed: {e}")))?;
        w.write(&batch)
            .map_err(|e| EditFailure::fatal(format!("redo IPC write failed: {e}")))?;
        w.finish()
            .map_err(|e| EditFailure::fatal(format!("redo IPC finish failed: {e}")))?;
    }
    if redo_bytes.len() > messages::MAX_INLINE_PAYLOAD - 64 * 1024 {
        return Err(EditFailure::fatal(
            "the redo is too large to transport — the change itself is too large",
        ));
    }

    // The plan: program columns restore their PRE-PASTE field (the IPC one) and cells —
    // even a ZERO-ROW capture matters, because its values slot carries the pre-edit
    // DICTIONARY (absorption grew it; the surviving keys still index the old list by
    // the append-only prefix property). Columns at/beyond the current count are
    // RECREATED from the capture (a re-paste's overflow). Everything else passes;
    // overflow columns and grown rows are trimmed by the segment table and plan width.
    let mut out_cols: Vec<OutCol> = Vec::with_capacity(old_cols as usize);
    for j in 0..old_cols as usize {
        if let Some(pos) = prog_indices.iter().position(|&t| t == j) {
            let (inv_field, inv_arr) = &inv.columns[pos];
            let build = restored_build(restored_of(inv_field, inv_arr, locale)?);
            out_cols.push(OutCol {
                field: inv_field.clone(),
                old: (j < n).then_some(j),
                build,
                scale_stats: ScaleStats::new(),
                value_count: 0,
                cat: None,
            });
        } else if j < n {
            out_cols.push(pass_col(fields[j].as_ref().clone(), j));
        } else {
            return Err(EditFailure::fatal(
                "restore_block: a restored column has no source in the program",
            ));
        }
    }

    // Segments: pre-copy, holes (a redo anchored beyond the extent), the window, the
    // trailing copy — ending at `old_rows` (the trim). In FULL mode the capture spans
    // the whole column (window at 0); in RECT mode it spans the anchor's rectangle.
    let mut segs: Vec<Seg> = Vec::with_capacity(4);
    // The pre-copy can never run past the TRIMMED extent (a pure-growth anchor sits
    // beyond `old_rows`: every surviving row is an untouched prefix row).
    let pre = window_out.min(n_rows).min(old_rows);
    if pre > 0 {
        segs.push(Seg::Copy {
            out: 0,
            old: 0,
            len: pre,
        });
    }
    if window_out > n_rows {
        segs.push(Seg::Null {
            len: window_out - n_rows,
        });
    }
    if capture_rows > 0 {
        segs.push(Seg::Block {
            out: window_out,
            len: capture_rows,
        });
    }
    let tail_start = window_out + capture_rows;
    if old_rows > tail_start {
        segs.push(Seg::Copy {
            out: tail_start,
            old: tail_start,
            len: old_rows - tail_start,
        });
    }

    let out_schema = plan_schema(&out_cols);
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        old_rows,
        n_rows,
        locale,
    )?;

    let schema_json = wire_schema(&mut out_cols, locale);
    Ok(EditOutput {
        rows: old_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: messages::Invalidation {
            all: Some(true),
            rows_from: None,
            rows_to: None,
        },
        inverse_meta: Some(messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: job.revision,
            ops: json!({
                "v": 1,
                "op": "restore_block",
                "anchor": { "row": row, "col": ops_col(ops), "rows": r_len, "cols": c_len(ops) },
                "old_rows": n_rows,
                "old_cols": n,
                "capture": if redo_full { "full" } else { "rect" },
                "capture_rows": redo_window,
                "columns": redo_indices.iter().map(|&j| json!({
                    "old_index": j,
                    "name": fields[j].name(),
                    "type": csv2arrow::level_of_field(&fields[j]).map(|l| l.as_str()),
                })).collect::<Vec<_>>(),
            }),
        }),
        inverse_bytes: redo_bytes,
    })
}

/// `restore_schema` (undo of `schema_change`): restore every column's pre-change
/// identity — name, display name, type, dictionary, labels — and the old column ORDER;
/// rows never move (one `Block` window over the extent: capture columns replay their IPC
/// field+cells wholesale — metadata rides Arrow — everything else reads the current file
/// underneath). Entries locate their column by `current` (the POST-edit name); a
/// metadata-only entry rebuilds its field from `old_*` — a categorical remaps keys into
/// `dict_from_list(old_levels)` (the recorded list IS the pre-edit dictionary, verbatim
/// order), a scale column passes through (its data never moved). The redo records the
/// post-edit state exactly as d6 recorded the pre-edit one: JSON-only for the metadata
/// classes, a full-width capture (taken BEFORE the write) for the re-encodes.
#[allow(clippy::too_many_lines)]
fn restore_schema_exec(
    job: &EditJob,
    cache: &CacheSchema,
    ops: &serde_json::Value,
    bytes: &[u8],
) -> Result<EditOutput, EditFailure> {
    let locale = Locale::new(job.ingest.decimal_sep, job.ingest.thousands_sep);
    let fields = cache.schema.fields();
    let n = fields.len();
    let n_rows = cache.rows_total;
    let old_cols = u64_of(ops, "old_cols")?;
    let old_rows = u64_of(ops, "old_rows")?;
    if old_cols != n as u64 || old_rows != n_rows {
        return Err(EditFailure::fatal(format!(
            "restore_schema: the program expects {old_cols} columns × {old_rows} rows but the \
             dataset has {n} × {n_rows} (schema_change never changes the extent — a later \
             structural edit broke the stack)"
        )));
    }

    // `old_order`: the pre-edit positions, by name — the permutation the undo restores.
    let Some(order) = ops.get("old_order").and_then(|v| v.as_array()) else {
        return Err(EditFailure::fatal(
            "restore_schema: no 'old_order'".to_string(),
        ));
    };
    let old_order = order
        .iter()
        .map(|v| {
            v.as_str().map(|s| s.to_string()).ok_or_else(|| {
                EditFailure::fatal(
                    "restore_schema: 'old_order' entries must be strings".to_string(),
                )
            })
        })
        .collect::<Result<Vec<_>, _>>()?;
    {
        let mut seen = std::collections::HashSet::new();
        if old_order.len() != n || old_order.iter().any(|s| !seen.insert(s.as_str())) {
            return Err(EditFailure::fatal(
                "restore_schema: 'old_order' is not a permutation of the pre-edit names"
                    .to_string(),
            ));
        }
    }

    let Some(prog_cols) = ops.get("columns").and_then(|v| v.as_array()) else {
        return Err(EditFailure::fatal(
            "restore_schema: no 'columns' list".to_string(),
        ));
    };
    let prog: Vec<SchemaProgCol> = prog_cols
        .iter()
        .map(parse_schema_prog_col)
        .collect::<Result<_, _>>()?;

    // Every entry locates its column by `current`; the currents are distinct (the
    // file's names are) and so are the `old_name`s (the pre-edit schema's were).
    let cur_of: Vec<Option<usize>> = prog
        .iter()
        .map(|e| fields.iter().position(|f| f.name() == e.current.as_str()))
        .collect();
    if let Some(i) = cur_of.iter().position(|c| c.is_none()) {
        return Err(EditFailure::fatal(format!(
            "restore_schema: the program names column '{}' — the dataset has no such column",
            prog[i].current
        )));
    }
    {
        let mut used = vec![false; n];
        for &j in cur_of.iter().flatten() {
            if std::mem::replace(&mut used[j], true) {
                return Err(EditFailure::fatal(
                    "restore_schema: two entries name the same column".to_string(),
                ));
            }
        }
        let mut old_seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for e in &prog {
            if !old_seen.insert(e.old_name.as_str()) {
                return Err(EditFailure::fatal(
                    "restore_schema: two entries restore the same old name".to_string(),
                ));
            }
        }
    }

    // A metadata-only entry cannot change the physical type (P9: only re-encodes do,
    // and those carry a capture). The blob decodes only when some entry declares one —
    // a JSON-only undo ships no bytes.
    let inv = if prog.iter().any(|e| e.capture) {
        let inv = decode_inverse(bytes)?;
        if inv.rows() != n_rows {
            return Err(EditFailure::fatal(
                "restore_schema: the blob does not match the program (full-width columns \
                 were expected)"
                    .to_string(),
            ));
        }
        for e in prog.iter().filter(|e| e.capture) {
            if !inv
                .columns
                .iter()
                .any(|(f, _)| f.name() == e.old_name.as_str())
            {
                return Err(EditFailure::fatal(format!(
                    "restore_schema: the blob has no column '{}' — the capture is incomplete",
                    e.old_name
                )));
            }
        }
        Some(inv)
    } else {
        if !bytes.is_empty() {
            return Err(EditFailure::fatal(
                "restore_schema: the program declares no capture but carries bytes".to_string(),
            ));
        }
        None
    };
    for (e, &j) in prog.iter().zip(cur_of.iter().flatten()) {
        let cur_scale = csv2arrow::level_of_field(&fields[j]) == Some(Level::Scale);
        if !e.capture && cur_scale != (e.old_type == Level::Scale) {
            return Err(EditFailure::fatal(format!(
                "restore_schema: column '{}' claims a metadata-only change but the physical \
                 type disagrees — the program is corrupt",
                e.current
            )));
        }
    }

    // ── Pass A: the CURRENT dictionaries of every categorical program column (first
    // batch only — the v1 contract) feed the key remaps and the redo's `old_levels`;
    // the redo capture (full-width, BEFORE the write) carries the re-encodes' post-edit
    // columns — exactly the state d6 would have recorded. ──
    let mut cur_dicts: HashMap<usize, CatDict> = HashMap::new();
    let first_batch: Vec<usize> = cur_of
        .iter()
        .flatten()
        .copied()
        .filter(|&j| csv2arrow::level_of_field(&fields[j]) != Some(Level::Scale))
        .collect();
    if !first_batch.is_empty() && n_rows > 0 {
        read_range(&job.source, Some(&first_batch), 1, |batch, _base| {
            for (pi, &j) in first_batch.iter().enumerate() {
                let d = batch
                    .column(pi)
                    .as_dictionary::<arrow::datatypes::Int32Type>();
                cur_dicts.entry(j).or_insert_with(|| {
                    csv2arrow::cat_dict_from_values(Arc::clone(d.values()), locale)
                });
            }
        })?;
    }

    let redo_cols: Vec<usize> = (0..prog.len())
        .filter(|&i| prog[i].capture)
        .map(|i| cur_of[i].expect("located above"))
        .collect();
    let mut redo_bytes = Vec::new();
    if !redo_cols.is_empty() {
        let mut captured: Vec<Vec<ArrayRef>> = vec![Vec::new(); redo_cols.len()];
        if n_rows > 0 {
            read_range(&job.source, Some(&redo_cols), n_rows, |batch, _base| {
                for (pi, slices) in captured.iter_mut().enumerate() {
                    slices.push(Arc::clone(batch.column(pi)));
                }
            })?;
        }
        let inv_schema: SchemaRef = Arc::new(Schema::new(
            redo_cols
                .iter()
                .map(|&j| fields[j].clone())
                .collect::<Vec<_>>(),
        ));
        let mut arrays: Vec<ArrayRef> = Vec::with_capacity(redo_cols.len());
        for (slices, &j) in captured.into_iter().zip(redo_cols.iter()) {
            let slices = if slices.is_empty() {
                vec![new_null_array(fields[j].data_type(), 0)]
            } else {
                slices
            };
            let refs: Vec<&dyn Array> = slices.iter().map(|a| a.as_ref()).collect();
            arrays.push(
                arrow::compute::concat(&refs)
                    .map_err(|e| EditFailure::fatal(format!("redo capture concat failed: {e}")))?,
            );
        }
        let batch = RecordBatch::try_new(inv_schema.clone(), arrays)
            .map_err(|e| EditFailure::fatal(format!("redo capture batch failed: {e}")))?;
        let mut w = csv2arrow::make_ipc_writer(&mut redo_bytes, &inv_schema)
            .map_err(|e| EditFailure::fatal(format!("redo IPC writer failed: {e}")))?;
        w.write(&batch)
            .map_err(|e| EditFailure::fatal(format!("redo IPC write failed: {e}")))?;
        w.finish()
            .map_err(|e| EditFailure::fatal(format!("redo IPC finish failed: {e}")))?;
    }
    if redo_bytes.len() > messages::MAX_INLINE_PAYLOAD - 64 * 1024 {
        return Err(EditFailure::fatal(
            "the redo is too large to transport — the change itself is too large".to_string(),
        ));
    }

    // The redo program: `current` = the post-UNDO name (the redo locates columns after
    // the undo restored them); `old_*` = the post-edit state, recorded exactly as d6
    // recorded the pre-edit one.
    let redo_columns: Vec<serde_json::Value> = prog
        .iter()
        .zip(cur_of.iter().flatten())
        .map(|(e, &j)| {
            let cur_level = csv2arrow::level_of_field(&fields[j]).expect("v1 shape guard");
            let old_levels: Option<Vec<String>> = (cur_level != Level::Scale)
                .then(|| {
                    cur_dicts
                        .get(&j)
                        .map(|d| {
                            (0..d.values.len())
                                .map(|i| d.values.as_string::<i32>().value(i).to_string())
                                .collect::<Vec<_>>()
                        })
                        .unwrap_or_default()
                })
                .filter(|l| !l.is_empty());
            json!({
                "current": e.old_name,
                "old_name": e.current,
                "old_display": display_of(&fields[j]),
                "old_type": cur_level.as_str(),
                "old_levels": old_levels,
                "old_labels": csv2arrow::labels_of_field(&fields[j]),
                "capture": e.capture.then_some("full"),
                "capture_rows": e.capture.then_some(n_rows),
            })
        })
        .collect();

    // ── The plan: `out_cols` in `old_order` order. An entry restores its old identity
    // (the capture replays field+cells from the IPC; the metadata classes rebuild the
    // field from `old_*`, remapping categorical keys into the recorded list); an
    // `old_order` name with no entry is a pure-Keep column passing through unchanged. ──
    let mut used = vec![false; n];
    let mut out_cols: Vec<OutCol> = Vec::with_capacity(n);
    for name in &old_order {
        if let Some(i) = prog.iter().position(|e| e.old_name == *name) {
            let e = &prog[i];
            let j = cur_of[i].expect("located above");
            used[j] = true;
            if e.capture {
                let inv = inv.as_ref().expect("decoded above");
                let (inv_field, inv_arr) = &inv
                    .columns
                    .iter()
                    .find(|(f, _)| f.name() == name.as_str())
                    .expect("checked above");
                let build = restored_build(restored_of(inv_field, inv_arr, locale)?);
                out_cols.push(OutCol {
                    field: inv_field.clone(),
                    old: None,
                    build,
                    scale_stats: ScaleStats::new(),
                    value_count: 0,
                    cat: None,
                });
            } else {
                // The all_integer hint survives on the current field (a metadata-only
                // change never touched the data, so the hint never moved).
                let all_int = fields[j]
                    .metadata()
                    .get("jasp:all_integer")
                    .is_some_and(|v| v == "true");
                let mut field =
                    csv2arrow::jasp_field(&e.old_name, &e.old_display, e.old_type, all_int);
                if let Some(l) = &e.old_labels {
                    csv2arrow::attach_labels(&mut field, l);
                }
                let build = match e.old_type {
                    Level::Scale => ColBuild::Pass,
                    _ => match &e.old_levels {
                        Some(levels) => {
                            let dict = csv2arrow::dict_from_list(levels);
                            let key_map: Vec<i32> = match cur_dicts.get(&j) {
                                Some(c) => (0..c.values.len())
                                    .map(|i| {
                                        let v = c.values.as_string::<i32>().value(i);
                                        dict.index.get(v).copied().unwrap_or(-1)
                                    })
                                    .collect(),
                                None => Vec::new(),
                            };
                            ColBuild::RemapKeys {
                                dict,
                                key_map,
                                keys: Vec::new(),
                            }
                        }
                        // An empty pre-edit dictionary (a zero-row cache) — the column
                        // passes through; only its identity moves.
                        None => ColBuild::Pass,
                    },
                };
                out_cols.push(OutCol {
                    field,
                    old: Some(j),
                    build,
                    scale_stats: ScaleStats::new(),
                    value_count: 0,
                    cat: None,
                });
            }
        } else if let Some(j) = fields.iter().position(|f| f.name() == name.as_str()) {
            // A pure-Keep column: same name, same everything — it passes through.
            used[j] = true;
            out_cols.push(pass_col(fields[j].as_ref().clone(), j));
        } else {
            return Err(EditFailure::fatal(format!(
                "restore_schema: 'old_order' names column '{name}' — no entry restores it \
                 and the dataset has no such column"
            )));
        }
    }
    if used.iter().any(|u| !u) {
        return Err(EditFailure::fatal(
            "restore_schema: the program does not account for every column".to_string(),
        ));
    }

    // Rows never move: one `Block` window over the extent (build columns replay window
    // cells; everything else reads the old rows underneath).
    let out_schema = plan_schema(&out_cols);
    let segs = vec![Seg::Block {
        out: 0,
        len: n_rows,
    }];
    stream_rewrite(
        job,
        &out_schema,
        &mut out_cols,
        &segs,
        n_rows,
        n_rows,
        locale,
    )?;

    let schema_json = wire_schema(&mut out_cols, locale);
    Ok(EditOutput {
        rows: n_rows,
        schema: Some(serde_json::Value::Array(schema_json)),
        invalidation: messages::Invalidation {
            all: Some(true),
            rows_from: None,
            rows_to: None,
        },
        inverse_meta: Some(messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: job.revision,
            ops: json!({
                "v": 1,
                "op": "restore_schema",
                "old_order": fields.iter().map(|f| f.name().to_string()).collect::<Vec<_>>(),
                "old_cols": n,
                "old_rows": n_rows,
                "columns": redo_columns,
            }),
        }),
        inverse_bytes: redo_bytes,
    })
}

fn ops_col(ops: &serde_json::Value) -> u64 {
    ops.get("anchor")
        .and_then(|a| a.get("col"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

fn c_len(ops: &serde_json::Value) -> u64 {
    ops.get("anchor")
        .and_then(|a| a.get("cols"))
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
}

/// Serve one `data_edit` work unit. Two-phase (§3 atomicity): the validation below
/// touches only the footer + dictionaries + the parsed block — small things — and the
/// apply pass streams only after every check has passed, so a refusal leaves the
/// dataset untouched.
pub fn serve(job: &EditJob) -> Result<EditOutput, EditFailure> {
    let cache = read_cache_schema(&job.source)?;
    // v1 cache shape guard: every column must map to a measurement level — anything else
    // is not a cache this engine produced, and guessing its semantics would be wrong.
    for f in cache.schema.fields() {
        if csv2arrow::level_of_field(f).is_none() {
            return Err(EditFailure::fatal(format!(
                "column '{}' has unsupported type {} (the v1 cache holds Float64 + \
                 Dictionary(Int32, Utf8) only)",
                f.name(),
                f.data_type()
            )));
        }
    }

    match &job.edit {
        messages::EditOp::InsertBlock {
            row,
            col,
            target_schema,
        } => {
            let block = parse_block(&job.tail)?;
            let c_len = block.cells[0].len() as u64;
            let window = target_schema
                .as_ref()
                .map(|v| parse_window_schema(v, cache.schema.fields(), *col, c_len))
                .transpose()?;
            apply_insert_block(job, &cache, *row, *col, &block, window.as_deref())
        }
        messages::EditOp::InsertRows { at, count } => apply_insert_rows(job, &cache, *at, *count),
        messages::EditOp::DeleteRows { at, count } => apply_delete_rows(job, &cache, *at, *count),
        messages::EditOp::InsertCols { at, columns } => {
            apply_insert_cols(job, &cache, *at, columns)
        }
        messages::EditOp::DeleteCols { at, count } => apply_delete_cols(job, &cache, *at, *count),
        messages::EditOp::SchemaChange { target_schema } => {
            apply_schema_change(job, &cache, target_schema)
        }
        messages::EditOp::ApplyInverse { inverse } => {
            apply_inverse(job, &cache, inverse, &job.tail)
        }
    }
}

// ─── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh converted cache per test (the production pipeline as fixture).
    fn fixture(dir_name: &str, csv_text: &str) -> (std::path::PathBuf, csv2arrow::ConvertOutput) {
        let dir =
            std::env::temp_dir().join(format!("jasp-dataedit-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let csv = dir.join("src.csv");
        let cache = dir.join("out.arrow");
        std::fs::write(&csv, csv_text).unwrap();
        let out = csv2arrow::convert(
            csv.to_str().unwrap(),
            cache.to_str().unwrap(),
            &messages::IngestParams::default(),
        )
        .expect("converts");
        (cache, out)
    }

    fn edit_job(
        source: &std::path::Path,
        cache_path: &str,
        row: u64,
        col: u64,
        tail: &str,
    ) -> EditJob {
        EditJob {
            source: source.to_string_lossy().into_owned(),
            cache_path: cache_path.to_string(),
            ingest: messages::IngestParams::default(),
            revision: 3,
            edit: messages::EditOp::InsertBlock {
                row,
                col,
                target_schema: None,
            },
            tail: tail.as_bytes().to_vec(),
        }
    }

    /// A row-op job (no tail, any edit variant) on the standard revision-3 base.
    fn rows_job(edit: messages::EditOp, source: &std::path::Path, cache_path: &str) -> EditJob {
        EditJob {
            edit,
            ..edit_job(source, cache_path, 0, 0, "")
        }
    }

    /// Read one column of a cache as a materialized Float64 vector (test helper).
    fn read_f64(path: &std::path::Path, name: &str) -> Vec<Option<f64>> {
        let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
        let idx = reader
            .schema()
            .fields()
            .iter()
            .position(|f| f.name() == name)
            .expect("column exists");
        let mut out = Vec::new();
        for batch in reader {
            let b = batch.unwrap();
            let a = b
                .column(idx)
                .as_primitive::<arrow::datatypes::Float64Type>();
            for i in 0..a.len() {
                out.push(a.is_valid(i).then(|| a.value(i)));
            }
        }
        out
    }

    /// Read one dictionary column's VALUE strings per row (test helper).
    fn read_dict_values(path: &std::path::Path, name: &str) -> Vec<Option<String>> {
        let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
        let idx = reader
            .schema()
            .fields()
            .iter()
            .position(|f| f.name() == name)
            .expect("column exists");
        let mut out = Vec::new();
        for batch in reader {
            let b = batch.unwrap();
            let d = b.column(idx).as_dictionary::<arrow::datatypes::Int32Type>();
            let vals = d.values().as_string::<i32>();
            for i in 0..d.len() {
                if d.is_valid(i) {
                    out.push(Some(vals.value(d.keys().value(i) as usize).to_string()));
                } else {
                    out.push(None);
                }
            }
        }
        out
    }

    /// A dictionary column's value list — the field's dictionary itself (first batch;
    /// the v1 cache contract), not the rows (test helper: catches a rewrite that emitted
    /// a different values array for one field).
    fn read_dict_dictionary(path: &std::path::Path, name: &str) -> Vec<String> {
        let mut reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
        let idx = reader
            .schema()
            .fields()
            .iter()
            .position(|f| f.name() == name)
            .expect("column exists");
        match reader.next() {
            Some(Ok(b)) => b
                .column(idx)
                .as_dictionary::<arrow::datatypes::Int32Type>()
                .values()
                .as_string::<i32>()
                .iter()
                .flatten()
                .map(|s| s.to_string())
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A cell for [`direct_cache`] — a scale value or a dictionary string.
    #[derive(Clone, Copy)]
    enum Cell {
        F(f64),
        S(&'static str),
    }

    /// Write a v1-shaped cache with EXPLICIT batch boundaries (conversion writes
    /// byte-budgeted batches — hundreds of thousands of rows — far too heavy for tests;
    /// boundaries are where streaming bugs live, the `read_range` lesson). One shared
    /// dictionary per categorical across all batches, built with the csv2arrow helper
    /// (the no-copies rule holds in tests too).
    fn direct_cache(
        dir_name: &str,
        cols: &[(&str, Level)],
        batches: &[Vec<Vec<Cell>>],
    ) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("jasp-dataedit-{dir_name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.arrow");
        let locale = Locale::new('.', None);
        let schema = Arc::new(Schema::new(
            cols.iter()
                .map(|(n, l)| csv2arrow::jasp_field(n, n, *l, false))
                .collect::<Vec<Field>>(),
        ));
        let dicts: Vec<Option<CatDict>> = (0..cols.len())
            .map(|j| {
                if cols[j].1 == Level::Scale {
                    return None;
                }
                let mut set = IndexSet::new();
                for rows in batches {
                    for r in rows {
                        if let Cell::S(s) = r[j] {
                            set.insert(s.to_string());
                        }
                    }
                }
                Some(csv2arrow::prebuild_dict(
                    set,
                    false,
                    locale,
                    messages::IngestParams::default().sort_limit,
                ))
            })
            .collect();
        let mut w = csv2arrow::make_feather_writer(path.to_str().unwrap(), &schema).unwrap();
        for rows in batches {
            let mut arrays: Vec<ArrayRef> = Vec::with_capacity(cols.len());
            for (j, (_, level)) in cols.iter().enumerate() {
                let arr = match level {
                    Level::Scale => {
                        let mut b = Float64Builder::with_capacity(rows.len());
                        for r in rows {
                            match r[j] {
                                Cell::F(v) => b.append_value(v),
                                _ => b.append_null(),
                            }
                        }
                        Arc::new(b.finish()) as ArrayRef
                    }
                    _ => {
                        let dict = dicts[j].as_ref().unwrap();
                        let mut b = Int32Builder::with_capacity(rows.len());
                        for r in rows {
                            match r[j] {
                                Cell::S(s) => b.append_value(dict.index[s]),
                                _ => b.append_null(),
                            }
                        }
                        Arc::new(
                            DictionaryArray::<arrow::datatypes::Int32Type>::try_new(
                                b.finish(),
                                Arc::clone(&dict.values),
                            )
                            .unwrap(),
                        )
                    }
                };
                arrays.push(arr);
            }
            w.write(&RecordBatch::try_new(schema.clone(), arrays).unwrap())
                .unwrap();
        }
        w.finish().unwrap();
        path
    }

    /// The §1.2 parse-back (d2, kept green): rectangularity, escapes, null-vs-empty.
    #[test]
    fn block_parses_rectangular_cells() {
        let tail = b"val\t\t\\N\nhas\\ttab\t\t\\\\N\n";
        let block = parse_block(tail).expect("parses");
        assert_eq!(block.cells.len(), 2);
        assert_eq!(block.cells[0][0], ("val".to_string(), false));
        assert_eq!(block.cells[0][1], (String::new(), false)); // empty string IS a value
        assert_eq!(block.cells[0][2], (String::new(), true)); // \N = null
        assert_eq!(block.cells[1][0], ("has\ttab".to_string(), false));
        assert_eq!(block.cells[1][2], ("\\N".to_string(), false)); // \\N = the literal string \N
    }

    #[test]
    fn single_lf_is_one_empty_string_cell() {
        let block = parse_block(b"\n").expect("parses");
        assert_eq!(block.cells, vec![vec![(String::new(), false)]]);
    }

    #[test]
    fn block_shape_refusals_are_anchor() {
        for (tail, why) in [
            (&b""[..], "empty"),
            (b"no terminator", "unterminated"),
            (b"a\tb\nc\n", "ragged"),
        ] {
            let err = parse_block(tail).expect_err(why);
            let EditFailure::Validation(issues) = err else {
                panic!("{why}: expected a validation refusal, got {err:?}");
            };
            assert_eq!(issues.len(), 1, "{why}: one issue");
            assert_eq!(issues[0].code, "anchor", "{why}");
        }
        let EditFailure::Validation(issues) = parse_block(b"a\tb\nc\n").unwrap_err() else {
            unreachable!()
        };
        assert_eq!(issues[0].rows, Some(vec![1]));
    }

    /// The cache-schema read + shape guard (d2, kept green).
    #[test]
    fn cache_schema_reads_levels_from_a_converted_cache() {
        let (cache, out) = fixture("levels", "score,group,rank\n1.5,A,1\n2.5,B,2\n3.5,A,3\n");
        assert_eq!(out.rows, 3);
        let cs = read_cache_schema(cache.to_str().unwrap()).expect("footer reads");
        assert_eq!(cs.rows_total, 3);
        let levels: Vec<Level> = cs
            .schema
            .fields()
            .iter()
            .map(|f| csv2arrow::level_of_field(f).expect("v1 shape"))
            .collect();
        assert_eq!(levels, vec![Level::Scale, Level::Nominal, Level::Ordinal]);

        // An Int64 column is not a v1 cache — serve refuses it fatally.
        use arrow::array::Int64Array;
        let dir = cache.parent().unwrap().join("weird");
        std::fs::create_dir_all(&dir).unwrap();
        let weird = dir.join("weird.arrow");
        let schema = Arc::new(Schema::new(vec![arrow::datatypes::Field::new(
            "i",
            arrow::datatypes::DataType::Int64,
            true,
        )]));
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(Int64Array::from(vec![Some(1)])) as ArrayRef],
        )
        .unwrap();
        let mut w = csv2arrow::make_feather_writer(weird.to_str().unwrap(), &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();

        let job = edit_job(&weird, "", 0, 0, "\n");
        let EditFailure::Fatal(msg) = serve(&job).expect_err("refused") else {
            panic!("expected a fatal refusal for a non-v1 cache shape");
        };
        assert!(msg.contains("unsupported type"), "{msg}");
    }

    /// In-range paste into a scale column (the keystroke workhorse): the value changes,
    /// the type stays, untouched columns pass through, the schema recomputes stats, the
    /// invalidation is the covered range, and the inverse captured the old cell.
    #[test]
    fn in_range_paste_into_scale() {
        let (cache, _) = fixture("inrange", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "9.75\n");
        let out = serve(&job).expect("applies");

        assert_eq!(out.rows, 2);
        assert_eq!(read_f64(&out_cache, "score"), vec![Some(9.75), Some(2.5)]);
        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![Some("A".into()), Some("B".into())]
        );

        // The §6 table: extent unchanged → the covered range (rows_to exclusive).
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: None,
                rows_from: Some(0),
                rows_to: Some(1)
            }
        );

        // The schema always ships for insert_block (value counts moved).
        let schema = out.schema.expect("schema present");
        let score = &schema[0];
        assert_eq!(score["type"], "scale");
        assert_eq!(score["value_count"], 2);
        assert_eq!(score["distinct_count"], 2);
        assert_eq!(score["all_integer"], false);
        let group = &schema[1];
        assert_eq!(group["levels"], json!(["A", "B"]));
        assert_eq!(group["value_count"], 2);

        // The inverse: rect capture of the one old cell, machine-exact in IPC.
        let meta = out.inverse_meta.expect("inverse present");
        assert_eq!(meta.format, "arrow_ipc_v1");
        assert_eq!(
            meta.base_revision, 3,
            "the D11-checked revision the edit was computed against"
        );
        let ops = meta.ops;
        assert_eq!(ops["op"], "restore_block");
        assert_eq!(ops["capture"], "rect");
        assert_eq!(ops["capture_rows"], 1);
        assert_eq!(ops["trim_rows_from"], serde_json::Value::Null);
        assert_eq!(ops["trim_cols_from"], serde_json::Value::Null);

        let reader =
            FileReader::try_new(std::io::Cursor::new(&out.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let a = batch
            .column(0)
            .as_primitive::<arrow::datatypes::Float64Type>();
        assert_eq!(a.len(), 1);
        assert_eq!(a.value(0), 1.5);
    }

    /// Absorption on a nominal column (§4 rule 1): a new level appends at the end in
    /// first-appearance order; the dictionary never prunes (unused "A" stays).
    #[test]
    fn absorption_appends_new_levels() {
        let (cache, _) = fixture("absorb", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = edit_job(&cache, out_cache.to_str().unwrap(), 0, 1, "C\n");
        let out = serve(&job).expect("applies");

        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![Some("C".into()), Some("B".into())]
        );
        let schema = out.schema.expect("schema");
        let group = &schema[1];
        assert_eq!(
            group["levels"],
            json!(["A", "B", "C"]),
            "appended at the end"
        );
        assert_eq!(group["distinct_count"], 3);
        assert_eq!(
            group["value_count"], 2,
            "A is unused but retained (no pruning)"
        );
    }

    /// Promotion (§4 rule 2): a scale column receiving text becomes nominal — old f64s
    /// re-encode as canonical level strings, the merged dictionary re-infers
    /// csv2arrow-style (value-sorted within sort_limit), and the inverse captures the
    /// FULL old column (a promotion is a whole-column change, §5).
    #[test]
    fn promotion_retypes_scale_to_nominal() {
        let (cache, _) = fixture("promote", "score,group\n1.5,A\n2.5,B\n3.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = edit_job(&cache, out_cache.to_str().unwrap(), 1, 0, "oops\n");
        let out = serve(&job).expect("applies");

        let vals = read_dict_values(&out_cache, "score");
        assert_eq!(
            vals,
            vec![
                Some("1.5".into()), // canonical 'g'-10 strings (P1)
                Some("oops".into()),
                Some("3.5".into())
            ]
        );
        let schema = out.schema.expect("schema");
        assert_eq!(schema[0]["type"], "nominal");
        // The merged column re-inferred csv2arrow-style: "2.5" was OVERWRITTEN by the
        // block, so it is gone from the data and from the dictionary.
        assert_eq!(schema[0]["levels"], json!(["1.5", "3.5", "oops"]));

        // Column-set/type change → whole-dataset invalidation (§6 table).
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None
            }
        );

        // The inverse: FULL capture of the old Float64 column — machine-exact.
        let ops = &out.inverse_meta.as_ref().unwrap().ops;
        assert_eq!(ops["capture"], "full");
        assert_eq!(ops["capture_rows"], 3);
        let reader =
            FileReader::try_new(std::io::Cursor::new(&out.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let a = batch
            .column(0)
            .as_primitive::<arrow::datatypes::Float64Type>();
        assert_eq!(a.len(), 3);
        assert_eq!(a.value(0).to_bits(), 1.5f64.to_bits());
        assert_eq!(a.value(1).to_bits(), 2.5f64.to_bits());
        assert_eq!(a.value(2).to_bits(), 3.5f64.to_bits());
    }

    /// Machine-exactness where the display grammar would corrupt (D10's reason): a
    /// 17-significant-digit f64 round-trips bitwise through the inverse's IPC — while the
    /// 'g'-10 RENDER of it (what TSV would carry) does not parse back to the same bits.
    #[test]
    fn inverse_is_machine_exact_where_tsv_would_corrupt() {
        let (cache, _) = fixture("exact", "score\n0.3333333333333333\n");
        let orig = 0.3333333333333333f64;
        let rendered = arrowview::render_double(orig, ".", "", 10); // "0.3333333333"
        assert_ne!(
            rendered.parse::<f64>().unwrap().to_bits(),
            orig.to_bits(),
            "precondition: the display grammar IS lossy here"
        );

        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "1\n");
        let out = serve(&job).expect("applies");
        let reader =
            FileReader::try_new(std::io::Cursor::new(&out.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let a = batch
            .column(0)
            .as_primitive::<arrow::datatypes::Float64Type>();
        assert_eq!(
            a.value(0).to_bits(),
            orig.to_bits(),
            "IPC restores the exact bits"
        );
    }

    /// Extent growth: the anchor may name not-yet-existing rows/cols — rows grow with
    /// null holes before the block, new columns infer per D5 and name per the importer
    /// convention, and the invalidation goes whole-dataset (column-set change).
    #[test]
    fn growth_extends_rows_and_columns() {
        let (cache, _) = fixture("grow", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        // 2×2 block at (5, 1): overwrites nothing (rows 5+ are new), extends cols by one.
        let job = edit_job(&cache, out_cache.to_str().unwrap(), 5, 1, "7\tq\n8\tz\n");
        let out = serve(&job).expect("applies");

        assert_eq!(out.rows, 7, "row growth to anchor + block");
        // Col 0 (score) sits OUTSIDE the 2-col block at col 1 — untouched, null in the
        // grown rows (the block covers cols 1–2 only).
        assert_eq!(
            read_f64(&out_cache, "score"),
            vec![Some(1.5), Some(2.5), None, None, None, None, None]
        );
        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![
                Some("A".into()),
                Some("B".into()),
                None,
                None,
                None,
                Some("7".into()),
                Some("8".into())
            ]
        );
        // The new column: V3 (importer convention), inferred nominal from its cells,
        // null before the block.
        assert_eq!(
            read_dict_values(&out_cache, "V3"),
            vec![
                None,
                None,
                None,
                None,
                None,
                Some("q".into()),
                Some("z".into())
            ]
        );
        let schema = out.schema.expect("schema");
        assert_eq!(schema.as_array().unwrap().iter().count(), 3);
        assert_eq!(schema[2]["name"], "V3");
        assert_eq!(
            schema[2]["type"], "nominal",
            "two distinct text cells → nominal"
        );

        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None
            },
            "a column-set change invalidates everything (§6)"
        );

        // The inverse: rect capture missed the old extent (rows 5+ are new) — the undo
        // program is schema-restore + trim.
        let ops = &out.inverse_meta.as_ref().unwrap().ops;
        assert_eq!(ops["capture"], "rect");
        assert_eq!(ops["capture_rows"], 0);
        assert_eq!(ops["trim_rows_from"], 2);
        assert_eq!(ops["trim_cols_from"], 2);
    }

    /// Row-only growth: rows_to-open invalidation (rows_from to the end — new rows
    /// included), and the schema keeps its column set.
    #[test]
    fn row_growth_invalidates_from_the_anchor() {
        let (cache, _) = fixture("rowgrow", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = edit_job(&cache, out_cache.to_str().unwrap(), 2, 0, "9\tB\n"); // 1×2 at the end; "B" is an existing level (no levels change)
        let out = serve(&job).expect("applies");
        assert_eq!(out.rows, 3);
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: None,
                rows_from: Some(2),
                rows_to: None
            }
        );
        assert_eq!(
            out.schema
                .as_ref()
                .unwrap()
                .as_array()
                .unwrap()
                .iter()
                .count(),
            2,
            "no column change"
        );
    }

    /// Atomicity (§3): a refused edit writes NOTHING — no file, no partial dictionary.
    #[test]
    fn refused_edit_writes_nothing() {
        let (cache, _) = fixture("atomic", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let mut job = edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "a\tb\nc\n"); // ragged
        job.tail = b"a\tb\nc\n".to_vec();
        let err = serve(&job).expect_err("refused");
        assert!(matches!(err, EditFailure::Validation(_)));
        assert!(!out_cache.exists(), "nothing written on a refusal");

        // A declared schema with the WRONG WIDTH is a schema_mismatch (P13: the array
        // is positional over the paste) — also atomic. (An empty array against a 1-wide
        // block is the width mismatch; d6b serves the declared path.)
        let job2 = EditJob {
            edit: messages::EditOp::InsertBlock {
                row: 0,
                col: 0,
                target_schema: Some(json!([])),
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "1\n")
        };
        match serve(&job2) {
            Err(EditFailure::Validation(issues)) => {
                assert_eq!(issues[0].code, "schema_mismatch");
            }
            other => panic!("expected a validation refusal, got {other:?}"),
        }
        assert!(!out_cache.exists());
    }

    /// The §6 invalidation table as a unit (normative rows for insert_block).
    #[test]
    fn invalidation_table() {
        let i = |all, from, to| messages::Invalidation {
            all,
            rows_from: from,
            rows_to: to,
        };
        // extent unchanged, schema unchanged
        assert_eq!(
            block_invalidation(2, 2, false, 0, 1, 2),
            i(None, Some(0), Some(1))
        );
        // row growth
        assert_eq!(
            block_invalidation(2, 2, false, 2, 2, 2),
            i(None, Some(2), None)
        );
        // column growth wins
        assert_eq!(
            block_invalidation(2, 3, false, 0, 1, 2),
            i(Some(true), None, None)
        );
        // a type/levels change wins too (§6: "any column-set / order / type / value-levels change")
        assert_eq!(
            block_invalidation(2, 2, true, 0, 1, 2),
            i(Some(true), None, None)
        );
    }

    // ── d6b: insert_block's declared target_schema (P13) ───────────────────

    /// A declared window over a GROWING paste: null = the auto path, non-null = strict
    /// postcondition. Names verbatim (no V{j+1} fallback for declared columns), types
    /// honored, levels VERBATIM in spec order, display_name honored, `all:true` (a
    /// column-set change), schema ships.
    #[test]
    fn insert_block_declared_schema_names_overflow() {
        let (cache, _) = fixture("d6bover", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = EditJob {
            edit: messages::EditOp::InsertBlock {
                row: 1,
                col: 1,
                target_schema: Some(json!([
                    null,
                    { "name": "Q1", "type": "scale" },
                    { "name": "Q3", "type": "nominal", "levels": ["c", "a", "b"],
                      "display_name": "Question 3" }
                ])),
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 1, 1, "D\t9\tc\n")
        };
        let out = serve(&job).expect("the declared paste applies");
        assert_eq!(out.rows, 2);
        assert_eq!(out.invalidation.all, Some(true));
        assert_eq!(out.invalidation.rows_from, None);
        let schema = out.schema.as_ref().unwrap().as_array().unwrap();
        let names: Vec<&str> = schema.iter().map(|c| c["name"].as_str().unwrap()).collect();
        assert_eq!(names, vec!["score", "group", "Q1", "Q3"]);
        let q1 = &schema[2];
        assert_eq!(q1["type"], "scale");
        let q3 = &schema[3];
        assert_eq!(q3["type"], "nominal");
        assert_eq!(q3["display_name"], "Question 3");
        assert_eq!(
            q3["levels"].as_array().unwrap(),
            &["c".to_string(), "a".into(), "b".into()][..]
        );
        // The cells landed: Q1 scale, Q3 keys point at the declared list.
        assert_eq!(read_f64(&out_cache, "Q1"), vec![None, Some(9.0)]);
        assert_eq!(
            read_dict_values(&out_cache, "Q3"),
            vec![None, Some("c".into())]
        );
        // The null entry behaved as undeclared: "group" absorbed "D" (auto path).
        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![Some("A".into()), Some("D".into())]
        );
    }

    /// A covered entry declaring levels REPLACES the dictionary (d6's Remap class):
    /// verbatim order, the row→value mapping outside the window preserved, pasted cells
    /// from the declared list, `all:true` (a value-levels change), FULL inverse capture.
    #[test]
    fn insert_block_declared_schema_covered_remap() {
        let (cache, _) = fixture("d6bremap", "code,label\n1,lo\n2,mid\n1,lo\n2,mid\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = EditJob {
            edit: messages::EditOp::InsertBlock {
                row: 1,
                col: 1,
                target_schema: Some(json!([
                    { "name": "label", "levels": ["mid", "hi", "lo"] }
                ])),
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 1, 1, "hi\n")
        };
        let out = serve(&job).expect("the declared remap applies");
        assert_eq!(out.invalidation.all, Some(true));
        let schema = out.schema.as_ref().unwrap().as_array().unwrap();
        let label = schema.iter().find(|c| c["name"] == "label").unwrap();
        // VERBATIM spec order — never value-sorted (invariant 9).
        assert_eq!(
            label["levels"].as_array().unwrap(),
            &["mid".to_string(), "hi".into(), "lo".into()][..]
        );
        // Outside-window rows keep their VALUES (the row→value mapping survives the
        // reorder); the window takes the pasted cell.
        assert_eq!(
            read_dict_values(&out_cache, "label"),
            vec![
                Some("lo".into()),
                Some("hi".into()),
                Some("lo".into()),
                Some("mid".into())
            ]
        );
        // I4 widened: a Remap re-encodes — the inverse captures the column full-width.
        let ops = &out.inverse_meta.as_ref().unwrap().ops;
        assert_eq!(ops["capture"], "full");
        assert_eq!(ops["capture_rows"], 4);
    }

    /// P13 adherence on covered columns: a declared scale postcondition refuses text
    /// (no automatic promotion under a declaration); a pasted cell outside a declared
    /// level list refuses; a declared level that an OUTSIDE-WINDOW row still uses may
    /// not drop (level_in_use — but a level used ONLY inside the window may).
    #[test]
    fn insert_block_declared_schema_covered_adherence() {
        let (cache, _) = fixture("d6badh", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");

        // Declared scale + text → schema_mismatch (adhere totally).
        let job = EditJob {
            edit: messages::EditOp::InsertBlock {
                row: 1,
                col: 0,
                target_schema: Some(json!([{ "name": "score", "type": "scale" }, null])),
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 1, 0, "oops\tA\n")
        };
        match serve(&job) {
            Err(EditFailure::Validation(issues)) => {
                assert_eq!(issues[0].code, "schema_mismatch");
                assert!(issues[0].message.contains("score"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!out_cache.exists(), "atomic");

        // A pasted cell outside the declared list → schema_mismatch.
        let job2 = EditJob {
            edit: messages::EditOp::InsertBlock {
                row: 1,
                col: 1,
                target_schema: Some(json!([
                    { "name": "group", "levels": ["A", "B"] }
                ])),
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 1, 1, "zzz\n")
        };
        match serve(&job2) {
            Err(EditFailure::Validation(issues)) => {
                assert_eq!(issues[0].code, "schema_mismatch");
                assert!(issues[0].message.contains("zzz"));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!out_cache.exists(), "atomic");

        // level_in_use: the declared list drops "A" but an OUTSIDE-window row uses it.
        let job3 = EditJob {
            edit: messages::EditOp::InsertBlock {
                row: 1,
                col: 1,
                target_schema: Some(json!([
                    { "name": "group", "levels": ["B", "zzz"] }
                ])),
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 1, 1, "zzz\n")
        };
        match serve(&job3) {
            Err(EditFailure::Validation(issues)) => {
                assert_eq!(issues[0].code, "level_in_use");
                assert!(issues[0].message.contains('A'));
            }
            other => panic!("expected a refusal, got {other:?}"),
        }
        assert!(!out_cache.exists(), "atomic");

        // The same drop is FINE when the only in-use rows are inside the window
        // ("B" survives outside; pasting "zzz" over the row that used "B").
        let job4 = EditJob {
            edit: messages::EditOp::InsertBlock {
                row: 1,
                col: 1,
                target_schema: Some(json!([
                    { "name": "group", "levels": ["A", "zzz"] }
                ])),
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 1, 1, "zzz\n")
        };
        let out4 = serve(&job4).expect("window-only usage may drop");
        assert_eq!(out4.invalidation.all, Some(true));
        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![Some("A".into()), Some("zzz".into())]
        );
    }

    /// The P13 structural refusals (all footer-decidable, all atomic).
    #[test]
    fn insert_block_declared_schema_refusals() {
        let (cache, _) = fixture("d6bref", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let refused = |schema: serde_json::Value, tail: &str, col: u64| {
            let job = EditJob {
                edit: messages::EditOp::InsertBlock {
                    row: 0,
                    col,
                    target_schema: Some(schema),
                },
                ..edit_job(&cache, out_cache.to_str().unwrap(), 0, col, tail)
            };
            let err = serve(&job).expect_err("refused");
            assert!(
                matches!(&err, EditFailure::Validation(v) if v[0].code == "schema_mismatch"),
                "wrong failure: {err:?}"
            );
            assert!(!out_cache.exists(), "atomic: nothing written");
        };
        // Wrong width (positional array).
        refused(json!([null]), "1\tA\n", 0);
        // Covered echo mismatch — the array is POSITIONAL.
        refused(json!([{ "name": "group" }, null]), "1\tA\n", 0);
        // Covered type change — schema_change's job (P13 v1).
        refused(
            json!([{ "name": "score", "type": "nominal" }, null]),
            "1\tA\n",
            0,
        );
        // Covered display_name — renaming is schema_change's job.
        refused(
            json!([{ "name": "score", "display_name": "s" }, null]),
            "1\tA\n",
            0,
        );
        // A declared name colliding with a live column (never silently uniquified).
        refused(json!([null, null, { "name": "group" }]), "1\tA\tX\n", 0);
        // Duplicate declared names.
        refused(
            json!([null, { "name": "Q1" }, { "name": "Q1" }]),
            "1\tA\tX\n",
            0,
        );
        // labels are schema_change's job.
        refused(
            json!([{ "name": "score", "labels": { "1.5": "one" } }, null]),
            "1\tA\n",
            0,
        );
        // scale + levels (P8).
        refused(
            json!([null, { "name": "Q1", "type": "scale", "levels": ["1"] }]),
            "1\tA\n",
            1,
        );
        // Unknown type string.
        refused(
            json!([null, { "name": "Q1", "type": "ratio" }]),
            "1\tA\n",
            1,
        );
        // Duplicate levels inside one declared list.
        refused(
            json!([null, { "name": "Q1", "type": "nominal", "levels": ["x", "x"] }]),
            "1\tA\n",
            1,
        );
    }

    /// The crown for the declared paths (P13): both algebraic identities, bit-exact —
    /// declared overflow growth (new columns + declared levels), a covered Remap (the
    /// full-capture mode + the redo's dictionary-replacement detection), and a declared
    /// scale column under row growth.
    #[test]
    fn rt_insert_block_declared_overflow() {
        round_trip_csv(
            "rt-d6b-over",
            "score,group\n1.5,A\n2.5,B\n",
            messages::EditOp::InsertBlock {
                row: 1,
                col: 1,
                target_schema: Some(json!([
                    null,
                    { "name": "Q1", "type": "scale" },
                    { "name": "Q3", "type": "nominal", "levels": ["c", "a", "b"] }
                ])),
            },
            "D\t9\tc\n", // every cell from the declared list; group absorbs "D" (null = auto)
        );
    }

    #[test]
    fn rt_insert_block_declared_remap() {
        round_trip_csv(
            "rt-d6b-remap",
            "code,label\n1,lo\n2,mid\n1,lo\n2,mid\n",
            messages::EditOp::InsertBlock {
                row: 1,
                col: 1,
                target_schema: Some(json!([
                    { "name": "label", "levels": ["mid", "hi", "lo"] }
                ])),
            },
            "hi\n",
        );
    }

    #[test]
    fn rt_insert_block_declared_scale_growth() {
        round_trip_csv(
            "rt-d6b-scale",
            "score,group\n1.5,A\n2.5,B\n",
            messages::EditOp::InsertBlock {
                row: 2,
                col: 1,
                target_schema: Some(json!([
                    null,
                    { "name": "ratio", "type": "scale" }
                ])),
            },
            "A\t0.25\nB\t0.75\n",
        );
    }

    // ── d4: insert_rows / delete_rows ───────────────────────────────────

    /// insert_rows (§3): shift down + null fill. No column touched, nulls move no count
    /// → no schema ships (§4: it ships when it moves); §6 gives `{rows_from: at}`; the
    /// inverse is metadata-only — undo deletes exactly the inserted window (§5).
    #[test]
    fn insert_rows_shifts_and_null_fills() {
        let (cache, _) = fixture("insrows", "score,group\n1.5,A\n2.5,B\n3.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = rows_job(
            messages::EditOp::InsertRows { at: 1, count: 2 },
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(out.rows, 5);
        assert_eq!(
            read_f64(&out_cache, "score"),
            vec![Some(1.5), None, None, Some(2.5), Some(3.5)]
        );
        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![
                Some("A".into()),
                None,
                None,
                Some("B".into()),
                Some("A".into())
            ]
        );

        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: None,
                rows_from: Some(1),
                rows_to: None
            }
        );
        assert_eq!(out.schema, None, "nulls move nothing in the schema");

        let meta = out.inverse_meta.expect("inverse present");
        assert_eq!(meta.base_revision, 3);
        assert_eq!(meta.format, "arrow_ipc_v1");
        assert_eq!(meta.ops["op"], "delete_rows");
        assert_eq!(meta.ops["at"], 1);
        assert_eq!(meta.ops["count"], 2);
        assert_eq!(meta.ops["old_rows"], 3);
        assert!(out.inverse_bytes.is_empty());
    }

    /// insert_rows at 0: the null fill is emitted BEFORE any data piece — the eager
    /// first-batch dictionary capture must have run, or the written file would carry two
    /// values arrays for one field (the writer rejects that). Also: at = rows (append).
    #[test]
    fn insert_rows_at_the_top_and_end() {
        let (cache, _) = fixture("insrows2", "score,group\n1.5,A\n2.5,B\n");
        let top = cache.parent().unwrap().join("top.arrow");
        let job = rows_job(
            messages::EditOp::InsertRows { at: 0, count: 1 },
            &cache,
            top.to_str().unwrap(),
        );
        serve(&job).expect("applies");
        assert_eq!(
            read_dict_values(&top, "group"),
            vec![None, Some("A".into()), Some("B".into())]
        );
        assert_eq!(
            read_dict_dictionary(&top, "group"),
            vec!["A".to_string(), "B".to_string()],
            "the original dictionary — captured before the fill was emitted"
        );

        let end = cache.parent().unwrap().join("end.arrow");
        let job = rows_job(
            messages::EditOp::InsertRows { at: 2, count: 3 },
            &cache,
            end.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");
        assert_eq!(out.rows, 5);
        assert_eq!(
            read_f64(&end, "score"),
            vec![Some(1.5), Some(2.5), None, None, None]
        );
    }

    /// insert_rows on a zero-row cache: dictionary columns have never seen a batch, so
    /// the fill degenerates to empty values — still one dictionary per field. (An empty
    /// CSV infers every column nominal: `decide` sees count 0.)
    #[test]
    fn insert_rows_on_an_empty_dataset() {
        let (cache, converted) = fixture("insrows0", "score,group\n");
        assert_eq!(converted.rows, 0);
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = rows_job(
            messages::EditOp::InsertRows { at: 0, count: 2 },
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");
        assert_eq!(out.rows, 2);
        // An empty CSV infers EVERY column nominal (decide sees count 0) — so both read
        // back as dictionaries; empty ones, one per field.
        assert_eq!(read_dict_values(&out_cache, "score"), vec![None, None]);
        assert_eq!(read_dict_values(&out_cache, "group"), vec![None, None]);
    }

    /// insert_rows refusals (§3 atomicity: nothing written).
    #[test]
    fn insert_rows_refusals_are_range() {
        let (cache, _) = fixture("insrows-r", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        for (at, count) in [(3u64, 1u64), (0, 0)] {
            let job = rows_job(
                messages::EditOp::InsertRows { at, count },
                &cache,
                out_cache.to_str().unwrap(),
            );
            let err = serve(&job).expect_err("refused");
            let EditFailure::Validation(issues) = err else {
                panic!("expected a validation refusal, got {err:?}");
            };
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].code, "range");
        }
        assert!(!out_cache.exists());
    }

    /// delete_rows (§3): shift up. Removed rows remove values → value_count drops → the
    /// schema ships (§4); the dictionary never prunes ("B"/"C" stay as levels). The
    /// inverse carries the removed rows — every column, machine-exact (D10) — for the
    /// multi-step undo (re-insert, then restore; §5).
    #[test]
    fn delete_rows_removes_and_captures() {
        let (cache, _) = fixture("delrows", "score,group\n1.5,A\n2.5,B\n3.5,C\n9.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = rows_job(
            messages::EditOp::DeleteRows { at: 1, count: 2 },
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(out.rows, 2);
        assert_eq!(read_f64(&out_cache, "score"), vec![Some(1.5), Some(9.5)]);
        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![Some("A".into()), Some("A".into())]
        );

        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: None,
                rows_from: Some(1),
                rows_to: None
            }
        );

        let schema = out.schema.expect("value_count moved — schema ships");
        assert_eq!(schema[0]["value_count"], 2);
        assert_eq!(schema[1]["levels"], json!(["A", "B", "C"]), "no pruning");
        assert_eq!(schema[1]["value_count"], 2);
        assert_eq!(schema[1]["distinct_count"], 3);

        let meta = out.inverse_meta.expect("inverse present");
        assert_eq!(meta.base_revision, 3);
        assert_eq!(meta.ops["op"], "restore_rows");
        assert_eq!(meta.ops["at"], 1);
        assert_eq!(meta.ops["count"], 2);
        assert_eq!(meta.ops["old_rows"], 4);
        assert_eq!(meta.ops["columns"].as_array().unwrap().len(), 2);

        let reader =
            FileReader::try_new(std::io::Cursor::new(&out.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let f = batch
            .column(0)
            .as_primitive::<arrow::datatypes::Float64Type>();
        assert_eq!(f.len(), 2);
        assert_eq!(f.value(0).to_bits(), 2.5f64.to_bits());
        assert_eq!(f.value(1).to_bits(), 3.5f64.to_bits());
        let d = batch
            .column(1)
            .as_dictionary::<arrow::datatypes::Int32Type>();
        let vals = d.values().as_string::<i32>();
        assert_eq!(vals.value(d.keys().value(0) as usize), "B");
        assert_eq!(vals.value(d.keys().value(1) as usize), "C");
    }

    /// delete_rows refusals: the window must sit inside the extent (§3 says `range`).
    #[test]
    fn delete_rows_refusals_are_range() {
        let (cache, _) = fixture("delrows-r", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        for (at, count) in [(1u64, 2u64), (0, 0)] {
            let job = rows_job(
                messages::EditOp::DeleteRows { at, count },
                &cache,
                out_cache.to_str().unwrap(),
            );
            let err = serve(&job).expect_err("refused");
            let EditFailure::Validation(issues) = err else {
                panic!("expected a validation refusal, got {err:?}");
            };
            assert_eq!(issues.len(), 1);
            assert_eq!(issues[0].code, "range");
        }
        assert!(!out_cache.exists());
    }

    /// The batch-boundary crown (the `read_range` off-by-one lesson): a cache with
    /// EXPLICIT batches (rows 0–2 | 3–5). Insert at the boundary, delete across it, and
    /// delete a whole tail batch — the row-merge cursor must slice across input batches.
    #[test]
    fn row_ops_across_batch_boundaries() {
        let row = |f: f64, s: &'static str| vec![Cell::F(f), Cell::S(s)];
        let b1: Vec<Vec<Cell>> = (0..3)
            .map(|i| row(i as f64, if i == 1 { "b" } else { "a" }))
            .collect();
        let b2: Vec<Vec<Cell>> = (3..6)
            .map(|i| row(i as f64, if i == 4 { "c" } else { "a" }))
            .collect();
        let cache = direct_cache(
            "bounds",
            &[("score", Level::Scale), ("tag", Level::Nominal)],
            &[b1, b2],
        );

        // Insert exactly at the boundary (row 3): the fill splits the second batch's rows.
        let ins = cache.parent().unwrap().join("ins.arrow");
        let job = rows_job(
            messages::EditOp::InsertRows { at: 3, count: 2 },
            &cache,
            ins.to_str().unwrap(),
        );
        let o = serve(&job).expect("applies");
        assert_eq!(o.rows, 8);
        assert_eq!(
            read_f64(&ins, "score"),
            vec![
                Some(0.),
                Some(1.),
                Some(2.),
                None,
                None,
                Some(3.),
                Some(4.),
                Some(5.)
            ]
        );
        assert_eq!(
            read_dict_values(&ins, "tag"),
            vec![
                Some("a".into()),
                Some("b".into()),
                Some("a".into()),
                None,
                None,
                Some("a".into()),
                Some("c".into()),
                Some("a".into())
            ]
        );

        // Delete across the boundary (rows 2–3, one from each batch): the cursor slices
        // around the removed window inside two different input batches, and the inverse
        // captured both removed rows (concat across batches).
        let del = cache.parent().unwrap().join("del.arrow");
        let job = rows_job(
            messages::EditOp::DeleteRows { at: 2, count: 2 },
            &cache,
            del.to_str().unwrap(),
        );
        let o2 = serve(&job).expect("applies");
        assert_eq!(o2.rows, 4);
        assert_eq!(
            read_f64(&del, "score"),
            vec![Some(0.), Some(1.), Some(4.), Some(5.)]
        );
        let reader =
            FileReader::try_new(std::io::Cursor::new(&o2.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 2);
        let f = batch
            .column(0)
            .as_primitive::<arrow::datatypes::Float64Type>();
        assert_eq!(f.value(0).to_bits(), 2.0f64.to_bits());
        assert_eq!(f.value(1).to_bits(), 3.0f64.to_bits());
        let d = batch
            .column(1)
            .as_dictionary::<arrow::datatypes::Int32Type>();
        let vals = d.values().as_string::<i32>();
        assert_eq!(vals.value(d.keys().value(0) as usize), "a");
        assert_eq!(vals.value(d.keys().value(1) as usize), "a");

        // Delete the entire second batch: the output keeps exactly the first.
        let tail = cache.parent().unwrap().join("tail.arrow");
        let job = rows_job(
            messages::EditOp::DeleteRows { at: 3, count: 3 },
            &cache,
            tail.to_str().unwrap(),
        );
        let o3 = serve(&job).expect("applies");
        assert_eq!(o3.rows, 3);
        assert_eq!(read_f64(&tail, "score"), vec![Some(0.), Some(1.), Some(2.)]);
        assert_eq!(
            read_dict_dictionary(&tail, "tag"),
            vec!["a".to_string(), "b".to_string(), "c".to_string()],
            "no pruning — 'c' survives even though its rows are gone"
        );
    }

    /// Chained edits: an edit's OUTPUT feeds the next edit as its source — the v1 cache
    /// contract (one dictionary per field, emitted once) must survive the round trip
    /// through the engine's own files. insert_block → insert_rows → delete_rows, watching
    /// the dictionaries at every hop.
    #[test]
    fn chained_edits_keep_the_cache_contract() {
        let (cache, _) = fixture("chain", "score,group\n1.5,A\n2.5,B\n");
        let dir = cache.parent().unwrap();

        // rev 1: a 1×2 block at (2, 0) grows the extent and absorbs "C".
        let rev1 = dir.join("rev1.arrow");
        let job = edit_job(&cache, rev1.to_str().unwrap(), 2, 0, "9.5\tC\n");
        let o1 = serve(&job).expect("applies");
        assert_eq!(o1.rows, 3);
        assert_eq!(
            read_f64(&rev1, "score"),
            vec![Some(1.5), Some(2.5), Some(9.5)]
        );
        assert_eq!(
            read_dict_values(&rev1, "group"),
            vec![Some("A".into()), Some("B".into()), Some("C".into())]
        );

        // rev 2: insert_rows on rev1's output — the dictionary carries over verbatim.
        let rev2 = dir.join("rev2.arrow");
        let job2 = rows_job(
            messages::EditOp::InsertRows { at: 1, count: 1 },
            &rev1,
            rev2.to_str().unwrap(),
        );
        let o2 = serve(&job2).expect("applies");
        assert_eq!(o2.rows, 4);
        assert_eq!(
            read_f64(&rev2, "score"),
            vec![Some(1.5), None, Some(2.5), Some(9.5)]
        );
        assert_eq!(
            read_dict_dictionary(&rev2, "group"),
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );

        // rev 3: delete_rows on rev2's output (the hole and the row beneath it).
        let rev3 = dir.join("rev3.arrow");
        let job3 = rows_job(
            messages::EditOp::DeleteRows { at: 1, count: 2 },
            &rev2,
            rev3.to_str().unwrap(),
        );
        let o3 = serve(&job3).expect("applies");
        assert_eq!(o3.rows, 2);
        assert_eq!(read_f64(&rev3, "score"), vec![Some(1.5), Some(9.5)]);
        assert_eq!(
            read_dict_values(&rev3, "group"),
            vec![Some("A".into()), Some("C".into())]
        );
        assert_eq!(
            read_dict_dictionary(&rev3, "group"),
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
            "the dictionary never prunes across a chain of edits"
        );
    }

    // ── d5: insert_cols / delete_cols ─────────────────────────────────────

    /// A spec builder (the wire shape from d1).
    fn spec(name: &str, ty: Option<&str>, levels: Option<Vec<&str>>) -> messages::NewColumnSpec {
        messages::NewColumnSpec {
            name: name.to_string(),
            display_name: None,
            column_type: ty.map(|t| t.to_string()),
            levels: levels.map(|l| l.into_iter().map(|s| s.to_string()).collect()),
        }
    }

    /// The output cache's field names, in order (column position IS schema meaning).
    fn read_field_names(path: &std::path::Path) -> Vec<String> {
        let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
        reader
            .schema()
            .fields()
            .iter()
            .map(|f| f.name().to_string())
            .collect()
    }

    /// insert_cols (§3): the specs land as null-filled columns at `at`, survivors shift,
    /// declared levels pre-populate the dictionary VERBATIM (spec order — an ordinal's
    /// order is its meaning, so "3","1","2" must NOT value-sort), the ordered flag
    /// marks ordinal, and the inverse is metadata-only (nothing was destroyed).
    #[test]
    fn insert_cols_shifts_and_fills() {
        let (cache, _) = fixture("inscols", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = rows_job(
            messages::EditOp::InsertCols {
                at: 1,
                columns: vec![
                    spec("age", Some("scale"), None),
                    spec("rank", Some("ordinal"), Some(vec!["3", "1", "2"])),
                ],
            },
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(out.rows, 2, "rows never move");
        assert_eq!(
            read_field_names(&out_cache),
            vec!["score", "age", "rank", "group"],
            "survivors shift right around the inserted window"
        );
        assert_eq!(read_f64(&out_cache, "age"), vec![None, None]);
        assert_eq!(read_dict_values(&out_cache, "rank"), vec![None, None]);
        assert_eq!(
            read_dict_dictionary(&out_cache, "rank"),
            vec!["3".to_string(), "1".to_string(), "2".to_string()],
            "declared order verbatim — never value-sorted"
        );
        // the ordered flag carries ordinal (the cache's type encoding)
        let reader = FileReader::try_new(File::open(&out_cache).unwrap(), None).unwrap();
        let schema_ref = reader.schema();
        let rank = schema_ref
            .fields()
            .iter()
            .find(|f| f.name() == "rank")
            .unwrap();
        assert_eq!(rank.dict_is_ordered(), Some(true));

        // the wire schema: column order, null-filled stats, spec-order levels
        let schema = out.schema.expect("column set changed — schema ships");
        assert_eq!(
            schema
                .as_array()
                .unwrap()
                .iter()
                .map(|c| c["name"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["score", "age", "rank", "group"]
        );
        assert_eq!(schema[1]["type"], "scale");
        assert_eq!(schema[1]["value_count"], 0);
        assert_eq!(schema[2]["type"], "ordinal");
        assert_eq!(schema[2]["levels"], json!(["3", "1", "2"]));
        assert_eq!(schema[2]["distinct_count"], 3);
        assert_eq!(
            schema[2]["value_count"], 0,
            "pre-declared labels, no cells yet"
        );

        // §6: a column-set change invalidates everything.
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None
            }
        );

        // The inverse: metadata-only — undo deletes exactly the inserted window.
        let meta = out.inverse_meta.expect("inverse present");
        assert_eq!(meta.base_revision, 3);
        assert_eq!(meta.ops["op"], "delete_cols");
        assert_eq!(meta.ops["at"], 1);
        assert_eq!(meta.ops["count"], 2);
        assert_eq!(meta.ops["old_cols"], 2);
        assert!(out.inverse_bytes.is_empty());
    }

    /// P6 uniquification: names loop until GENUINELY free — an earlier insert may already
    /// own the first candidate ("score_2" exists, so "score" → "score_3" → "score_4").
    /// Plus the importer conventions: empty name → V<position>, pure-numeric → V<name>.
    #[test]
    fn insert_cols_names_uniquefy() {
        let (cache, _) = fixture("inscols-n", "score,score_2,score_3\n1,2,3\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = rows_job(
            messages::EditOp::InsertCols {
                at: 1,
                columns: vec![
                    spec("score", None, None),
                    spec("", None, None),
                    spec("5", None, None),
                ],
            },
            &cache,
            out_cache.to_str().unwrap(),
        );
        serve(&job).expect("applies");
        assert_eq!(
            read_field_names(&out_cache),
            vec!["score", "score_4", "V3", "V5", "score_2", "score_3"],
            "score_2/score_3 taken → the loop walks to score_4; empty → V3; 5 → V5"
        );
    }

    /// insert_cols refusals (P7/P8, §3 atomicity): unknown type, scale+levels, duplicate
    /// levels, empty spec list, out-of-range anchor — nothing written for any of them.
    #[test]
    fn insert_cols_refusals() {
        let (cache, _) = fixture("inscols-r", "score,group\n1.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let cases: Vec<(messages::EditOp, &str)> = vec![
            (
                messages::EditOp::InsertCols {
                    at: 0,
                    columns: vec![],
                },
                "empty spec list",
            ),
            (
                messages::EditOp::InsertCols {
                    at: 3,
                    columns: vec![spec("x", None, None)],
                },
                "anchor beyond extent",
            ),
            (
                messages::EditOp::InsertCols {
                    at: 0,
                    columns: vec![spec("x", Some("interval"), None)],
                },
                "unknown type",
            ),
            (
                messages::EditOp::InsertCols {
                    at: 0,
                    columns: vec![spec("x", Some("scale"), Some(vec!["a"]))],
                },
                "scale with levels",
            ),
            (
                messages::EditOp::InsertCols {
                    at: 0,
                    columns: vec![spec("x", Some("nominal"), Some(vec!["a", "a"]))],
                },
                "duplicate levels",
            ),
        ];
        for (edit, why) in cases {
            let job = rows_job(edit, &cache, out_cache.to_str().unwrap());
            let err = serve(&job).expect_err(why);
            let EditFailure::Validation(issues) = err else {
                panic!("{why}: expected a validation refusal, got {err:?}");
            };
            assert_eq!(issues.len(), 1, "{why}: one issue");
            let expected = if why == "empty spec list" || why == "anchor beyond extent" {
                "range"
            } else {
                "schema_mismatch"
            };
            assert_eq!(issues[0].code, expected, "{why}");
        }
        assert!(!out_cache.exists(), "atomicity: nothing written");
    }

    /// delete_cols (§3): survivors shift left, the inverse carries the removed columns
    /// FULL-WIDTH (a column's whole contents were destroyed) machine-exact — f64 bits and
    /// dictionary strings — and `{all:true}` + the schema ship (column set changed).
    #[test]
    fn delete_cols_removes_and_captures() {
        let (cache, _) = fixture("delcols", "score,group,extra\n1.5,A,x\n2.5,B,y\n3.5,C,x\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = rows_job(
            messages::EditOp::DeleteCols { at: 1, count: 1 },
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(out.rows, 3);
        assert_eq!(
            read_field_names(&out_cache),
            vec!["score", "extra"],
            "survivors shift left over the removed window"
        );
        assert_eq!(
            read_dict_values(&out_cache, "extra"),
            vec![Some("x".into()), Some("y".into()), Some("x".into())]
        );

        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None
            }
        );
        let schema = out.schema.expect("schema ships");
        assert_eq!(schema.as_array().unwrap().len(), 2);

        // The inverse: the removed column, full width, machine-exact.
        let meta = out.inverse_meta.expect("inverse present");
        assert_eq!(meta.ops["op"], "restore_cols");
        assert_eq!(meta.ops["at"], 1);
        assert_eq!(meta.ops["count"], 1);
        assert_eq!(meta.ops["old_cols"], 3);
        assert_eq!(meta.ops["columns"][0]["name"], "group");

        let reader =
            FileReader::try_new(std::io::Cursor::new(&out.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 3, "full-width capture");
        let d = batch
            .column(0)
            .as_dictionary::<arrow::datatypes::Int32Type>();
        let vals = d.values().as_string::<i32>();
        assert_eq!(
            (0..3)
                .map(|i| vals.value(d.keys().value(i) as usize).to_string())
                .collect::<Vec<_>>(),
            vec!["A".to_string(), "B".to_string(), "C".to_string()],
            "dictionary-typed capture — restore is type-faithful"
        );
    }

    /// delete_cols refusals (P7): the window must sit inside the extent, and a dataset
    /// must keep at least one column. Atomicity for every refusal.
    #[test]
    fn delete_cols_refusals() {
        let (cache, _) = fixture("delcols-r", "score,group\n1.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        for (at, count, why) in [
            (1u64, 2u64, "window beyond extent"),
            (0, 0, "count 0"),
            (1, 0, "count 0 mid-extent"),
            (0, 2, "the last column"),
            (2, 1, "anchor at extent end"),
            (3, 1, "anchor beyond extent"),
        ] {
            let job = rows_job(
                messages::EditOp::DeleteCols { at, count },
                &cache,
                out_cache.to_str().unwrap(),
            );
            let err = serve(&job).expect_err(why);
            let EditFailure::Validation(issues) = err else {
                panic!("{why}: expected a validation refusal, got {err:?}");
            };
            assert_eq!(issues[0].code, "range", "{why}");
        }
        assert!(!out_cache.exists(), "atomicity: nothing written");
    }

    /// A `schema_change` job helper.
    fn change_job(
        target: serde_json::Value,
        source: &std::path::Path,
        cache_path: &str,
    ) -> EditJob {
        rows_job(
            messages::EditOp::SchemaChange {
                target_schema: target,
            },
            source,
            cache_path,
        )
    }

    /// A field's `jasp:labels` overlay read back from a cache.
    fn read_field_labels(path: &std::path::Path, name: &str) -> Option<serde_json::Value> {
        let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
        let schema = reader.schema();
        let f = schema.fields().iter().find(|f| f.name() == name).unwrap();
        csv2arrow::labels_of_field(f)
    }

    /// A field's ordered flag (nominal↔ordinal).
    fn read_field_ordered(path: &std::path::Path, name: &str) -> bool {
        let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
        let schema = reader.schema();
        let f = schema.fields().iter().find(|f| f.name() == name).unwrap();
        f.dict_is_ordered() == Some(true)
    }

    /// schema_change rename + reorder (§3): the entry array order is the NEW order, a
    /// rename's field name derives from the new display name (P4) and uniquifies (P6).
    /// Reorder → all:true; rename-only → {} — asserted separately below.
    #[test]
    fn schema_change_renames_and_reorders() {
        let (cache, _) = fixture("schrename", "score,group\n1.5,A\n2.5,B\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([
                { "name": "group" },
                { "name": "score", "display_name": "Score v2" },
            ]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(read_field_names(&out_cache), vec!["group", "Score v2"]);
        assert_eq!(read_f64(&out_cache, "Score v2"), vec![Some(1.5), Some(2.5)]);
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None
            },
            "a column-order change invalidates everything (§6)"
        );
        let schema = out.schema.expect("ships");
        assert_eq!(schema[1]["display_name"], "Score v2");

        // The inverse is JSON-only: the old order + the old identity, no capture.
        let meta = out.inverse_meta.unwrap();
        assert_eq!(meta.ops["op"], "restore_schema");
        assert_eq!(meta.ops["old_order"], json!(["score", "group"]));
        let col = &meta.ops["columns"][0];
        assert_eq!(col["current"], "Score v2");
        assert_eq!(col["old_name"], "score");
        assert_eq!(col["old_display"], "score");
        assert_eq!(col["capture"], serde_json::Value::Null);
        assert!(out.inverse_bytes.is_empty());

        // Rename WITHOUT reorder → the §6 rename-only row: {}.
        let out2 = cache.parent().unwrap().join("rev2.arrow");
        let job2 = change_job(
            json!([
                { "name": "score", "display_name": "New Score" },
                { "name": "group" },
            ]),
            &cache,
            out2.to_str().unwrap(),
        );
        let o2 = serve(&job2).expect("applies");
        assert_eq!(
            o2.invalidation,
            messages::Invalidation {
                all: None,
                rows_from: None,
                rows_to: None
            },
            "rename-only: headers ride the schema; no row is stale"
        );
    }

    /// Retype scale → nominal (§4/P1): old f64s re-encode as canonical 'g'-10 level
    /// strings, the dictionary derives csv2arrow-style; the inverse captures the FULL old
    /// Float64 column machine-exact (a re-encode is a whole-column change — P9).
    #[test]
    fn schema_change_retype_scale_to_nominal() {
        let (cache, _) = fixture("schtocat", "score,group\n1.5,A\n2.5,B\n3.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([
                { "name": "score", "type": "nominal" },
                { "name": "group" },
            ]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(
            read_dict_values(&out_cache, "score"),
            vec![Some("1.5".into()), Some("2.5".into()), Some("3.5".into())],
            "canonical 'g'-10 renders (P1)"
        );
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None
            },
            "a type change invalidates everything"
        );
        let schema = out.schema.expect("ships");
        assert_eq!(schema[0]["type"], "nominal");
        assert_eq!(schema[0]["levels"], json!(["1.5", "2.5", "3.5"]));

        // The inverse: the full old Float64 column, bit-exact.
        let meta = out.inverse_meta.as_ref().unwrap();
        assert_eq!(meta.ops["columns"][0]["old_type"], "scale");
        assert_eq!(meta.ops["columns"][0]["capture"], "full");
        let reader =
            FileReader::try_new(std::io::Cursor::new(&out.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let f = batch
            .column(0)
            .as_primitive::<arrow::datatypes::Float64Type>();
        assert_eq!(f.len(), 3);
        assert_eq!(f.value(0).to_bits(), 1.5f64.to_bits());
        assert_eq!(f.value(2).to_bits(), 3.5f64.to_bits());
    }

    /// Retype categorical → scale (§4): in-use levels must all parse (coerce-or-error);
    /// UNUSED levels die with the dictionary — undo restores them via the capture. Also
    /// pins `all_integer` honest recompute from the rendered values. The unused level
    /// arrives the honest way: absorption appended it, an overwrite orphaned it (the
    /// dictionary never prunes).
    #[test]
    fn schema_change_retype_categorical_to_scale() {
        let (cache, _) = fixture("schtoscale", "code\n1\n2\n1\n3\n");
        let dir = cache.parent().unwrap();

        // Absorb "x" (row 0), then overwrite it back to "1" — "x" stays in the
        // dictionary, unused.
        let rev1 = dir.join("rev1.arrow");
        let j1 = edit_job(&cache, rev1.to_str().unwrap(), 0, 0, "x\n");
        serve(&j1).expect("applies");
        let rev2 = dir.join("rev2.arrow");
        let j2 = edit_job(&rev1, rev2.to_str().unwrap(), 0, 0, "1\n");
        serve(&j2).expect("applies");
        assert_eq!(
            read_dict_dictionary(&rev2, "code"),
            vec![
                "1".to_string(),
                "2".to_string(),
                "3".to_string(),
                "x".to_string()
            ],
            "'x' is in the dictionary though no row holds it"
        );

        // Retype to scale: the in-use levels parse; unused 'x' is dropped.
        let out_cache = dir.join("rev3.arrow");
        let job = change_job(
            json!([{ "name": "code", "type": "scale" }]),
            &rev2,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");
        assert_eq!(
            read_f64(&out_cache, "code"),
            vec![Some(1.0), Some(2.0), Some(1.0), Some(3.0)]
        );
        let schema = out.schema.expect("ships");
        assert_eq!(schema[0]["type"], "scale");
        assert_eq!(
            schema[0]["all_integer"], true,
            "recomputed from the rendered values"
        );

        // The inverse carries the old DICTIONARY-typed column — 'x' and all.
        let reader =
            FileReader::try_new(std::io::Cursor::new(&out.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        let d = batch
            .column(0)
            .as_dictionary::<arrow::datatypes::Int32Type>();
        let vals = d.values().as_string::<i32>();
        assert_eq!(vals.len(), 4, "the unused level survived in the capture");
        assert_eq!(vals.value(3), "x");
        assert_eq!(d.keys().value(0), 0);
    }

    /// Retype categorical → scale with an in-use non-numeric level → `coercion`, atomic.
    #[test]
    fn schema_change_coercion_refusal() {
        let (cache, _) = fixture("schcoerce", "group\nA\nB\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([{ "name": "group", "type": "scale" }]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let err = serve(&job).expect_err("refused");
        let EditFailure::Validation(issues) = err else {
            panic!("expected a validation refusal, got {err:?}");
        };
        assert_eq!(issues[0].code, "coercion");
        assert_eq!(issues[0].column.as_deref(), Some("group"));
        assert!(issues[0].message.contains('A'), "names an offending level");
        assert!(!out_cache.exists());
    }

    /// nominal ↔ ordinal is the FLAG-FLIP class (P9): keys and values arrays identical,
    /// only `dict_is_ordered` moves — JSON-only inverse, but all:true (a type change).
    #[test]
    fn schema_change_flag_flip() {
        let (cache, _) = fixture("schflip", "rank\n1\n2\n3\n");
        assert!(
            read_field_ordered(&cache, "rank"),
            "3 distinct ints → ordinal"
        );
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([{ "name": "rank", "type": "nominal" }]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");
        assert!(!read_field_ordered(&out_cache, "rank"));
        assert_eq!(
            read_dict_values(&out_cache, "rank"),
            vec![Some("1".into()), Some("2".into()), Some("3".into())],
            "data untouched — only the flag moved"
        );
        assert_eq!(out.invalidation.all, Some(true));
        let meta = out.inverse_meta.unwrap();
        assert_eq!(meta.ops["columns"][0]["old_type"], "ordinal");
        assert_eq!(meta.ops["columns"][0]["capture"], serde_json::Value::Null);
        assert!(out.inverse_bytes.is_empty());
    }

    /// Level remap (§4): the declared list REPLACES the dictionary — reorder + append are
    /// fine, in-use values must survive, unused may drop. Keys remap; the row→VALUE
    /// mapping is preserved; JSON-only inverse carries the old list.
    #[test]
    fn schema_change_remaps_levels() {
        let (cache, _) = fixture("schremap", "group\nA\nB\nA\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([{ "name": "group", "levels": ["B", "A", "C"] }]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(
            read_dict_dictionary(&out_cache, "group"),
            vec!["B".to_string(), "A".to_string(), "C".to_string()],
            "the declared order, verbatim (dict_from_list — never re-sorted)"
        );
        assert_eq!(
            read_dict_values(&out_cache, "group"),
            vec![Some("A".into()), Some("B".into()), Some("A".into())],
            "the row→value mapping is preserved through the reorder"
        );
        let schema = out.schema.expect("ships");
        assert_eq!(schema[0]["levels"], json!(["B", "A", "C"]));
        assert_eq!(schema[0]["distinct_count"], 3);
        assert_eq!(
            out.invalidation.all,
            Some(true),
            "a value-levels change (I6)"
        );

        let meta = out.inverse_meta.unwrap();
        assert_eq!(meta.ops["columns"][0]["old_levels"], json!(["A", "B"]));
        assert_eq!(meta.ops["columns"][0]["capture"], serde_json::Value::Null);
        assert!(out.inverse_bytes.is_empty(), "JSON-only inverse");
    }

    /// Deleting an in-use level → `level_in_use` (§4), atomic.
    #[test]
    fn schema_change_level_in_use_refusal() {
        let (cache, _) = fixture("schinuse", "group\nA\nB\nA\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([{ "name": "group", "levels": ["B", "C"] }]), // drops in-use "A"
            &cache,
            out_cache.to_str().unwrap(),
        );
        let err = serve(&job).expect_err("refused");
        let EditFailure::Validation(issues) = err else {
            panic!("expected a validation refusal, got {err:?}");
        };
        assert_eq!(issues[0].code, "level_in_use");
        assert!(issues[0].message.contains('A'));
        assert!(!out_cache.exists());
    }

    /// The label overlay (P11): `labels` is a sparse value→display map on the FIELD — a
    /// relabel never touches data (the dictionary is unchanged), the wire schema carries
    /// the overlay, `{}` detaches, and unknown values refuse with `level_unknown`.
    #[test]
    fn schema_change_labels() {
        let (cache, _) = fixture("schlabels", "group\nA\nB\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([{ "name": "group", "labels": { "A": "Alpha" } }]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");

        assert_eq!(
            read_field_labels(&out_cache, "group"),
            Some(json!({ "A": "Alpha" }))
        );
        assert_eq!(
            read_dict_dictionary(&out_cache, "group"),
            vec!["A".to_string(), "B".to_string()],
            "O(k): the data and its dictionary never move"
        );
        let schema = out.schema.expect("ships");
        assert_eq!(schema[0]["labels"], json!({ "A": "Alpha" }));
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: None,
                rows_from: None,
                rows_to: None
            },
            "labels-only: the view renders VALUES — nothing shown moved"
        );
        let meta = out.inverse_meta.unwrap();
        assert_eq!(
            meta.ops["columns"][0]["old_labels"],
            serde_json::Value::Null
        );

        // Detach: an empty map removes the overlay.
        let out2 = cache.parent().unwrap().join("rev2.arrow");
        let job2 = change_job(
            json!([{ "name": "group", "labels": {} }]),
            &out_cache,
            out2.to_str().unwrap(),
        );
        serve(&job2).expect("applies");
        assert_eq!(read_field_labels(&out2, "group"), None, "sparse = absent");

        // A label for a value that is no level → level_unknown.
        let out3 = cache.parent().unwrap().join("rev3.arrow");
        let job3 = change_job(
            json!([{ "name": "group", "labels": { "Z": "zed" } }]),
            &cache,
            out3.to_str().unwrap(),
        );
        let err = serve(&job3).expect_err("refused");
        let EditFailure::Validation(issues) = err else {
            panic!("expected a validation refusal, got {err:?}");
        };
        assert_eq!(issues[0].code, "level_unknown");
    }

    /// Structural refusals (§3 "never changes the column set" + malformed entries), all
    /// atomic.
    #[test]
    fn schema_change_refusals() {
        let (cache, _) = fixture("schrefuse", "score,group\n1.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let cases: Vec<(serde_json::Value, &str)> = vec![
            (json!(null), "not an array"),
            (json!([]), "empty list"),
            (json!([{ "name": "score" }]), "count mismatch"),
            (
                json!([{ "name": "nope" }, { "name": "group" }]),
                "unknown column",
            ),
            (
                json!([{ "name": "score" }, { "name": "score" }]),
                "duplicate entries",
            ),
            (
                json!([{ "name": "score", "type": "interval" }, { "name": "group" }]),
                "unknown type",
            ),
            (
                json!([{ "name": "score", "levels": ["a"] }, { "name": "group" }]),
                "scale with levels",
            ),
            (
                json!([{ "name": "group", "levels": "not-a-list" }]),
                "levels not an array",
            ),
            (
                json!([{ "name": "group", "labels": ["not", "a", "map"] }]),
                "labels not an object",
            ),
            (
                json!([{ "name": "group", "levels": ["A", "A"] }]),
                "duplicate levels",
            ),
        ];
        for (target, why) in cases {
            let job = change_job(target, &cache, out_cache.to_str().unwrap());
            let err = serve(&job).expect_err(why);
            let EditFailure::Validation(issues) = err else {
                panic!("{why}: expected a validation refusal, got {err:?}");
            };
            assert_eq!(issues[0].code, "schema_mismatch", "{why}");
        }
        assert!(!out_cache.exists(), "atomicity: nothing written");
    }

    /// The cap rule (§4, normative): a declared full levels list requires the column's
    /// distinct_count within the wire cap — you may only rewrite a label list you could
    /// have seen in full. 10,001 distinct values → refusal.
    #[test]
    fn schema_change_cap_rule() {
        let mut csv = String::from("v\n");
        for i in 0..=10_000u32 {
            csv.push_str(&format!("v{i}\n"));
        }
        let (cache, converted) = fixture("schcap", &csv);
        assert_eq!(converted.rows, 10_001);
        let v = &converted.columns[0];
        assert_eq!(v.level, "nominal", "10k+ distinct ints → nominal");
        assert_eq!(v.distinct_count, 10_001);

        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([{ "name": "v", "levels": ["0", "1"] }]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let err = serve(&job).expect_err("refused");
        let EditFailure::Validation(issues) = err else {
            panic!("expected a validation refusal, got {err:?}");
        };
        assert_eq!(issues[0].code, "schema_mismatch");
        assert!(issues[0].message.contains("cap"));
        assert!(!out_cache.exists());
    }

    /// The identity no-op is a legal schema_change: applies, nothing stale, the inverse
    /// records nothing.
    #[test]
    fn schema_change_noop() {
        let (cache, _) = fixture("schnoop", "score,group\n1.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let job = change_job(
            json!([{ "name": "score" }, { "name": "group" }]),
            &cache,
            out_cache.to_str().unwrap(),
        );
        let out = serve(&job).expect("applies");
        assert_eq!(out.rows, 1);
        assert_eq!(read_field_names(&out_cache), vec!["score", "group"]);
        assert_eq!(
            out.invalidation,
            messages::Invalidation {
                all: None,
                rows_from: None,
                rows_to: None
            }
        );
        let meta = out.inverse_meta.unwrap();
        assert_eq!(meta.ops["columns"].as_array().unwrap().len(), 0);
        assert!(out.inverse_bytes.is_empty());
    }

    /// The d5 batch-boundary crown on the two-batch cache: full-width column capture
    /// concats across input batches (dictionary slices share one values array), and an
    /// insert at position 0 keeps every dictionary consistent while all survivors shift.
    #[test]
    fn col_ops_across_batch_boundaries() {
        let mk = |i: u64| {
            vec![
                Cell::F(i as f64),
                Cell::S(if i.is_multiple_of(2) { "even" } else { "odd" }),
                Cell::S(if i.is_multiple_of(3) { "lo" } else { "hi" }),
            ]
        };
        let b1: Vec<Vec<Cell>> = (0..3).map(mk).collect();
        let b2: Vec<Vec<Cell>> = (3..6).map(mk).collect();
        let cache = direct_cache(
            "colbounds",
            &[
                ("score", Level::Scale),
                ("tag", Level::Nominal),
                ("band", Level::Ordinal),
            ],
            &[b1, b2],
        );

        // Delete the middle column across the two batches: the capture concatenates
        // one slice per batch into ONE dictionary-typed array.
        let del = cache.parent().unwrap().join("del.arrow");
        let job = rows_job(
            messages::EditOp::DeleteCols { at: 1, count: 1 },
            &cache,
            del.to_str().unwrap(),
        );
        let o = serve(&job).expect("applies");
        let reader = FileReader::try_new(std::io::Cursor::new(&o.inverse_bytes[..]), None).unwrap();
        let batch = reader.into_iter().next().unwrap().unwrap();
        assert_eq!(batch.num_rows(), 6);
        let d = batch
            .column(0)
            .as_dictionary::<arrow::datatypes::Int32Type>();
        let vals = d.values().as_string::<i32>();
        assert_eq!(
            (0..6)
                .map(|i| vals.value(d.keys().value(i) as usize).to_string())
                .collect::<Vec<_>>(),
            vec!["even", "odd", "even", "odd", "even", "odd"]
        );
        assert_eq!(read_field_names(&del), vec!["score", "band"]);

        // Insert at position 0: every survivor shifts, dictionaries stay whole.
        let ins = cache.parent().unwrap().join("ins.arrow");
        let job = rows_job(
            messages::EditOp::InsertCols {
                at: 0,
                columns: vec![spec("new", Some("nominal"), Some(vec!["p", "q"]))],
            },
            &cache,
            ins.to_str().unwrap(),
        );
        serve(&job).expect("applies");
        assert_eq!(read_field_names(&ins), vec!["new", "score", "tag", "band"]);
        assert_eq!(
            read_dict_dictionary(&ins, "band"),
            vec!["hi".to_string(), "lo".to_string()],
            "the shifted ordinal keeps its dictionary verbatim (the fixture's prebuild_dict \
             value-sorted it to [hi, lo] at build — the engine moves it, never re-sorts it)"
        );
        assert_eq!(
            read_dict_values(&ins, "tag"),
            vec![
                Some("even".to_string()),
                Some("odd".to_string()),
                Some("even".to_string()),
                Some("odd".to_string()),
                Some("even".to_string()),
                Some("odd".to_string())
            ],
            "values ride along verbatim under the shifted position"
        );
    }

    // ── d7: the round-trip crown — undo∘edit = id and redo∘undo∘edit = edit ──────

    /// A materialized cell — f64 by BITS (the D10 point: the display grammar is lossy,
    /// IPC is not).
    #[derive(Debug, PartialEq, Clone)]
    enum CellSnap {
        N,
        F(u64),
        S(String),
    }

    /// A column's WHOLE truth: identity (name, display, level, ordered flag,
    /// all_integer, labels overlay), dictionary (ORDER + unused levels — where naive
    /// equality passes but real equality fails), and every cell.
    #[derive(Debug, PartialEq)]
    struct ColSnap {
        name: String,
        display: String,
        level: &'static str,
        ordered: bool,
        all_integer: bool,
        labels: Option<serde_json::Value>,
        dict: Vec<String>,
        cells: Vec<CellSnap>,
    }

    fn snapshot(path: &std::path::Path) -> Vec<ColSnap> {
        let reader = FileReader::try_new(File::open(path).unwrap(), None).unwrap();
        let schema = reader.schema();
        let snaps: std::cell::RefCell<Vec<ColSnap>> = std::cell::RefCell::new(
            schema
                .fields()
                .iter()
                .map(|f| ColSnap {
                    name: f.name().to_string(),
                    display: f
                        .metadata()
                        .get("jasp:display_name")
                        .cloned()
                        .unwrap_or_else(|| f.name().to_string()),
                    level: csv2arrow::level_of_field(f)
                        .map(|l| l.as_str())
                        .unwrap_or("?"),
                    ordered: f.dict_is_ordered() == Some(true),
                    all_integer: f
                        .metadata()
                        .get("jasp:all_integer")
                        .is_some_and(|v| v == "true"),
                    labels: csv2arrow::labels_of_field(f),
                    dict: Vec::new(),
                    cells: Vec::new(),
                })
                .collect(),
        );
        for batch in reader {
            let b = batch.unwrap();
            for (j, oc) in snaps.borrow_mut().iter_mut().enumerate() {
                let arr = b.column(j);
                match csv2arrow::level_of_field(&schema.fields()[j].as_ref().clone()) {
                    Some(Level::Scale) => {
                        let f = arr.as_primitive::<arrow::datatypes::Float64Type>();
                        for i in 0..f.len() {
                            oc.cells.push(if f.is_valid(i) {
                                CellSnap::F(f.value(i).to_bits())
                            } else {
                                CellSnap::N
                            });
                        }
                    }
                    _ => {
                        let d = arr.as_dictionary::<arrow::datatypes::Int32Type>();
                        if oc.dict.is_empty() {
                            oc.dict = d
                                .values()
                                .as_string::<i32>()
                                .iter()
                                .flatten()
                                .map(|s| s.to_string())
                                .collect();
                        }
                        let vals = d.values().as_string::<i32>();
                        for i in 0..d.len() {
                            oc.cells.push(if d.is_valid(i) {
                                CellSnap::S(vals.value(d.keys().value(i) as usize).to_string())
                            } else {
                                CellSnap::N
                            });
                        }
                    }
                }
            }
        }
        snaps.into_inner()
    }

    /// THE d7 assertion: forward → undo → the ORIGINAL (bit-exact, whole truth), and
    /// the undo's own inverse (redo) → the edited state. Both algebraic identities in
    /// one runner — if any restore program is wrong in any way the engine can express,
    /// one of these two equalities fails.
    #[allow(clippy::too_many_arguments)]
    fn round_trip(dir: &str, source: &std::path::Path, edit: messages::EditOp, tail: Vec<u8>) {
        let out_dir = source
            .parent()
            .unwrap()
            .join(format!("rt-{dir}-{}", std::process::id()));
        std::fs::create_dir_all(&out_dir).unwrap();
        let before = snapshot(source);

        // Forward.
        let rev1 = out_dir.join("rev1.arrow");
        let out1 = serve(&EditJob {
            edit,
            tail,
            ..edit_job(source, rev1.to_str().unwrap(), 0, 0, "")
        })
        .expect("the forward edit applies");
        assert!(
            out1.inverse_meta.is_some(),
            "every edit carries its inverse"
        );
        let after_edit = snapshot(&rev1);
        assert_ne!(
            after_edit, before,
            "the edit changed something (test sanity)"
        );

        // Undo: submit the blob verbatim.
        let inv1_meta = out1.inverse_meta.clone().unwrap();
        let rev2 = out_dir.join("rev2.arrow");
        let out2 = serve(&EditJob {
            edit: messages::EditOp::ApplyInverse { inverse: inv1_meta },
            tail: out1.inverse_bytes.clone(),
            ..edit_job(&rev1, rev2.to_str().unwrap(), 0, 0, "")
        })
        .expect("the undo applies");
        assert_eq!(
            snapshot(&rev2),
            before,
            "UNDO IDENTITY FAILED: undo ∘ edit ≠ id for {dir}"
        );

        // Redo: the undo's own inverse.
        let rev3 = out_dir.join("rev3.arrow");
        serve(&EditJob {
            edit: messages::EditOp::ApplyInverse {
                inverse: out2.inverse_meta.clone().unwrap(),
            },
            tail: out2.inverse_bytes.clone(),
            ..edit_job(&rev2, rev3.to_str().unwrap(), 0, 0, "")
        })
        .expect("the redo applies");
        assert_eq!(
            snapshot(&rev3),
            after_edit,
            "REDO IDENTITY FAILED: redo ∘ undo ∘ edit ≠ edit for {dir}"
        );
    }

    /// A round_trip over a converted fixture (csv text → cache).
    fn round_trip_csv(dir: &str, csv: &str, edit: messages::EditOp, tail: &str) {
        let (cache, _) = fixture(dir, csv);
        round_trip(dir, &cache, edit, tail.as_bytes().to_vec());
    }

    #[test]
    fn rt_insert_block_in_range() {
        round_trip_csv(
            "rt-block",
            "score,group\n1.5,A\n2.5,B\n3.5,A\n",
            messages::EditOp::InsertBlock {
                row: 1,
                col: 0,
                target_schema: None,
            },
            "9.75\tC\n",
        );
    }

    #[test]
    fn rt_insert_block_growth() {
        round_trip_csv(
            "rt-grow",
            "score,group\n1.5,A\n2.5,B\n",
            messages::EditOp::InsertBlock {
                row: 4,
                col: 1,
                target_schema: None,
            },
            "7\tq\n8\tz\n",
        );
    }

    #[test]
    fn rt_insert_block_promotion() {
        round_trip_csv(
            "rt-promo",
            "score,group\n1.5,A\n2.5,B\n3.5,A\n",
            messages::EditOp::InsertBlock {
                row: 1,
                col: 0,
                target_schema: None,
            },
            "oops\tB\n",
        );
    }

    #[test]
    fn rt_insert_rows_variants() {
        for (dir, at) in [("rt-ir-mid", 1u64), ("rt-ir-top", 0), ("rt-ir-end", 3)] {
            round_trip_csv(
                dir,
                "score,group\n1.5,A\n2.5,B\n3.5,A\n",
                messages::EditOp::InsertRows { at, count: 2 },
                "",
            );
        }
    }

    #[test]
    fn rt_delete_rows_variants() {
        for (dir, at, count) in [
            ("rt-dr-mid", 1u64, 1u64),
            ("rt-dr-tail", 2, 1),
            ("rt-dr-many", 0, 2),
        ] {
            round_trip_csv(
                dir,
                "score,group\n1.5,A\n2.5,B\n3.5,C\n",
                messages::EditOp::DeleteRows { at, count },
                "",
            );
        }
    }

    #[test]
    fn rt_delete_rows_across_batches() {
        let mk = |i: u64| {
            vec![
                Cell::F(i as f64),
                Cell::S(if i.is_multiple_of(2) { "even" } else { "odd" }),
            ]
        };
        let b1: Vec<Vec<Cell>> = (0..3).map(mk).collect();
        let b2: Vec<Vec<Cell>> = (3..6).map(mk).collect();
        let cache = direct_cache(
            "rt-dr-batch",
            &[("score", Level::Scale), ("tag", Level::Nominal)],
            &[b1, b2],
        );
        round_trip(
            "rt-dr-batch",
            &cache,
            messages::EditOp::DeleteRows { at: 2, count: 2 },
            Vec::new(),
        );
    }

    #[test]
    fn rt_insert_cols_and_delete_cols() {
        let (cache, _) = fixture("rt-cols", "score,group\n1.5,A\n2.5,B\n");
        round_trip(
            "rt-inscols",
            &cache,
            messages::EditOp::InsertCols {
                at: 1,
                columns: vec![
                    spec("age", Some("scale"), None),
                    spec("rank", Some("ordinal"), Some(vec!["3", "1", "2"])),
                ],
            },
            Vec::new(),
        );
        round_trip(
            "rt-delcols",
            &cache,
            messages::EditOp::DeleteCols { at: 0, count: 1 },
            Vec::new(),
        );
    }

    #[test]
    fn rt_insert_cols_name_collision() {
        let (cache, _) = fixture("rt-collide", "score,score_2\n1,2\n3,4\n");
        round_trip(
            "rt-collide",
            &cache,
            messages::EditOp::InsertCols {
                at: 0,
                columns: vec![spec("score", None, None)],
            },
            Vec::new(),
        );
    }

    #[test]
    fn rt_chained_edits_lifo() {
        // e1 → e2 → undo e2 → undo e1 → original (§5's soundness argument, executable).
        let (cache, _) = fixture("rt-lifo", "score,group\n1.5,A\n2.5,B\n");
        let dir = cache.parent().unwrap();
        let before = snapshot(&cache);

        // e1: paste.
        let rev1 = dir.join("l1.arrow");
        let out1 = serve(&edit_job(&cache, rev1.to_str().unwrap(), 1, 0, "9.9\tB\n")).expect("e1");
        // e2: insert rows.
        let rev2 = dir.join("l2.arrow");
        let out2 = serve(&EditJob {
            edit: messages::EditOp::InsertRows { at: 0, count: 1 },
            ..edit_job(&rev1, rev2.to_str().unwrap(), 0, 0, "")
        })
        .expect("e2");

        // undo e2.
        let rev3 = dir.join("l3.arrow");
        let out3 = serve(&EditJob {
            edit: messages::EditOp::ApplyInverse {
                inverse: out2.inverse_meta.clone().unwrap(),
            },
            tail: out2.inverse_bytes.clone(),
            ..edit_job(&rev2, rev3.to_str().unwrap(), 0, 0, "")
        })
        .expect("undo e2");
        assert_eq!(snapshot(&rev3), snapshot(&rev1), "undo e2 → the e1 state");

        // undo e1 → the original.
        let rev4 = dir.join("l4.arrow");
        serve(&EditJob {
            edit: messages::EditOp::ApplyInverse {
                inverse: out1.inverse_meta.clone().unwrap(),
            },
            tail: out1.inverse_bytes.clone(),
            ..edit_job(&rev3, rev4.to_str().unwrap(), 0, 0, "")
        })
        .expect("undo e1");
        assert_eq!(snapshot(&rev4), before, "LIFO: undo both → the original");
        let _ = out3;
    }

    // ── d7b: the schema_change family joins the crown ───────────────────────────

    #[test]
    fn rt_schema_change_rename_reorder() {
        round_trip_csv(
            "rt-sch-rename",
            "score,group\n1.5,A\n2.5,B\n",
            messages::EditOp::SchemaChange {
                target_schema: json!([
                    { "name": "group", "display_name": "The Group" },
                    { "name": "score" },
                ]),
            },
            "",
        );
    }

    #[test]
    fn rt_schema_change_flag_flip_and_labels() {
        round_trip_csv(
            "rt-sch-flag",
            "score,group\n1.5,A\n2.5,B\n",
            messages::EditOp::SchemaChange {
                target_schema: json!([
                    { "name": "score" },
                    { "name": "group", "type": "ordinal", "labels": { "A": "Alpha" } },
                ]),
            },
            "",
        );
    }

    #[test]
    fn rt_schema_change_remaps_levels() {
        round_trip_csv(
            "rt-sch-remap",
            "score,group\n1.5,A\n2.5,B\n3.5,A\n",
            messages::EditOp::SchemaChange {
                target_schema: json!([
                    { "name": "score" },
                    { "name": "group", "levels": ["B", "A"] },
                ]),
            },
            "",
        );
    }

    #[test]
    fn rt_schema_change_scale_to_nominal() {
        round_trip_csv(
            "rt-sch-s2n",
            "score,group\n1.5,A\n2.5,B\n3.5,A\n",
            messages::EditOp::SchemaChange {
                target_schema: json!([
                    { "name": "score", "type": "nominal" },
                    { "name": "group" },
                ]),
            },
            "",
        );
    }

    /// Retype categorical → scale over a numeric categorical (the d6 fixture), with an
    /// ORPHANED level in the dictionary (absorb + overwrite first) — the capture must
    /// restore it (P9: the dictionary is not a function of the data).
    #[test]
    fn rt_schema_change_categorical_to_scale() {
        let (cache, _) = fixture("rt-sch-n2s", "code\n1\n2\n1\n3\n");
        let dir = cache.parent().unwrap();
        // Absorb "x" then overwrite it back — "x" stays in the dictionary, unused.
        let rev1 = dir.join("x1.arrow");
        serve(&edit_job(&cache, rev1.to_str().unwrap(), 0, 0, "x\n")).expect("applies");
        let rev2 = dir.join("x2.arrow");
        serve(&edit_job(&rev1, rev2.to_str().unwrap(), 0, 0, "1\n")).expect("applies");
        round_trip(
            "rt-sch-n2s",
            &rev2,
            messages::EditOp::SchemaChange {
                target_schema: json!([{ "name": "code", "type": "scale" }]),
            },
            Vec::new(),
        );
    }

    /// All classes at once: a rename+reorder on one column, a retype+relabel on the
    /// other — one program, mixed capture and metadata-only entries.
    #[test]
    fn rt_schema_change_mixed() {
        round_trip_csv(
            "rt-sch-mixed",
            "score,group\n1.5,A\n2.5,B\n",
            messages::EditOp::SchemaChange {
                target_schema: json!([
                    { "name": "group", "display_name": "Grp" },
                    { "name": "score", "type": "nominal", "labels": { "1.5": "one point five" } },
                ]),
            },
            "",
        );
    }

    /// The DETACH path needs a source that already carries an overlay — CSV open never
    /// creates one, so attach first (its own crown is above), then detach under test.
    #[test]
    fn rt_schema_change_labels_detach() {
        let (cache, _) = fixture("rt-sch-detach", "score,group\n1.5,A\n2.5,B\n");
        let attach = cache.parent().unwrap().join("attach.arrow");
        serve(&change_job(
            json!([
                { "name": "score" },
                { "name": "group", "labels": { "A": "Alpha" } },
            ]),
            &cache,
            attach.to_str().unwrap(),
        ))
        .expect("attaches");
        round_trip(
            "rt-sch-detach",
            &attach,
            messages::EditOp::SchemaChange {
                target_schema: json!([
                    { "name": "score" },
                    { "name": "group", "labels": {} },
                ]),
            },
            Vec::new(),
        );
    }

    /// Direct asserts the snapshot cannot express: the undo's envelope (all:true, the
    /// schema ships) and the redo program's shape (entries swapped, the retype carrying
    /// its own capture).
    #[test]
    fn restore_schema_undo_output_shape() {
        let (cache, _) = fixture("rs-shape", "score,group\n1.5,A\n2.5,B\n");
        let dir = cache.parent().unwrap();
        let rev1 = dir.join("rev1.arrow");
        let out = serve(&change_job(
            json!([
                { "name": "group", "display_name": "Grp" },
                { "name": "score", "type": "nominal" },
            ]),
            &cache,
            rev1.to_str().unwrap(),
        ))
        .expect("applies");
        assert_eq!(
            read_field_names(&rev1),
            vec!["Grp".to_string(), "score".to_string()],
            "post-edit: entry order + derived names"
        );

        let rev2 = dir.join("rev2.arrow");
        let undo = serve(&EditJob {
            edit: messages::EditOp::ApplyInverse {
                inverse: out.inverse_meta.clone().unwrap(),
            },
            tail: out.inverse_bytes.clone(),
            ..edit_job(&rev1, rev2.to_str().unwrap(), 0, 0, "")
        })
        .expect("undo applies");

        assert_eq!(
            undo.invalidation,
            messages::Invalidation {
                all: Some(true),
                rows_from: None,
                rows_to: None
            },
            "undo invalidates everything (P12)"
        );
        assert!(undo.schema.is_some(), "undo always ships the schema");
        assert_eq!(
            read_field_names(&rev2),
            vec!["score".to_string(), "group".to_string()],
            "old order + old names restored"
        );

        let ops = &undo.inverse_meta.as_ref().unwrap().ops;
        assert_eq!(ops["op"], "restore_schema");
        assert_eq!(ops["old_order"], json!(["Grp", "score"]));
        let cols = ops["columns"].as_array().unwrap();
        let grp = cols.iter().find(|c| c["current"] == "group").unwrap();
        assert_eq!(grp["old_name"], "Grp");
        assert_eq!(grp["old_type"], "nominal");
        assert_eq!(grp["old_levels"], json!(["A", "B"]));
        let score = cols.iter().find(|c| c["current"] == "score").unwrap();
        assert_eq!(score["old_type"], "nominal");
        assert_eq!(score["capture"], "full");
        assert!(
            !undo.inverse_bytes.is_empty(),
            "the redo carries the re-encode"
        );
    }

    /// `restore_schema` defensive refusals (a corrupt program is infrastructure, not a
    /// user error — always Fatal, never a write).
    #[test]
    fn restore_schema_refusals() {
        let (cache, _) = fixture("rs-refuse", "score,group\n1.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        let mk = |ops: serde_json::Value| EditJob {
            edit: messages::EditOp::ApplyInverse {
                inverse: messages::InverseMeta {
                    format: INVERSE_FORMAT_V1.to_string(),
                    base_revision: 3,
                    ops,
                },
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "")
        };
        let entry = |current: &str, old_type: &str| {
            json!({
                "current": current,
                "old_name": "score",
                "old_display": "score",
                "old_type": old_type,
            })
        };
        // Names a column the dataset does not have.
        let e = serve(&mk(json!({
            "v": 1, "op": "restore_schema",
            "old_order": ["score", "group"], "old_cols": 2, "old_rows": 1,
            "columns": [entry("ghost", "scale")],
        })));
        assert!(matches!(e, Err(EditFailure::Fatal(_))), "ghost current");
        // The extent disagrees (a later structural edit broke the stack).
        let e = serve(&mk(json!({
            "v": 1, "op": "restore_schema",
            "old_order": ["score", "group"], "old_cols": 3, "old_rows": 1,
            "columns": [],
        })));
        assert!(matches!(e, Err(EditFailure::Fatal(_))), "extent mismatch");
        // A metadata-only entry whose physical type disagrees.
        let e = serve(&mk(json!({
            "v": 1, "op": "restore_schema",
            "old_order": ["score", "group"], "old_cols": 2, "old_rows": 1,
            "columns": [entry("score", "nominal")],
        })));
        assert!(matches!(e, Err(EditFailure::Fatal(_))), "family mismatch");
        // old_order names a column nothing restores.
        let e = serve(&mk(json!({
            "v": 1, "op": "restore_schema",
            "old_order": ["score", "ghost"], "old_cols": 2, "old_rows": 1,
            "columns": [],
        })));
        assert!(matches!(e, Err(EditFailure::Fatal(_))), "unrestorable name");
        // A declared capture with no blob to serve it.
        let e = serve(&mk(json!({
            "v": 1, "op": "restore_schema",
            "old_order": ["score", "group"], "old_cols": 2, "old_rows": 1,
            "columns": [{
                "current": "group", "old_name": "group", "old_display": "group",
                "old_type": "scale", "capture": "full", "capture_rows": 1,
            }],
        })));
        assert!(
            matches!(e, Err(EditFailure::Fatal(_))),
            "capture without a blob"
        );
        assert!(!out_cache.exists(), "atomicity: nothing written");
    }

    #[test]
    fn apply_inverse_refusals() {
        let (cache, _) = fixture("ai-refuse", "score,group\n1.5,A\n");
        let out_cache = cache.parent().unwrap().join("rev1.arrow");
        // Unknown format.
        let bad_format = messages::InverseMeta {
            format: "magic_tokens_v9".into(),
            base_revision: 3,
            ops: json!({ "v": 1, "op": "delete_rows", "at": 0, "count": 1 }),
        };
        let job = EditJob {
            edit: messages::EditOp::ApplyInverse {
                inverse: bad_format,
            },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "")
        };
        assert!(matches!(serve(&job), Err(EditFailure::Fatal(_))));
        // Stale base revision (the stack is no longer LIFO).
        let stale = messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: 99,
            ops: json!({ "v": 1, "op": "delete_rows", "at": 0, "count": 1, "old_rows": 1 }),
        };
        let job = EditJob {
            edit: messages::EditOp::ApplyInverse { inverse: stale },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "")
        };
        let err = serve(&job).expect_err("refused");
        let EditFailure::Validation(issues) = err else {
            panic!("expected a validation refusal, got {err:?}");
        };
        assert_eq!(issues[0].code, "stale_edit");
        // Unknown program.
        let unknown = messages::InverseMeta {
            format: INVERSE_FORMAT_V1.to_string(),
            base_revision: 3,
            ops: json!({ "v": 1, "op": "time_travel" }),
        };
        let job = EditJob {
            edit: messages::EditOp::ApplyInverse { inverse: unknown },
            ..edit_job(&cache, out_cache.to_str().unwrap(), 0, 0, "")
        };
        assert!(matches!(serve(&job), Err(EditFailure::Fatal(_))));
        assert!(!out_cache.exists(), "atomicity: nothing written");
    }
}
