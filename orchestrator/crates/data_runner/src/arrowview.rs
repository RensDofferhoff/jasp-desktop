//! Lane-side serving of `data_view` — windowed slices of a cached Feather rendered as
//! escaped TSV (data-view-format.md §1.2 is the **normative grammar**).
//!
//! The lane is the **only** float→string converter: cells are rendered per the request's
//! `render` spec (`'g'` semantics — exact legacy `QLocale::toString(dbl, 'g', 10)` parity),
//! escaped (`\t \n \r \\`; null = whole-cell `\N`), and counted in **escaped** bytes against
//! the response budget. Chunks are always whole rows: the lane stops at a row boundary before
//! exceeding `max_bytes`, with a progress guarantee — if nothing has been emitted yet it emits
//! the next row regardless of size, unless that single row exceeds the wire ceiling
//! ([`crate::messages::MAX_INLINE_PAYLOAD`] minus envelope margin), which is a `fatalError`
//! ("row too large to transport", with the row index) instead of an emission.
//!
//! Slicing: arrow-ipc 59's `FileReader` has **no row-random-access API** (the design docs'
//! `read_range` does not exist in this version) — instead the footer is parsed for the
//! per-block row counts (metadata only, no batch bodies decoded), the block containing
//! `row_offset` is located, and decoding starts there (`set_index`). Deep windows thus cost
//! only the window itself, not the skipped prefix. Sequential fill (buffered mode) is
//! unaffected.

use crate::messages;

use arrow::array::{Array, AsArray, DictionaryArray, Float64Array};
use arrow::datatypes::{DataType, Float64Type, Int32Type};
use arrow::record_batch::RecordBatch;
use arrow_ipc::reader::FileReader;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};

/// Envelope margin reserved on top of [`messages::MAX_INLINE_PAYLOAD`] for the JSON envelope
/// + framing of a view result (format doc §1.1: the hard cap is "256 MiB − envelope margin").
const ROW_CEILING: usize = messages::MAX_INLINE_PAYLOAD - 64 * 1024;

/// The IPC encapsulated-message continuation marker (post-0.15 files).
const CONTINUATION_MARKER: [u8; 4] = [0xFF, 0xFF, 0xFF, 0xFF];

/// The window a `data_view` work asks for.
pub struct ViewRequest<'a> {
    pub row_offset: u64,
    pub row_limit: Option<u64>,
    /// DISPLAY names (§24.4: the frontend never sees canonical names); `None` = all columns.
    pub columns: Option<&'a [String]>,
    pub max_bytes: u64,
    pub render: Option<&'a messages::ViewRender>,
}

/// The served chunk: `tsv` is exactly the frame's binary part (whole LF-terminated rows).
#[derive(Debug)]
pub struct ViewOutput {
    pub tsv: Vec<u8>,
    /// TOTAL rows of the dataset at serve time.
    pub rows_total: u64,
    pub row_offset: u64,
    pub row_count: u64,
    pub truncated: bool,
}

/// Serve one view window from the Feather at `cache_path`. Every `Err` is user-visible
/// (`fatalError` on the work): missing/corrupt cache, unknown column, unsupported type,
/// oversized row.
pub fn serve(cache_path: &str, req: &ViewRequest) -> Result<ViewOutput, String> {
    serve_inner(cache_path, req, ROW_CEILING)
}

fn serve_inner(
    cache_path: &str,
    req: &ViewRequest,
    row_ceiling: usize,
) -> Result<ViewOutput, String> {
    let cache_err = |e: String| format!("corrupt cache file '{cache_path}': {e}");
    let mut file = File::open(cache_path)
        .map_err(|e| format!("cannot open cache file '{cache_path}': {e}"))?;
    // Footer pass: schema + per-block row counts (metadata only — no bodies decoded).
    let (schema, block_rows) = footer_info(&mut file).map_err(cache_err)?;
    let rows_total: u64 = block_rows.iter().sum();

    // Column selection: DISPLAY name → field index via `jasp:display_name` field metadata
    // (canonical-name fallback for direct consumers/tests). Response order = request order.
    let projection: Option<Vec<usize>> = match req.columns {
        None => None,
        Some(names) => {
            let mut idx = Vec::with_capacity(names.len());
            for name in names {
                let pos = schema
                    .fields()
                    .iter()
                    .position(|f| {
                        f.metadata().get("jasp:display_name").map(String::as_str)
                            == Some(name.as_str())
                            || f.name() == name
                    })
                    .ok_or_else(|| format!("unknown column '{name}'"))?;
                idx.push(pos);
            }
            Some(idx)
        }
    };

    let render = req
        .render
        .cloned()
        .unwrap_or_else(messages::ViewRender::default);
    let mut out = ViewOutput {
        tsv: Vec::new(),
        rows_total,
        row_offset: req.row_offset,
        row_count: 0,
        truncated: false,
    };
    if rows_total == 0 || req.row_offset >= rows_total {
        return Ok(out); // at the end: zero bytes, row_count 0 (format doc §1.1)
    }

    // Locate the block containing row_offset; decoding starts there (no skipped decodes).
    let mut block_start_row: u64 = 0;
    let mut start_block = 0usize;
    for (i, n) in block_rows.iter().enumerate() {
        if req.row_offset < block_start_row + n {
            start_block = i;
            break;
        }
        block_start_row += n;
    }
    let file2 = File::open(cache_path)
        .map_err(|e| format!("cannot open cache file '{cache_path}': {e}"))?;
    let mut reader = FileReader::try_new(file2, projection)
        .map_err(|e| cache_err(format!("cannot decode: {e}")))?;
    reader
        .set_index(start_block)
        .map_err(|e| cache_err(format!("cannot seek block {start_block}: {e}")))?;

    let mut next_row = block_start_row; // global row of the current batch's first row
    let mut row = req.row_offset; // next row to emit
    let end = match req.row_limit {
        Some(limit) => rows_total.min(req.row_offset.saturating_add(limit)),
        None => rows_total,
    };
    let budget = req.max_bytes as usize;
    let mut row_buf: Vec<u8> = Vec::new();

    while row < end {
        let Some(batch) = reader
            .next()
            .transpose()
            .map_err(|e| cache_err(format!("read failed at row {row}: {e}")))?
        else {
            break; // file ended early (shouldn't — the footer promised these rows)
        };
        let batch_rows = batch.num_rows() as u64;
        let batch_end = next_row + batch_rows;
        if batch_end <= req.row_offset {
            next_row = batch_end;
            continue; // entirely before the window (only possible for the start block)
        }
        let skip = req.row_offset.saturating_sub(next_row).min(batch_rows);
        let cols = column_views(&batch)?;
        for i in (skip as usize)..(batch_rows as usize) {
            row_buf.clear();
            render_row(&cols, i, &render, &mut row_buf)?;
            let fits = out.tsv.len() + row_buf.len() <= budget || out.row_count == 0;
            if !fits {
                out.truncated = true;
                return Ok(out); // stop at the row boundary before exceeding max_bytes
            }
            if row_buf.len() > row_ceiling {
                return Err(format!(
                    "row {row} is {} bytes escaped — too large to transport (ceiling {row_ceiling})",
                    row_buf.len()
                ));
            }
            out.tsv.extend_from_slice(&row_buf);
            out.row_count += 1;
            row += 1;
            if row >= end {
                return Ok(out); // row_limit/end reached — NOT truncated
            }
        }
        next_row = batch_end;
    }
    Ok(out)
}

/// Parse the Feather footer: the Arrow schema + the row count of every record-batch block.
/// Reads only the encapsulated message metadata per block — batch bodies stay untouched, so
/// this is O(blocks) tiny reads regardless of file size.
pub(crate) fn footer_info(file: &mut File) -> Result<(arrow::datatypes::Schema, Vec<u64>), String> {
    let io = |e: std::io::Error| e.to_string();
    let mut tail = [0u8; 10];
    file.seek(SeekFrom::End(-10)).map_err(io)?;
    file.read_exact(&mut tail).map_err(io)?;
    let footer_len = arrow_ipc::reader::read_footer_length(tail).map_err(|e| e.to_string())?;
    let mut footer_data = vec![0u8; footer_len];
    file.seek(SeekFrom::End(-10 - footer_len as i64))
        .map_err(io)?;
    file.read_exact(&mut footer_data).map_err(io)?;
    let footer = arrow_ipc::root_as_footer(&footer_data).map_err(|e| format!("bad footer: {e}"))?;
    let schema = arrow_ipc::convert::fb_to_schema(
        footer
            .schema()
            .ok_or_else(|| "footer has no schema".to_string())?,
    );
    let mut rows = Vec::new();
    if let Some(blocks) = footer.recordBatches() {
        for block in blocks {
            file.seek(SeekFrom::Start(block.offset() as u64))
                .map_err(io)?;
            let mut meta = vec![0u8; block.metaDataLength() as usize];
            file.read_exact(&mut meta).map_err(io)?;
            let fb = if meta.len() >= 8 && meta[..4] == CONTINUATION_MARKER {
                &meta[8..]
            } else if meta.len() >= 4 {
                &meta[4..]
            } else {
                return Err("block metadata too small".to_string());
            };
            let msg = arrow_ipc::root_as_message(fb).map_err(|e| format!("bad block: {e}"))?;
            let rb = msg
                .header_as_record_batch()
                .ok_or_else(|| "block is not a record batch".to_string())?;
            rows.push(rb.length() as u64);
        }
    }
    Ok((schema, rows))
}

/// A batch's columns, downcast once per batch to the two cache shapes (output contract,
/// neo-jasp §8.3): scale = `Float64`, categorical = `Dictionary(Int32, Utf8)`.
enum ColView<'a> {
    Float(&'a Float64Array),
    Dict(&'a DictionaryArray<Int32Type>),
}

fn column_views<'b>(batch: &'b RecordBatch) -> Result<Vec<ColView<'b>>, String> {
    batch
        .columns()
        .iter()
        .map(|col| match col.data_type() {
            DataType::Float64 => Ok(ColView::Float(col.as_primitive::<Float64Type>())),
            DataType::Dictionary(k, v)
                if matches!(**k, DataType::Int32) && matches!(**v, DataType::Utf8) =>
            {
                Ok(ColView::Dict(col.as_dictionary::<Int32Type>()))
            }
            other => Err(format!(
                "unsupported column type {other} (the v1 cache holds Float64 + Dictionary(Int32, Utf8) only)"
            )),
        })
        .collect()
}

/// Render one row into `out` per the normative grammar: TAB separators, LF terminator after
/// every row (including the last), null = whole-cell `\N` (format doc §1.2).
fn render_row(
    cols: &[ColView],
    i: usize,
    render: &messages::ViewRender,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    for (c, col) in cols.iter().enumerate() {
        if c > 0 {
            out.push(b'\t');
        }
        match col {
            ColView::Float(a) if a.is_valid(i) => {
                let s = render_double(
                    a.value(i),
                    &render.decimal,
                    &render.thousands,
                    render.precision,
                );
                escape_into(&s, out);
            }
            ColView::Dict(d) if d.is_valid(i) => {
                let key = d.keys().value(i) as usize;
                let value = d.values().as_string::<i32>().value(key);
                escape_into(value, out); // dictionary VALUE verbatim — data, never formatted
            }
            _ => out.extend_from_slice(b"\\N"), // null (any type)
        }
    }
    out.push(b'\n');
    Ok(())
}

/// Escape one cell into the TSV stream (format doc §1.2): backslash-escape TAB/LF/CR/
/// backslash and **nothing else**. A literal `\N` cell thus encodes as `\\N` — unambiguous
/// by construction; no special case needed. Real TAB/LF bytes never survive escaping, so
/// separators stay byte-unambiguous and splitting is a raw scan.
fn escape_into(cell: &str, out: &mut Vec<u8>) {
    for &b in cell.as_bytes() {
        match b {
            b'\t' => out.extend_from_slice(b"\\t"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\\' => out.extend_from_slice(b"\\\\"),
            other => out.push(other),
        }
    }
}

/// Split one §1.2 row's bytes into cell slices — a raw TAB scan with NO escape logic
/// (real TABs never survive escaping, format doc §1.2's byte-uniqueness property). The
/// row excludes its LF terminator. Unused until the edit engine lands (d3).
#[allow(dead_code)]
pub(crate) fn split_row(row: &[u8]) -> Vec<&[u8]> {
    row.split(|&b| b == b'\t').collect()
}

/// Parse one cell back (format doc §1.2 — the edit path's parse-back of the render grammar):
/// the WHOLE cell `\N` = null (a literal `\N` cell arrives as `\\N` and unescapes
/// normally); otherwise unescape `\t \n \r \\`. Cells without a backslash skip unescaping
/// entirely (the common case). An unknown escape sequence (a frontend authoring bug — the
/// grammar never emits one) passes through as literals rather than dropping bytes.
/// Returns `(text, is_null)`; an empty cell is a VALUE, distinct from null.
#[allow(dead_code)]
pub(crate) fn unescape_cell(cell: &[u8]) -> (String, bool) {
    if cell == b"\\N" {
        return (String::new(), true);
    }
    if !cell.contains(&b'\\') {
        return (String::from_utf8_lossy(cell).into_owned(), false);
    }
    let mut out = Vec::with_capacity(cell.len());
    let mut i = 0;
    while i < cell.len() {
        if cell[i] == b'\\' && i + 1 < cell.len() {
            match cell[i + 1] {
                b't' => {
                    out.push(b'\t');
                    i += 2;
                    continue;
                }
                b'n' => {
                    out.push(b'\n');
                    i += 2;
                    continue;
                }
                b'r' => {
                    out.push(b'\r');
                    i += 2;
                    continue;
                }
                b'\\' => {
                    out.push(b'\\');
                    i += 2;
                    continue;
                }
                _ => {
                    out.push(cell[i]);
                    i += 1;
                    continue;
                }
            }
        }
        out.push(cell[i]);
        i += 1;
    }
    (String::from_utf8_lossy(&out).into_owned(), false)
}

/// C/Qt `'g'` rendering — exact legacy parity (`QLocale::toString(dbl, 'g', 10)`,
/// `CommonData/columnutils.cpp:227` + `QMLComponents/utilities/qutils.cpp:509`): round to
/// `precision` significant digits; exponent form exactly when the decimal exponent is
/// < −4 or ≥ `precision` (exponent as `e±NN`, ≥ 2 digits: `2e-07`, `1e+10`). The plain form
/// gets `thousands` grouping on the integer part and `decimal` as separator; the exponent
/// form localizes only the mantissa's decimal separator.
///
/// `'g'` strips trailing zeros — integral values render without a decimal point (`42`), so
/// `all_integer` needs no special case. NaN/±Inf cannot occur in v1 caches (the CSV lane
/// maps unparseables to null); they get placeholder renderings until a future write path
/// pins dedicated ones (format doc §1.2).
pub fn render_double(x: f64, decimal: &str, thousands: &str, precision: u32) -> String {
    if x.is_nan() {
        return "NaN".to_string();
    }
    if x.is_infinite() {
        return if x > 0.0 { "inf".into() } else { "-inf".into() };
    }
    let (sign, mag) = if x.is_sign_negative() {
        ("-", -x)
    } else {
        ("", x)
    };
    if mag == 0.0 {
        return format!("{sign}0");
    }
    let p = precision.max(1) as usize;
    // Scientific at p significant digits: correctly-rounded 1.(p-1) digits + exponent.
    let sci = format!("{:.*e}", p - 1, mag);
    let (mant, exp_str) = sci.split_once('e').expect("std 'e' format has an exponent");
    let exp: i32 = exp_str.parse().expect("valid exponent");
    // Mantissa digits without the point, trailing zeros stripped ('g' parity).
    let mut digits: String = mant.chars().filter(|c| *c != '.').collect();
    while digits.ends_with('0') {
        digits.pop();
    }
    if digits.is_empty() {
        digits.push('0');
    }
    let mut out = String::with_capacity(digits.len() + 8);
    out.push_str(sign);
    if exp >= -4 && exp < p as i32 {
        // Plain form ('%g' rule: '%f' iff P > X ≥ −4, else '%e').
        if exp < 0 {
            out.push('0');
            out.push_str(decimal);
            for _ in 0..(-exp - 1) {
                out.push('0');
            }
            out.push_str(&digits);
        } else {
            let int_len = exp as usize + 1;
            if int_len >= digits.len() {
                let mut int_part = digits;
                while int_part.len() < int_len {
                    int_part.push('0'); // magnitude edge: integral part gets its zeros
                }
                out.push_str(&group_digits(&int_part, thousands));
            } else {
                let (int_part, frac) = digits.split_at(int_len);
                out.push_str(&group_digits(int_part, thousands));
                out.push_str(decimal);
                out.push_str(frac);
            }
        }
    } else {
        // Exponent form: localize only the mantissa's decimal separator (no grouping).
        out.push_str(&digits[..1]);
        if digits.len() > 1 {
            out.push_str(decimal);
            out.push_str(&digits[1..]);
        }
        out.push('e');
        out.push(if exp < 0 { '-' } else { '+' });
        let ae = exp.unsigned_abs();
        if ae < 10 {
            out.push('0'); // ≥ 2 exponent digits: e-07, e+10
        }
        out.push_str(&ae.to_string());
    }
    out
}

/// Group an ASCII digit string from the right in threes with `thousands` ("" = verbatim).
fn group_digits(int_part: &str, thousands: &str) -> String {
    if thousands.is_empty() {
        return int_part.to_string();
    }
    let n = int_part.len();
    let mut out = String::with_capacity(n + (n / 3) * thousands.len());
    for (i, ch) in int_part.chars().enumerate() {
        if i > 0 && (n - i).is_multiple_of(3) {
            out.push_str(thousands);
        }
        out.push(ch);
    }
    out
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::datatypes::{Field, Schema};
    use arrow_ipc::writer::FileWriter;
    use std::sync::Arc;

    fn req(offset: u64, limit: Option<u64>, max_bytes: u64) -> ViewRequest<'static> {
        ViewRequest {
            row_offset: offset,
            row_limit: limit,
            columns: None,
            max_bytes,
            render: None,
        }
    }

    /// The §1.2 grammar round-trips: escape → split → unescape returns the original cells
    /// verbatim (tab/newline/backslash torture, unicode, empty-as-value), and the null
    /// marker stays unambiguous against a literal `\N` cell. This is the parse-back the
    /// edit engine rides (data-edit-design §2: "the edit path parses the same display
    /// string back, exactly like legacy").
    #[test]
    fn grammar_escape_unescape_round_trips() {
        let cells: Vec<String> = vec![
            "plain".into(),
            "".into(), // empty string IS a value
            "has<TAB>\ttab".into(),
            "line1\nline2\r\n".into(),
            "back\\slash".into(),
            "\\N literal".into(), // encodes as \\N — a value, not null
            "héllo 世界 🚀".into(),
            "a\tb\nc\rd\\e".into(),
        ];
        let mut row: Vec<u8> = Vec::new();
        for (i, cell) in cells.iter().enumerate() {
            if i > 0 {
                row.push(b'\t');
            }
            escape_into(cell, &mut row);
        }
        row.push(b'\n');
        // The byte-uniqueness property (§1.2): splitting is a raw TAB scan — every part
        // belongs to exactly one cell because no CELL contains a real TAB/LF.
        let row_bytes = &row[..row.len() - 1];
        let parts = split_row(row_bytes);
        assert_eq!(parts.len(), cells.len(), "rectangular: one cell per column");
        for (part, orig) in parts.iter().zip(cells.iter()) {
            let (text, is_null) = unescape_cell(part);
            assert!(!is_null);
            assert_eq!(&text, orig, "round-trip must be verbatim");
        }

        // The null marker vs its literal twin.
        let (text, null) = unescape_cell(b"\\N");
        assert!(null);
        assert!(text.is_empty());
        let (text, null) = unescape_cell(b"\\\\N");
        assert!(!null, "\\\\N is the literal string \\N — a value");
        assert_eq!(text, "\\N");
    }

    /// Write a Feather mirroring the lane output contract: score (scale Float64),
    /// group (nominal dict), note (nominal dict) — the format doc §1.4 schema. Values:
    /// row 0: 1234567.891 / A / "hello"
    /// row 1: null        / B / "has<TAB>tab and \N literal"
    /// row 2: 0.0000002   / A / "line1<LF>line2"
    fn write_vector_feather(path: &std::path::Path) {
        let field = |name: &str, dt: DataType| {
            Field::new(name, dt, true).with_metadata(
                [("jasp:display_name".to_string(), name.to_string())]
                    .into_iter()
                    .collect(),
            )
        };
        let dict_dt = DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8));
        let schema = Arc::new(Schema::new(vec![
            field("score", DataType::Float64),
            field("group", dict_dt.clone()),
            field("note", dict_dt),
        ]));
        let score = Float64Array::from(vec![Some(1234567.891), None, Some(0.0000002)]);
        let group: DictionaryArray<Int32Type> =
            vec![Some("A"), Some("B"), Some("A")].into_iter().collect();
        let note: DictionaryArray<Int32Type> = vec![
            Some("hello"),
            Some("has\ttab and \\N literal"),
            Some("line1\nline2"),
        ]
        .into_iter()
        .collect();
        let batch = RecordBatch::try_new(
            schema.clone(),
            vec![Arc::new(score), Arc::new(group), Arc::new(note)],
        )
        .unwrap();
        let file = File::create(path).unwrap();
        let mut w = FileWriter::try_new(file, &schema).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
    }

    fn tmp(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "jasp-arrowview-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    // The format doc §1.4 test vector, byte-exact (pins the grammar).
    #[test]
    fn test_vector_nl_locale() {
        let path = tmp("vector");
        write_vector_feather(&path);
        let render = messages::ViewRender {
            decimal: ",".into(),
            thousands: ".".into(),
            precision: 10,
        };
        let out = serve(
            path.to_str().unwrap(),
            &ViewRequest {
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: 1_000_000,
                render: Some(&render),
            },
        )
        .unwrap();
        let expected = "1.234.567,891\tA\thello\n\\N\tB\thas\\ttab and \\\\N literal\n2e-07\tA\tline1\\nline2\n";
        assert_eq!(out.tsv, expected.as_bytes(), "grammar byte-exact");
        assert_eq!(out.rows_total, 3);
        assert_eq!(out.row_offset, 0);
        assert_eq!(out.row_count, 3);
        assert!(!out.truncated);
        let _ = std::fs::remove_file(&path);
    }

    // Oversized-row vector (format doc §1.4): max_bytes 10 < row 0 (21 bytes), but the
    // progress guarantee emits it — row_count 1, truncated, continues at row_offset 1.
    #[test]
    fn oversized_chunk_progress_guarantee() {
        let path = tmp("oversize");
        write_vector_feather(&path);
        let render = messages::ViewRender {
            decimal: ",".into(),
            thousands: ".".into(),
            precision: 10,
        };
        let out = serve(
            path.to_str().unwrap(),
            &ViewRequest {
                row_offset: 0,
                row_limit: None,
                columns: None,
                max_bytes: 10,
                render: Some(&render),
            },
        )
        .unwrap();
        assert_eq!(out.row_count, 1);
        assert!(out.truncated);
        assert_eq!(out.tsv, b"1.234.567,891\tA\thello\n");
        // The next request continues at row_offset 1.
        let out2 = serve(
            path.to_str().unwrap(),
            &ViewRequest {
                row_offset: 1,
                row_limit: None,
                columns: None,
                max_bytes: 1_000_000,
                render: None,
            },
        )
        .unwrap();
        assert_eq!(out2.row_offset, 1);
        assert_eq!(out2.row_count, 2);
        assert!(!out2.truncated);
        let _ = std::fs::remove_file(&path);
    }

    // Budget stop lands on a row boundary: whole rows, ≤ max_bytes, truncated set.
    #[test]
    fn budget_stops_at_row_boundary() {
        let path = tmp("boundary");
        write_vector_feather(&path);
        // Row 0 (default render) is "1234567.891\tA\thello\n" = 21 bytes; row 1 is
        // "\\N\tB\thas\\ttab and \\\\N literal\n" = 34 bytes; row 2 = 22 bytes.
        // Budget 55: rows 0+1 fit (55), row 2 would exceed → stop at the boundary.
        let out = serve(path.to_str().unwrap(), &req(0, None, 55)).unwrap();
        assert_eq!(out.row_count, 2);
        assert!(out.truncated);
        assert!(out.tsv.len() <= 55);
        assert!(out.tsv.ends_with(b"\n"), "whole rows only");
        let _ = std::fs::remove_file(&path);
    }

    // row_offset / row_limit slicing; offset at the end → empty chunk.
    #[test]
    fn window_slicing() {
        let path = tmp("slice");
        write_vector_feather(&path);
        let out = serve(path.to_str().unwrap(), &req(1, Some(1), 1_000_000)).unwrap();
        assert_eq!(out.row_count, 1);
        assert_eq!(out.row_offset, 1);
        assert!(!out.truncated, "row_limit reached is not truncation");
        assert!(out.tsv.starts_with(b"\\N\tB\t"));
        // At the end: zero bytes, row_count 0.
        let out = serve(path.to_str().unwrap(), &req(3, None, 1_000_000)).unwrap();
        assert_eq!(out.row_count, 0);
        assert!(out.tsv.is_empty());
        assert!(!out.truncated);
        let _ = std::fs::remove_file(&path);
    }

    // Column selection resolves DISPLAY names; unknown columns fail visibly.
    #[test]
    fn column_selection_by_display_name() {
        let path = tmp("cols");
        write_vector_feather(&path);
        let cols = vec!["note".to_string(), "score".to_string()];
        let out = serve(
            path.to_str().unwrap(),
            &ViewRequest {
                row_offset: 0,
                row_limit: Some(1),
                columns: Some(&cols),
                max_bytes: 1_000_000,
                render: None,
            },
        )
        .unwrap();
        assert_eq!(out.tsv, b"hello\t1234567.891\n", "request order");
        let bad = vec!["nope".to_string()];
        let err = serve(
            path.to_str().unwrap(),
            &ViewRequest {
                row_offset: 0,
                row_limit: None,
                columns: Some(&bad),
                max_bytes: 1_000_000,
                render: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("unknown column 'nope'"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn missing_and_corrupt_cache_fail_visibly() {
        let err = serve("/nonexistent/no-such.arrow", &req(0, None, 1000)).unwrap_err();
        assert!(err.contains("cannot open cache file"), "{err}");
        let path = tmp("corrupt");
        std::fs::write(&path, b"not a feather at all").unwrap();
        let err = serve(path.to_str().unwrap(), &req(0, None, 1000)).unwrap_err();
        assert!(err.contains("corrupt cache file"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    // A single row above the wire ceiling is a visible failure, not an oversized emission
    // (test seam: serve_inner with a small ceiling).
    #[test]
    fn row_over_the_ceiling_is_fatal() {
        let path = tmp("ceiling");
        write_vector_feather(&path);
        let err = serve_inner(path.to_str().unwrap(), &req(0, None, 1_000_000), 10).unwrap_err();
        assert!(err.contains("row 0"), "{err}");
        assert!(err.contains("too large to transport"), "{err}");
        let _ = std::fs::remove_file(&path);
    }

    // Multi-block files: the footer row counts and block seek must agree with the decode.
    #[test]
    fn multi_block_serving() {
        let path = tmp("blocks");
        let schema = Arc::new(Schema::new(vec![Field::new("v", DataType::Float64, true)]));
        let file = File::create(&path).unwrap();
        let mut w = FileWriter::try_new(file, &schema).unwrap();
        for b in 0..3u64 {
            let vals: Vec<f64> = (0..4).map(|i| (b * 4 + i) as f64).collect();
            let batch =
                RecordBatch::try_new(schema.clone(), vec![Arc::new(Float64Array::from(vals))])
                    .unwrap();
            w.write(&batch).unwrap();
        }
        w.finish().unwrap();
        // A window crossing block boundaries, starting inside block 1.
        let out = serve(path.to_str().unwrap(), &req(5, Some(4), 1_000_000)).unwrap();
        assert_eq!(out.rows_total, 12);
        assert_eq!(out.row_count, 4);
        assert_eq!(out.tsv, b"5\n6\n7\n8\n");
        let _ = std::fs::remove_file(&path);
    }

    // ── 'g' rendering: legacy parity (QLocale::toString(dbl, 'g', 10)) ──
    #[test]
    fn render_double_g_parity() {
        let r = |x: f64| render_double(x, ".", "", 10);
        assert_eq!(r(1234567.891), "1234567.891"); // exponent 6 < 10 → plain
        assert_eq!(r(0.0000002), "2e-07"); // exponent −7 < −4 → exponent form
        assert_eq!(r(42.0), "42"); // integral: no decimal point
        assert_eq!(r(-42.0), "-42");
        assert_eq!(r(1.5), "1.5");
        assert_eq!(r(0.0001), "0.0001"); // exponent −4: still plain
        assert_eq!(r(0.00001), "1e-05"); // exponent −5: exponent form
        assert_eq!(r(9999999999.0), "9999999999"); // 10 sig digits, plain (exp 9 < 10)
        assert_eq!(r(99999999999.0), "1e+11"); // rounds up; exp 11 ≥ 10 → exponent form
        assert_eq!(r(999999999999999.0), "1e+15"); // carry all the way up
        assert_eq!(r(123456789.0), "123456789"); // 9 sig digits, plain
        assert_eq!(r(1e12), "1e+12"); // magnitude edge (all_integer parity)
        assert_eq!(r(0.0), "0");
        assert_eq!(r(-0.0), "-0");
        assert_eq!(r(2.75e-5), "2.75e-05");
        // Grouping: plain form only, integer part only.
        assert_eq!(render_double(1234567.891, ",", ".", 10), "1.234.567,891");
        assert_eq!(render_double(1e12, ",", ".", 10), "1e+12"); // no grouping in exponent form
        assert_eq!(render_double(1234.5, ",", ".", 10), "1.234,5");
        assert_eq!(render_double(-1234567.0, ",", ".", 10), "-1.234.567");
        // Precision honored: 'g' at 3 sig digits.
        assert_eq!(render_double(1234567.891, ".", "", 3), "1.23e+06");
        assert_eq!(render_double(1.2345, ".", "", 3), "1.23");
    }

    // Escape grammar round-trip: the only escaped bytes, `\N`-vs-`\\N` unambiguity.
    #[test]
    fn escape_grammar() {
        let esc = |s: &str| {
            let mut v = Vec::new();
            escape_into(s, &mut v);
            String::from_utf8(v).unwrap()
        };
        assert_eq!(esc("plain"), "plain");
        assert_eq!(esc("has\ttab"), "has\\ttab");
        assert_eq!(esc("line1\nline2"), "line1\\nline2");
        assert_eq!(esc("cr\rhere"), "cr\\rhere");
        assert_eq!(esc("back\\slash"), "back\\\\slash");
        assert_eq!(esc("\\N"), "\\\\N", "literal \\N is not null");
        assert_eq!(esc(""), "", "empty is a value, distinct from null");
        assert_eq!(esc("국어 점수"), "국어 점수", "CJK passes through UTF-8");
        // No raw separator survives → byte-unambiguous splitting.
        let cell = "a\tb\nc\\d\re";
        assert!(!esc(cell).contains(['\t', '\n', '\r']));
    }
}
