//! CSV → canonical Arrow Feather lane (production path).
//!
//! A focused port of the `csv2arrow` PoC's *production* pipeline
//! (`refactor_design/poc/csv_arrow/src/main.rs`, `--infer row --build stream`), which is the
//! reference implementation measured in `HANDOVER-csvlane.md`. The A/B-only variants
//! (`--infer col`, `--build concat`, `--build queue`) are deliberately not ported — they exist
//! only to benchmark the production choices (§8 of that spec).
//!
//! Pipeline (see `HANDOVER-csvlane.md`):
//!   ① row-wise multithreaded inference — a producer streams row-aligned byte chunks, N workers
//!     each parse a whole chunk and observe into a per-chunk `Vec<ColStats>`, a merger folds them
//!     **in chunk-id order** (global first-appearance dictionary order — load-bearing).
//!   ② dictionaries — pre-build one shared dictionary per categorical, CONSUMING each column's
//!     inference `IndexSet` (move, no clone), then `drop(stats)` before the build loop.
//!   ③ build — sequential streaming, byte-budgeted batch with a 1M-row cap, encode each column
//!     against the shared dictionaries, write incrementally to the LZ4 Feather writer.
//!
//! Output contract (neo-jasp §8.3, `HANDOVER-csvlane.md` §4): scale → `Float64` (+
//! `jasp:all_integer`); categorical → `Dictionary(Int32, Utf8)` with the ordered flag carrying
//! ordinal-vs-nominal; field metadata `jasp:display_name` / `jasp:auto_sort_by_value` /
//! `jasp:all_integer`; Feather V2 + LZ4_FRAME.

use indexmap::IndexSet;
use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::sync::Arc;

use arrow::array::{
    Array, ArrayRef, AsArray, DictionaryArray, Float64Builder, Int32Builder, RecordBatch,
    StringArray,
};
use arrow::compute::cast;
use arrow::datatypes::{DataType, Field, Int32Type, Schema, SchemaRef};
use arrow_ipc::CompressionType;
use arrow_ipc::writer::{FileWriter, IpcWriteOptions};
use regex::Regex;

use crate::messages::IngestParams;

// ── Tunables (production defaults; see `HANDOVER-csvlane.md` §5) ─────────────

/// Inference/build threads.
const THREADS: usize = 4;
/// Reader batch ceiling / fallback (rows).
const BATCH: usize = 65536;
/// Stream output batch byte budget (MiB).
const BATCH_MB: usize = 64;
/// Target live-cell budget (rows × cols) for any per-batch arrow reader. Sizing by cells
/// (not a fixed row count) bounds per-batch builder pre-allocation regardless of shape:
/// `batch = clamp(CELL_BUDGET / ncols, 1, BATCH)`.
const CELL_BUDGET: usize = 1 << 23;
/// Upper row cap for a streaming-build output batch (guards the narrow-data degenerate case
/// where the byte budget would balloon to a multi-million-row batch).
const MAX_BUILD_ROWS: usize = 1_000_000;
/// Numeric distinct-tracking ceiling feeding the schema's `distinct_count` (data-model-design.md
/// §2). Form constraints only ever compare distinct counts against thresholds ≤ ~256 (module
/// `maxLevels`) / the "maximum levels for scale" preference — so 1k is exact-in-practice and
/// bounded in memory; anything ever needing more gets HyperLogLog estimates, never unbounded
/// sets. The inference cap is `max(threshold, DISTINCT_CAP) + 1`, so the scale/ordinal decision
/// (which needs exactness ≤ threshold) stays correct for any threshold.
pub(crate) const DISTINCT_CAP: usize = 1_000;
/// Wire cap on the per-column `levels` array (data-model-design.md §2, decision 15): beyond
/// this, levels are UI noise (nobody edits/enumerates 10k+ labels — the analysis itself reads
/// the full dictionary from the Arrow cache in the runner). The schema ships the first
/// WIRE_LEVELS_CAP entries in dictionary order and `distinct_count` (always exact for
/// categoricals) stays the single source of truth for counts. The Arrow dictionary itself is
/// NEVER truncated — every value needs its encoding entry.
pub(crate) const WIRE_LEVELS_CAP: usize = 10_000;

// ── Measurement-level inference (faithful port of column.cpp:setValues) ──────

#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Level {
    Scale,
    Ordinal,
    Nominal,
}

impl Level {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Level::Scale => "scale",
            Level::Ordinal => "ordinal",
            Level::Nominal => "nominal",
        }
    }
}

#[derive(Default)]
pub(crate) struct ColStats {
    pub(crate) only_ints: bool,
    pub(crate) only_doubles: bool,
    /// Distinct values in first-appearance order (IndexSet: ordered + O(1) dedup, one copy of
    /// each string). Numeric columns cap this at `max(threshold, DISTINCT_CAP) + 1`: exactness
    /// up to `threshold` drives the scale/ordinal decision; the headroom to DISTINCT_CAP feeds
    /// the schema's `distinct_count` (data-model-design.md §2). Text columns keep it in full —
    /// it *is* the categorical's dictionary.
    pub(crate) distinct: IndexSet<String>,
    pub(crate) capped: bool,
    pub(crate) count: usize,
}

impl ColStats {
    pub(crate) fn new() -> Self {
        ColStats {
            only_ints: true,
            only_doubles: true,
            ..Default::default()
        }
    }

    pub(crate) fn observe(&mut self, v: &str, cap: usize, locale: Locale) {
        self.count += 1;
        let (is_int, is_dbl) = if !locale.active {
            // Fast path: strict Rust parses.
            (v.parse::<i64>().is_ok(), v.parse::<f64>().is_ok())
        } else {
            match locale.parse_num(v) {
                Some(f) => (f.is_finite() && f.fract() == 0.0, true),
                None => (false, false),
            }
        };
        if !is_int {
            self.only_ints = false;
        }
        if !is_dbl {
            self.only_doubles = false;
        }
        // Distinct is counted on the *raw* string (correct for single-locale files; real data
        // is single-locale — see HANDOVER-csvlane §4).
        if self.only_doubles {
            if !self.capped {
                self.distinct.insert(v.to_string());
                if self.distinct.len() > cap {
                    self.capped = true;
                }
            }
        } else {
            self.distinct.insert(v.to_string());
        }
    }

    /// Fold another chunk's stats into this accumulator (associative). Inserting in chunk-id
    /// order yields global first-appearance order.
    fn merge_from(&mut self, other: ColStats, cap: usize) {
        self.count += other.count;
        self.only_ints &= other.only_ints;
        self.only_doubles &= other.only_doubles;
        if self.only_doubles {
            if !self.capped {
                for v in other.distinct {
                    self.distinct.insert(v);
                    if self.distinct.len() > cap {
                        self.capped = true;
                        break;
                    }
                }
            }
        } else {
            self.distinct.extend(other.distinct);
        }
    }
}

/// The "threshold for scale" rule, transcribed from JASP `Column::setValues`: integer columns
/// with ≤ threshold distinct are ordinal (2 → nominal); all-double columns are scale; text
/// columns are nominal when ≤2 or > threshold distinct, else ordinal.
pub(crate) fn decide(s: &ColStats, threshold: usize) -> Level {
    let n = s.distinct.len();
    if s.count > 0 && s.only_ints && n > 0 && n <= threshold {
        return if n == 2 {
            Level::Nominal
        } else {
            Level::Ordinal
        };
    }
    if s.count > 0 && s.only_doubles {
        return Level::Scale;
    }
    if n <= 2 || n > threshold {
        Level::Nominal
    } else {
        Level::Ordinal
    }
}

// ── Locale-aware number parsing (opt-in "slower" branch) ─────────────────────

/// Locale separators for numeric parsing. When `active` is false ('.' decimal, no thousands)
/// the fast strict-parse path is used.
#[derive(Clone, Copy)]
pub(crate) struct Locale {
    pub(crate) dec: char,
    pub(crate) thou: Option<char>,
    pub(crate) active: bool,
}

impl Locale {
    pub(crate) fn new(dec: char, thou: Option<char>) -> Self {
        Locale {
            dec,
            thou,
            active: dec != '.' || thou.is_some(),
        }
    }

    fn is_num_char(self, c: char) -> bool {
        c.is_ascii_digit() || c == self.dec || c == '+' || c == '-' || Some(c) == self.thou
    }

    /// Rewrite a locale-formatted number into canonical form. Trims surrounding whitespace and
    /// requires the *whole* trimmed cell to be numeric (so a text level like `"L0"` is never
    /// misread as 0); drops thousands separators, maps the decimal sep → '.'. `None` = text.
    pub(crate) fn canonicalize(self, s: &str) -> Option<String> {
        let t = s.trim();
        if t.is_empty() {
            return None;
        }
        let mut out = String::with_capacity(t.len());
        for c in t.chars() {
            if !self.is_num_char(c) {
                return None;
            }
            if Some(c) == self.thou {
                continue;
            }
            out.push(if c == self.dec { '.' } else { c });
        }
        Some(out)
    }

    pub(crate) fn parse_num(self, s: &str) -> Option<f64> {
        self.canonicalize(s)?.parse::<f64>().ok()
    }
}

// ── Delimiter auto-detect + csv reader construction ──────────────────────────

fn sniff_delimiter(content: &str) -> u8 {
    let sample: String = content.lines().take(20).collect::<Vec<_>>().join("\n");
    let mut best: Option<(u8, usize, usize)> = None; // (delim, min_cols, consistent_rows)
    for &d in b",;\t|" {
        let mut rdr = csv::ReaderBuilder::new()
            .delimiter(d)
            .has_headers(false)
            .flexible(true)
            .from_reader(sample.as_bytes());
        let mut counts: Vec<usize> = Vec::new();
        for rec in rdr.records().take(10).flatten() {
            counts.push(rec.len());
        }
        if counts.is_empty() {
            continue;
        }
        let min = *counts.iter().min().unwrap();
        let consistent = counts.iter().filter(|&&c| c == counts[0]).count();
        let better = match best {
            None => true,
            Some((_, bmin, bcons)) => min > bmin || (min == bmin && consistent > bcons),
        };
        if better {
            best = Some((d, min, consistent));
        }
    }
    best.map(|(d, _, _)| d).unwrap_or(b',')
}

fn utf8_schema(names: &[String]) -> SchemaRef {
    Arc::new(Schema::new(
        names
            .iter()
            .map(|n| Field::new(n, DataType::Utf8, true))
            .collect::<Vec<_>>(),
    ))
}

fn read_header_names(content: &str, delim: u8) -> Vec<String> {
    let mut rdr = csv::ReaderBuilder::new()
        .delimiter(delim)
        .has_headers(true)
        .flexible(true)
        .from_reader(content.as_bytes());
    rdr.headers()
        .cloned()
        .map(|h| h.iter().map(|s| s.to_string()).collect())
        .unwrap_or_default()
}

/// JASP's CSV column-naming convention, copied from `Desktop/data/importers/csvimporter.cpp`
/// (the GUI's importer) so the converted cache carries exactly the names JASP would assign:
/// 1. empty header → `V<1-based column position>` (the row-index column of JASP CSVs);
/// 2. pure-integer header ("3") → `V3` — an stoi round-trip check, so `1hahaha` and `007`
///    are NOT renamed;
/// 3. a duplicate (against the already-renamed earlier names) gets `_<1-based position>`
///    appended (single pass, as in the importer).
pub(crate) fn jasp_column_names(names: &mut [String]) {
    for col_no in 0..names.len() {
        let name = std::mem::take(&mut names[col_no]);
        let mut renamed = if name.is_empty() {
            format!("V{}", col_no + 1)
        } else if name.parse::<i64>().is_ok_and(|n| n.to_string() == name) {
            format!("V{name}")
        } else {
            name
        };
        if col_no > 0 && names[..col_no].contains(&renamed) {
            renamed = format!("{renamed}_{}", col_no + 1);
        }
        names[col_no] = renamed;
    }
}

fn csv_reader<R: Read>(
    schema: SchemaRef,
    reader: R,
    delim: u8,
    batch: usize,
    null_regex: Regex,
) -> Result<arrow::csv::Reader<R>, arrow::error::ArrowError> {
    arrow::csv::reader::ReaderBuilder::new(schema)
        .with_header(true)
        .with_delimiter(delim)
        .with_batch_size(batch)
        .with_null_regex(null_regex)
        .build(reader)
}

/// Read a small prefix of the file (delimiter sniffing + header names) so the whole file is
/// never held in memory.
fn read_head(path: &str, max_bytes: usize) -> Result<String, std::io::Error> {
    let mut f = File::open(path)?;
    let mut buf = vec![0u8; max_bytes];
    let n = f.read(&mut buf)?;
    buf.truncate(n);
    Ok(String::from_utf8_lossy(&buf).into_owned())
}

/// Build the null-matching regex from the null spellings (empty string included so blank cells
/// stay null). Arrow only treats *matching* cells as null, so every spelling is enumerated.
fn build_null_regex(nulls: &[String]) -> Result<Regex, regex::Error> {
    let pat = format!(
        "^(?:{})$",
        nulls
            .iter()
            .map(|n| regex::escape(n))
            .collect::<Vec<_>>()
            .join("|")
    );
    Regex::new(&pat)
}

// ── Inference chunk queue (row-wise, multithreaded) ──────────────────────────

/// Adaptive inference-queue chunk target from the column count: `clamp(ncols×16KiB, 4, 8)MiB`.
fn auto_chunk_target(ncols: usize) -> usize {
    const BYTES_PER_COL: usize = 16 << 10;
    const MIN_CHUNK: usize = 1 << 22;
    const MAX_CHUNK: usize = 1 << 23;
    ncols
        .saturating_mul(BYTES_PER_COL)
        .clamp(MIN_CHUNK, MAX_CHUNK)
}

/// Stream the file's *data* rows (header skipped) as raw byte chunks, each ending on a row
/// boundary and tagged with a chunk-id assigned in read order (sorting by id restores dataset
/// order). Bounded channel ⇒ backpressure, so the file is never fully buffered.
fn produce_chunks(
    path: &str,
    chunk_target: usize,
    tx: &std::sync::mpsc::SyncSender<(usize, Vec<u8>)>,
) -> std::io::Result<usize> {
    let mut reader = BufReader::new(File::open(path)?);
    let mut header = String::new();
    reader.read_line(&mut header)?; // skip the header row
    let mut chunk_id = 0usize;
    let mut buf: Vec<u8> = Vec::with_capacity(chunk_target + (1 << 16));
    let mut scratch = [0u8; 1 << 16];
    loop {
        let n = reader.read(&mut scratch)?;
        if n == 0 {
            if !buf.is_empty() {
                if tx.send((chunk_id, std::mem::take(&mut buf))).is_err() {
                    return Ok(chunk_id);
                }
                chunk_id += 1;
            }
            break;
        }
        buf.extend_from_slice(&scratch[..n]);
        if buf.len() >= chunk_target
            && let Some(pos) = buf.iter().rposition(|&b| b == b'\n')
        {
            let chunk: Vec<u8> = buf.drain(..=pos).collect();
            if tx.send((chunk_id, chunk)).is_err() {
                return Ok(chunk_id);
            }
            chunk_id += 1;
        }
    }
    Ok(chunk_id)
}

/// Fold every consecutive chunk present in `pending` starting at `*next_id` into `stats` (in
/// chunk-id order), advancing `*next_id` and freeing each chunk's stats as it merges. In-order
/// merge is what keeps the IndexSet distinct sets in global first-appearance order.
fn drain_pending(
    pending: &mut HashMap<usize, (Vec<ColStats>, usize)>,
    stats: &mut [ColStats],
    nrows: &mut usize,
    next_id: &mut usize,
    cap: usize,
) {
    while let Some((cs, rows)) = pending.remove(next_id) {
        *nrows += rows;
        for (i, c) in cs.into_iter().enumerate() {
            stats[i].merge_from(c, cap);
        }
        *next_id += 1;
    }
}

// ── Dictionaries (consume-and-move, then drop inference stats) ───────────────

/// A pre-built categorical dictionary, shared (same `Arc`) across every build batch so the IPC
/// writer emits the dictionary once per field instead of rejecting a "dictionary replacement".
pub(crate) struct CatDict {
    pub(crate) values: ArrayRef,
    /// dictionary value → index, for encoding each batch's cells.
    pub(crate) index: HashMap<String, i32>,
    /// When true, canonicalize each raw cell (locale numeric categorical) before lookup.
    pub(crate) canonicalize: bool,
}

/// Pre-build the shared dictionary for one categorical column from its merged distinct set,
/// taken BY VALUE so its owned strings are MOVED into the dictionary (no clone). For a
/// locale-active numeric categorical the dictionary holds *canonical* values. Value-sorts when
/// the distinct set is ≤ `sort_limit` (JASP `labelsOrderByValue`); above that the sort is
/// skipped (meaningless for near-unique columns) but the dictionary is always built.
pub(crate) fn prebuild_dict(
    distinct: IndexSet<String>,
    canonicalize: bool,
    locale: Locale,
    sort_limit: usize,
) -> CatDict {
    let mut values: Vec<String> = Vec::with_capacity(distinct.len());
    let mut index: HashMap<String, i32> = HashMap::with_capacity(distinct.len());
    for raw in distinct {
        let v = if canonicalize {
            locale.canonicalize(&raw).unwrap_or(raw)
        } else {
            raw
        };
        if index.contains_key(&v) {
            continue; // dedup guard (mixed-locale only)
        }
        let ix = values.len() as i32;
        index.insert(v.clone(), ix); // the one remaining clone (index key)
        values.push(v);
    }
    if values.len() <= sort_limit {
        values.sort_by(|a, b| match (a.parse::<f64>(), b.parse::<f64>()) {
            (Ok(x), Ok(y)) => x.total_cmp(&y),
            (Ok(_), Err(_)) => std::cmp::Ordering::Less,
            (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
            (Err(_), Err(_)) => a.cmp(b),
        });
        index.clear();
        for (ix, v) in values.iter().enumerate() {
            index.insert(v.clone(), ix as i32);
        }
    }
    CatDict {
        values: Arc::new(StringArray::from(values)),
        index,
        canonicalize,
    }
}

// ── Column encoding ──────────────────────────────────────────────────────────

/// Scale column, locale path: parse each cell gently into Float64 (arrow's `cast` only accepts
/// '.' decimals and no thousands).
fn build_float64_locale(col: &ArrayRef, locale: Locale) -> ArrayRef {
    let sa = col.as_string::<i32>();
    let mut b = Float64Builder::with_capacity(sa.len());
    for v in sa.iter() {
        match v.and_then(|s| locale.parse_num(s)) {
            Some(f) => b.append_value(f),
            None => b.append_null(),
        }
    }
    Arc::new(b.finish())
}

/// Encode one batch's string column against a pre-built shared dictionary. Null stays null; a
/// value missing from the dictionary (shouldn't happen) falls back to null.
fn build_dict_col(col: &ArrayRef, cd: &CatDict, locale: Locale) -> ArrayRef {
    let sa = col.as_string::<i32>();
    let mut keys = Int32Builder::with_capacity(sa.len());
    if cd.canonicalize {
        for v in sa.iter() {
            match v.and_then(|s| {
                let key = locale.canonicalize(s).unwrap_or_else(|| s.to_string());
                cd.index.get(&key).copied()
            }) {
                Some(ix) => keys.append_value(ix),
                None => keys.append_null(),
            }
        }
    } else {
        for v in sa.iter() {
            match v.and_then(|s| cd.index.get(s).copied()) {
                Some(ix) => keys.append_value(ix),
                None => keys.append_null(),
            }
        }
    }
    let dict = DictionaryArray::<Int32Type>::try_new(keys.finish(), Arc::clone(&cd.values))
        .expect("dictionary indices out of range");
    Arc::new(dict)
}

/// Convert one already-read string column to its final Arrow type. Scale → Float64 (vectorised
/// cast, or locale-aware parse for comma-decimals); categorical → encoded against the pre-built
/// shared dictionary.
fn encode_column(
    col: &ArrayRef,
    level: Level,
    cat_dict: Option<&CatDict>,
    locale: Locale,
    target: &DataType,
) -> Result<ArrayRef, arrow::error::ArrowError> {
    Ok(match level {
        Level::Scale => {
            if !locale.active {
                cast(col, target)?
            } else {
                build_float64_locale(col, locale)
            }
        }
        Level::Ordinal | Level::Nominal => build_dict_col(
            col,
            cat_dict.expect("categorical column missing its dictionary"),
            locale,
        ),
    })
}

/// Cast one string batch to the final typed columns, in parallel across columns (each thread
/// owns a contiguous column range; results reassembled in order). One batch at a time.
fn cast_batch_stream(
    sb: &RecordBatch,
    levels: &[Level],
    cat_dicts: &[Option<CatDict>],
    locale: Locale,
    schema: &SchemaRef,
) -> Result<Vec<ArrayRef>, arrow::error::ArrowError> {
    let ncols = levels.len();
    let cols_in: &[ArrayRef] = sb.columns();
    let chunk = ncols.div_ceil(THREADS);
    let ranges: Vec<(usize, usize)> = (0..ncols)
        .step_by(chunk)
        .map(|start| (start, (start + chunk).min(ncols)))
        .collect();
    let schema_c = Arc::clone(schema);
    std::thread::scope(|s| -> Result<Vec<ArrayRef>, arrow::error::ArrowError> {
        let handles: Vec<_> = ranges
            .iter()
            .map(|&(start, end)| {
                let schema = Arc::clone(&schema_c);
                s.spawn(move || -> Result<Vec<ArrayRef>, arrow::error::ArrowError> {
                    let mut out = Vec::with_capacity(end - start);
                    for i in start..end {
                        out.push(encode_column(
                            &cols_in[i],
                            levels[i],
                            cat_dicts[i].as_ref(),
                            locale,
                            schema.field(i).data_type(),
                        )?);
                    }
                    Ok(out)
                })
            })
            .collect();
        let mut cols = Vec::with_capacity(ncols);
        for h in handles {
            cols.extend(h.join().expect("build thread panicked")?);
        }
        Ok(cols)
    })
}

// ── Output schema & writer (neo-jasp §8.3) ───────────────────────────────────

/// Extend a categorical dictionary with new values in first-appearance order — absorption
/// (data-edit-design §4 rule 1): unseen values append at the END (ordinal appends at end
/// only; nominal first-appearance — for a single edit's incoming cells these coincide), seen
/// values keep their index so existing keys stay valid. Returns a fresh shared dictionary
/// for the rebuilt column's batches.
pub(crate) fn append_levels(base: &CatDict, extra: impl Iterator<Item = String>) -> CatDict {
    let mut values: Vec<String> = base
        .values
        .as_string::<i32>()
        .iter()
        .flatten()
        .map(|s| s.to_string())
        .collect();
    let mut index: HashMap<String, i32> = base.index.clone();
    for v in extra {
        if !index.contains_key(&v) {
            index.insert(v.clone(), values.len() as i32);
            values.push(v);
        }
    }
    CatDict {
        values: Arc::new(StringArray::from(values)),
        index,
        canonicalize: base.canonicalize,
    }
}

/// A dictionary from a DECLARED level list, verbatim in list order — no value-sort, no
/// canonicalization: declared order is intent (an ordinal's level order IS its meaning;
/// the label editor's list is what the user arranged). Callers validate duplicates before
/// here — a declared list is authoritative, not inferred. `insert_cols` specs (d5) and
/// declared `target_schema` levels (d6) both come here; inference never does.
pub(crate) fn dict_from_list(levels: &[String]) -> CatDict {
    let mut index: HashMap<String, i32> = HashMap::with_capacity(levels.len());
    for (i, v) in levels.iter().enumerate() {
        index.insert(v.clone(), i as i32);
    }
    CatDict {
        values: Arc::new(StringArray::from(levels.to_vec())),
        index,
        canonicalize: false,
    }
}

/// The single-new-column form of the [`jasp_column_names`] convention (same rules, one name
/// joining an existing list): empty → `V<position>`, pure-integer → `V<name>`, a duplicate
/// against the existing names → `_<position>` appended. `position` is the new column's
/// 1-based position. The suffix LOOPS until genuinely free — the first candidate can
/// itself be taken (an earlier insert already claimed `score_3`, columns shifted…), and a
/// name generator that can emit a duplicate is a bug, not a convention. One convention,
/// no copies — paste-overflow naming (§3) and `insert_cols` specs both come here.
pub(crate) fn unique_new_name(existing: &[String], base: &str, position: usize) -> String {
    let mut renamed = if base.is_empty() {
        format!("V{position}")
    } else if base.parse::<i64>().is_ok_and(|n| n.to_string() == base) {
        format!("V{base}")
    } else {
        base.to_string()
    };
    if existing.contains(&renamed) {
        let owned = renamed.clone();
        let mut n = position;
        while existing.contains(&renamed) {
            renamed = format!("{owned}_{n}");
            n += 1;
        }
    }
    renamed
}

/// One decorated output field (neo-jasp §8.3): scale → Float64 (+ `jasp:all_integer` when
/// hinted), categorical → Dictionary(Int32, Utf8) with the ordered flag carrying ordinal,
/// plus the `jasp:*` metadata. Extracted from `build_output_schema` so the edit engine's
/// rebuilt columns decorate identically — ONE rule, no copies.
pub(crate) fn jasp_field(name: &str, display_name: &str, level: Level, all_integer: bool) -> Field {
    let mut meta: Vec<(String, String)> =
        vec![("jasp:display_name".into(), display_name.to_string())];
    let f = match level {
        Level::Scale => {
            if all_integer {
                meta.push(("jasp:all_integer".into(), "true".into()));
            }
            Field::new(name, DataType::Float64, true)
        }
        Level::Ordinal | Level::Nominal => {
            meta.push(("jasp:auto_sort_by_value".into(), "true".into()));
            Field::new(
                name,
                DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                true,
            )
            .with_dict_is_ordered(level == Level::Ordinal)
        }
    };
    f.with_metadata(meta.into_iter().collect())
}

/// The inverse of [`jasp_field`]: read a cache column's measurement level back from its
/// Arrow encoding (output contract §8.3): Float64 → Scale; Dictionary(Int32, Utf8) with
/// the ordered flag → Ordinal, without → Nominal. `None` = not a v1 cache shape — the
/// edit engine refuses such columns visibly rather than guessing.
pub(crate) fn level_of_field(f: &Field) -> Option<Level> {
    match f.data_type() {
        DataType::Float64 => Some(Level::Scale),
        DataType::Dictionary(k, v)
            if matches!(**k, DataType::Int32) && matches!(**v, DataType::Utf8) =>
        {
            Some(if f.dict_is_ordered() == Some(true) {
                Level::Ordinal
            } else {
                Level::Nominal
            })
        }
        _ => None,
    }
}

/// Attach the `jasp:labels` overlay to a decorated field (neo-jasp.md §8: a SPARSE JSON
/// value→display-label map; an entry only where label ≠ value — identity labels are
/// ABSENT, which is why CSV-open caches never carry the key). One home beside
/// [`jasp_field`]: field decoration never lives elsewhere. An empty map is a detach
/// (relabelling back to identity removes the key — sparse).
pub(crate) fn attach_labels(f: &mut Field, labels: &serde_json::Value) {
    let mut meta = f.metadata().clone();
    let empty = labels.as_object().is_none_or(|m| m.is_empty());
    if empty {
        meta.remove("jasp:labels");
    } else {
        meta.insert(
            "jasp:labels".into(),
            serde_json::to_string(labels).unwrap_or_default(),
        );
    }
    f.set_metadata(meta);
}

/// Read a field's `jasp:labels` overlay back (present + non-empty), for the wire schema
/// and the edit engine's inverse capture.
pub(crate) fn labels_of_field(f: &Field) -> Option<serde_json::Value> {
    let raw = f.metadata().get("jasp:labels")?;
    let v: serde_json::Value = serde_json::from_str(raw).ok()?;
    (v.as_object().is_some_and(|m| !m.is_empty())).then_some(v)
}

/// Build the final decorated schema: scale→Float64, categorical→Dictionary<Int32,Utf8> with the
/// ordered flag carrying ordinal-vs-nominal, plus the jasp:* field metadata.
fn build_output_schema(names: &[String], levels: &[Level], stats: &[ColStats]) -> SchemaRef {
    let fields: Vec<Field> = names
        .iter()
        .enumerate()
        .map(|(i, name)| jasp_field(name, name, levels[i], stats[i].only_ints))
        .collect();
    Arc::new(Schema::new(fields))
}

/// The effective null spellings for an ingest (the conversion's fallback applies when the
/// caller left them empty) — shared by `convert` and the edit path's cell typing so the two
/// can never disagree about what a missing cell looks like.
pub(crate) fn null_spellings(ingest: &IngestParams) -> Vec<String> {
    if ingest.nulls.is_empty() {
        vec![String::new(), "NA".into(), "NaN".into()]
    } else {
        ingest.nulls.clone()
    }
}

/// Rebuild the shared-dictionary handle from a cache column's already-materialized values
/// (the edit path reads back the dictionary the conversion wrote). `canonicalize` is
/// RE-DERIVED from content, matching the build rule exactly: a locale-active column whose
/// every value parses as a number was canonicalized at conversion — and could not have
/// been built raw, since all-numeric content under an active locale canonicalizes — so the
/// round-trip is faithful without storing a flag in the file.
pub(crate) fn cat_dict_from_values(values: ArrayRef, locale: Locale) -> CatDict {
    let sa = values.as_string::<i32>();
    let mut index: HashMap<String, i32> = HashMap::with_capacity(sa.len());
    let mut all_numeric = true;
    for (i, v) in sa.iter().enumerate() {
        if let Some(v) = v {
            index.insert(v.to_string(), i as i32);
            if locale.active && locale.parse_num(v).is_none() {
                all_numeric = false;
            }
        }
    }
    CatDict {
        values,
        index,
        canonicalize: locale.active && all_numeric,
    }
}

/// A categorical dictionary's count of DISTINCT NUMERIC levels — the one numeric-levels
/// semantic, shared by the conversion's schema and the edit engine's post-edit schema.
pub(crate) fn numeric_levels_of(cd: &CatDict, locale: Locale) -> u64 {
    count_distinct_numbers(cd.values.as_string::<i32>().iter().flatten().map(|v| {
        if cd.canonicalize {
            v.parse::<f64>().ok() // canonical dictionary: plain '.' format
        } else {
            locale.parse_num(v) // raw dictionary: source-locale format
        }
    }))
}

/// One column's wire-schema JSON (the `kind:"data"` result shape, §19.2 / data-model-design
/// §2) — the SINGLE mapping shared by the open result and the edit engine's post-edit schema
/// so the two shapes can never drift.
pub(crate) fn column_info_json(c: &ColumnInfo) -> serde_json::Value {
    use serde_json::json;
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
    if let Some(labels) = &c.labels {
        col.insert("labels".into(), json!(labels));
    }
    // Constraint-check stats (data-model-design.md §2): value_count = non-empty cell count
    // (zero IS a value); distinct_count = distinct values, exact for categoricals and exact
    // up to ~1k for scale (the cap value means "at least that many").
    col.insert("value_count".into(), json!(c.value_count));
    col.insert("distinct_count".into(), json!(c.distinct_count));
    if let Some(nl) = c.numeric_levels {
        col.insert("numeric_levels".into(), json!(nl));
    }
    serde_json::Value::Object(col)
}

/// Open an in-memory LZ4 IPC writer — the inverse blob's encoding (data-edit-design §10:
/// the same codec stack as the caches; dictionary columns stay dictionary-typed).
pub(crate) fn make_ipc_writer<W: std::io::Write>(
    w: W,
    schema: &SchemaRef,
) -> Result<FileWriter<W>, Box<dyn std::error::Error>> {
    let opts = IpcWriteOptions::default().try_with_compression(Some(CompressionType::LZ4_FRAME))?;
    Ok(FileWriter::try_new_with_options(w, schema, opts)?)
}

/// Open the LZ4-compressed Feather (IPC file) writer.
pub(crate) fn make_feather_writer(
    out: &str,
    schema: &SchemaRef,
) -> Result<FileWriter<File>, Box<dyn std::error::Error>> {
    make_ipc_writer(File::create(out)?, schema)
}

// ── Result ────────────────────────────────────────────────────────────────────────────────

/// One column's contribution to the wire schema handed back on the kind:"data" result.
#[derive(Debug, Clone)]
pub struct ColumnInfo {
    pub name: String,
    pub display_name: String,
    /// `"scale"` | `"ordinal"` | `"nominal"`.
    pub level: &'static str,
    /// Scale columns only: every non-null value is integer-valued (`jasp:all_integer`).
    pub all_integer: bool,
    /// Categorical columns only: the dictionary's factor levels, capped at `WIRE_LEVELS_CAP`
    /// (a UI-enumeration prefix in dictionary order; `distinct_count` stays the exact truth).
    /// The Arrow dictionary itself is never truncated.
    pub levels: Option<Vec<String>>,
    /// Non-empty (non-missing) cell count observed during conversion — "empty" = blank or a
    /// null spelling; ZERO IS A VALUE and is counted (data-model-design.md §2).
    pub value_count: u64,
    /// Number of distinct values — categoricals: always exact (the dictionary size); scale:
    /// exact up to `DISTINCT_CAP`, above which the reported value means "at least this many"
    /// (data-model-design.md §2). Feeds the frontend's TotalLevels/TotalNumericValues so every
    /// form threshold (registry audit: ≤ 256, incl. the maxScaleLevels cast guard) is enforced
    /// without anyone counting distinct values in the frontend.
    pub distinct_count: u64,
    /// Categoricals only: the number of DISTINCT NUMERIC values among the levels (scale
    /// columns don't need it — every value is numeric by definition, `distinct_count`
    /// serves). Computed HERE, where the source format is known — locale-aware, deduped on
    /// the parsed number (legacy doubleset semantics: "1.5"/"1.50" count once), non-finite
    /// excluded. The frontend never parses wire values (data-model-design.md §2).
    pub numeric_levels: Option<u64>,
    /// Categoricals only: the `jasp:labels` overlay (neo-jasp.md §8) — a sparse JSON
    /// value→display-label map, present IFF the field carries a non-empty one. The label
    /// is what you READ (display, factor levels); the value is the data. Absent for every
    /// identity-labelled column (CSV-open caches) — sparse serialization.
    pub labels: Option<serde_json::Value>,
}

/// The lane's `{schema, rows}` reply payload.
#[derive(Debug, Clone)]
pub struct ConvertOutput {
    pub rows: u64,
    pub columns: Vec<ColumnInfo>,
}

// ── Entry point ──────────────────────────────────────────────────────────────

/// Count distinct numeric values, deduping on the PARSED number — legacy doubleset semantics:
/// "1.5"/"1.50" count once, ±0 unified, non-finite ("NaN"/"inf") excluded (data-model-design.md
/// §2). Feeds both the scale `distinct_count` and the categorical `numeric_levels`.
pub(crate) fn count_distinct_numbers(parsed: impl Iterator<Item = Option<f64>>) -> u64 {
    let mut seen: HashSet<u64> = HashSet::new();
    for f in parsed.flatten() {
        if f.is_finite() {
            seen.insert(if f == 0.0 { 0 } else { f.to_bits() });
        }
    }
    seen.len() as u64
}

/// Convert the CSV at `source` into a Feather file at `cache_path`, applying the ingestion
/// settings from `ingest`. Returns the row count and per-column schema info. Satisfies the lane
/// contract: **path in → `.arrow` written to `cache_path` → `{schema, rows}` reply**.
pub fn convert(
    source: &str,
    cache_path: &str,
    ingest: &IngestParams,
) -> Result<ConvertOutput, Box<dyn std::error::Error>> {
    let locale = Locale::new(ingest.decimal_sep, ingest.thousands_sep);
    let threshold = ingest.threshold;
    let sort_limit = ingest.sort_limit;
    let nulls = null_spellings(ingest);

    // Delimiter sniff + header from a small prefix; the whole file is never held.
    let head = read_head(source, 1 << 20)?;
    let delim = sniff_delimiter(&head);
    let mut names = read_header_names(&head, delim);
    jasp_column_names(&mut names);
    let ncols = names.len();
    if ncols == 0 {
        return Err("no header columns found".into());
    }
    let chunk_target = auto_chunk_target(ncols);
    let null_regex = build_null_regex(&nulls)?;

    // ① row-wise multithreaded inference.
    let cap = threshold.max(DISTINCT_CAP) + 1;
    let schema = utf8_schema(&names);
    let (mut stats, nrows) = {
        std::thread::scope(
            |s| -> Result<(Vec<ColStats>, usize), Box<dyn std::error::Error + Send + 'static>> {
                let (tx, rx) = std::sync::mpsc::sync_channel::<(usize, Vec<u8>)>(THREADS);
                let rx = Arc::new(std::sync::Mutex::new(rx));
                let infer_batch = (CELL_BUDGET / ncols.max(1)).clamp(1, BATCH);
                let (stats_tx, stats_rx) =
                    std::sync::mpsc::sync_channel::<(usize, Vec<ColStats>, usize)>(THREADS * 2);

                // Merger: fold per-chunk stats in chunk-id order as they arrive.
                let merger = s.spawn(move || -> (Vec<ColStats>, usize) {
                    let mut stats: Vec<ColStats> = (0..ncols).map(|_| ColStats::new()).collect();
                    let mut nrows = 0usize;
                    let mut next_id = 0usize;
                    let mut pending: HashMap<usize, (Vec<ColStats>, usize)> = HashMap::new();
                    while let Ok((id, cs, rows)) = stats_rx.recv() {
                        pending.insert(id, (cs, rows));
                        drain_pending(&mut pending, &mut stats, &mut nrows, &mut next_id, cap);
                    }
                    drain_pending(&mut pending, &mut stats, &mut nrows, &mut next_id, cap);
                    (stats, nrows)
                });

                // Workers: dequeue chunks, parse each, observe into per-chunk stats, hand the
                // result to the merger immediately (no per-worker hoarding).
                let mut handles = Vec::with_capacity(THREADS);
                for _ in 0..THREADS {
                    let rx = Arc::clone(&rx);
                    let schema = Arc::clone(&schema);
                    let null_regex = null_regex.clone();
                    let stats_tx = stats_tx.clone();
                    handles.push(s.spawn(move || -> Result<(), arrow::error::ArrowError> {
                        loop {
                            let item = {
                                let guard = rx.lock().unwrap();
                                guard.recv()
                            };
                            let (chunk_id, bytes) = match item {
                                Ok(x) => x,
                                Err(_) => break,
                            };
                            let mut rdr =
                                arrow::csv::reader::ReaderBuilder::new(Arc::clone(&schema))
                                    .with_header(false)
                                    .with_delimiter(delim)
                                    .with_batch_size(infer_batch)
                                    .with_null_regex(null_regex.clone())
                                    .build(std::io::Cursor::new(bytes))?;
                            let mut chunk_stats: Vec<ColStats> =
                                (0..ncols).map(|_| ColStats::new()).collect();
                            let mut rows = 0usize;
                            while let Some(rb) = rdr.next().transpose()? {
                                rows += rb.num_rows();
                                for (i, col) in rb.columns().iter().enumerate() {
                                    let sa = col.as_string::<i32>();
                                    let st = &mut chunk_stats[i];
                                    for v in sa.iter().flatten() {
                                        st.observe(v, cap, locale);
                                    }
                                }
                            }
                            if stats_tx.send((chunk_id, chunk_stats, rows)).is_err() {
                                break;
                            }
                        }
                        Ok(())
                    }));
                }
                drop(stats_tx);

                // Producer (this thread).
                let produce_result = produce_chunks(source, chunk_target, &tx);
                drop(tx);

                let mut worker_err: Option<arrow::error::ArrowError> = None;
                for h in handles {
                    if let Err(e) = h.join().expect("worker thread panicked")
                        && worker_err.is_none()
                    {
                        worker_err = Some(e);
                    }
                }
                let (stats, nrows) = merger.join().expect("merger thread panicked");
                if let Some(e) = worker_err {
                    return Err(Box::new(e));
                }
                produce_result
                    .map_err(|e| -> Box<dyn std::error::Error + Send + 'static> { Box::new(e) })?;
                Ok((stats, nrows))
            },
        )
        .map_err(|e| e.to_string())?
    };

    let levels: Vec<Level> = stats.iter().map(|s| decide(s, threshold)).collect();
    let out_schema = build_output_schema(&names, &levels, &stats);

    // ② pre-build each categorical's shared dictionary, CONSUMING each column's inference
    // IndexSet (moved, not cloned); then drop the (now empty-shell) stats' distinct sets — but
    // keep `stats` itself for the schema's `only_ints` (all_integer) until after the schema is
    // built and the column info captured.
    let cat_dicts: Vec<Option<CatDict>> = (0..ncols)
        .map(|i| match levels[i] {
            Level::Scale => None,
            Level::Ordinal | Level::Nominal => {
                let canonicalize = locale.active && (stats[i].only_ints || stats[i].only_doubles);
                let distinct = std::mem::take(&mut stats[i].distinct);
                Some(prebuild_dict(distinct, canonicalize, locale, sort_limit))
            }
        })
        .collect();

    // Capture the wire schema (needs the dictionaries' levels + stats' only_ints) before the
    // build loop drops `stats`.
    let columns: Vec<ColumnInfo> = (0..ncols)
        .map(|i| {
            let levels_values = cat_dicts[i].as_ref().map(|cd| {
                cd.values
                    .as_string::<i32>()
                    .iter()
                    .flatten()
                    .take(WIRE_LEVELS_CAP)
                    .map(|s| s.to_string())
                    .collect()
            });
            // Distinct NUMERIC levels (data-model-design.md §2): canonical dictionaries parse
            // plain; raw dictionaries parse with the source locale. Dedupe on the parsed
            // number, exclude non-finite ("NaN"/"inf" cells are not numeric levels).
            let numeric_levels = cat_dicts[i]
                .as_ref()
                .map(|cd| numeric_levels_of(cd, locale));
            ColumnInfo {
                name: names[i].clone(),
                display_name: names[i].clone(),
                level: levels[i].as_str(),
                all_integer: levels[i] == Level::Scale && stats[i].only_ints,
                value_count: stats[i].count as u64,
                // Categoricals: the dictionary IS the distinct set (always exact). Scale: the
                // inference-tracked raw set (exact up to DISTINCT_CAP), deduped on the parsed
                // number so textually different but equal values ("1.5"/"1.50", "0"/"-0.0")
                // count once — legacy doubleset semantics, bounded by the cap.
                distinct_count: match &cat_dicts[i] {
                    Some(cd) => cd.index.len() as u64,
                    None => count_distinct_numbers(
                        stats[i].distinct.iter().map(|v| locale.parse_num(v)),
                    ),
                },
                numeric_levels,
                levels: levels_values,
                labels: None, // CSV open never creates value≠label data (sparse = absent)
            }
        })
        .collect();
    drop(stats);

    // ③ sequential streaming build: re-read one batch at a time, encode, write, drop. Byte-
    // budgeted batch with a 1M-row cap bounds build RSS at ~one batch.
    let mut writer = make_feather_writer(cache_path, &out_schema)?;
    let file_bytes = fs::metadata(source).map(|m| m.len() as usize).unwrap_or(0);
    let bytes_per_row = file_bytes.checked_div(nrows).unwrap_or(0);
    let budget_bytes = BATCH_MB.saturating_mul(1 << 20);
    let build_batch = budget_bytes
        .checked_div(bytes_per_row)
        .map(|r| r.clamp(16, MAX_BUILD_ROWS))
        .unwrap_or(BATCH.min(MAX_BUILD_ROWS));
    let mut rdr = csv_reader(
        utf8_schema(&names),
        BufReader::new(File::open(source)?),
        delim,
        build_batch,
        null_regex,
    )?;
    let mut written = 0usize;
    while let Some(sb) = rdr.next().transpose()? {
        written += sb.num_rows();
        let cols = cast_batch_stream(&sb, &levels, &cat_dicts, locale, &out_schema)?;
        let batch = RecordBatch::try_new(Arc::clone(&out_schema), cols)?;
        writer.write(&batch)?;
    }
    writer.finish()?;

    Ok(ConvertOutput {
        rows: written as u64,
        columns,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // The single-new-column form must agree with the whole-header convention it came from
    // (jasp_column_names): same three rules, applied to one name at a known position.
    #[test]
    fn unique_new_name_follows_the_importer_convention() {
        let existing: Vec<String> = vec!["V1".into(), "cont".into(), "V3".into()];
        // position 4 (1-based) — the next column after `existing`.
        assert_eq!(unique_new_name(&existing, "", 4), "V4");
        assert_eq!(unique_new_name(&existing, "score", 4), "score");
        assert_eq!(unique_new_name(&existing, "5", 4), "V5");
        assert_eq!(unique_new_name(&existing, "007", 4), "007"); // not pure-int
        assert_eq!(unique_new_name(&existing, "cont", 4), "cont_4"); // duplicate
        assert_eq!(unique_new_name(&existing, "V1", 4), "V1_4");
    }

    // Absorption (§4 rule 1): unseen values append at the end in first-appearance order;
    // seen values keep their index so existing keys stay valid.
    #[test]
    fn append_levels_extends_in_first_appearance_order() {
        let base = prebuild_dict(
            ["a".to_string(), "b".to_string()].into_iter().collect(),
            false,
            Locale::new('.', None),
            0, // sort_limit 0: never sort — deterministic order for the assertion
        );
        let ext = append_levels(
            &base,
            ["z".to_string(), "a".to_string(), "m".to_string()].into_iter(),
        );
        let values: Vec<String> = ext
            .values
            .as_string::<i32>()
            .iter()
            .flatten()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(
            values,
            vec!["a", "b", "z", "m"],
            "seen keeps index, new appends"
        );
        assert_eq!(ext.index["a"], 0);
        assert_eq!(ext.index["z"], 2);
        assert_eq!(ext.index["m"], 3);
    }

    // One decoration rule (§8.3): the ordered flag carries ordinal-vs-nominal, metadata
    // keys are exactly the three jasp:* entries, scale carries all_integer only when true.
    #[test]
    fn jasp_field_decorates_the_output_contract() {
        let s = jasp_field("x", "X disp", Level::Scale, true);
        assert_eq!(s.data_type(), &DataType::Float64);
        assert_eq!(
            s.metadata().get("jasp:display_name").map(String::as_str),
            Some("X disp")
        );
        assert_eq!(
            s.metadata().get("jasp:all_integer").map(String::as_str),
            Some("true")
        );
        assert!(s.metadata().get("jasp:auto_sort_by_value").is_none());

        let n = jasp_field("g", "g", Level::Nominal, false);
        assert!(matches!(n.data_type(), DataType::Dictionary(_, _)));
        assert_eq!(n.dict_is_ordered(), Some(false));

        let o = jasp_field("r", "r", Level::Ordinal, false);
        assert_eq!(
            o.dict_is_ordered(),
            Some(true),
            "the ordered flag IS the ordinal marker"
        );

        let plain = jasp_field("p", "p", Level::Scale, false);
        assert!(plain.metadata().get("jasp:all_integer").is_none());

        // The decoration round-trips: jasp_field → level_of_field is the identity for all
        // three levels — the edit engine reads back exactly what the conversion wrote.
        for level in [Level::Scale, Level::Ordinal, Level::Nominal] {
            let f = jasp_field("x", "x", level, false);
            assert_eq!(level_of_field(&f), Some(level));
        }
        assert_eq!(
            level_of_field(&Field::new("i", DataType::Int64, true)),
            None,
            "a non-v1 cache shape has no level"
        );
    }

    // Column naming must match the GUI importer (csvimporter.cpp) exactly, because the
    // analysis side (read_jasp_data / tidyselect) references columns by these names.
    #[test]
    fn jasp_column_names_matches_gui_importer() {
        let mut names: Vec<String> = vec![
            "".into(),        // empty (the row-index column) -> V1
            "cont".into(),    // normal name unchanged
            "3".into(),       // pure integer -> V3
            "cont".into(),    // duplicate -> cont_4 (1-based position)
            "007".into(),     // NOT pure-int (stoi roundtrip fails) -> unchanged
            "1hahaha".into(), // stoi prefix != whole string -> unchanged
            "V1".into(),      // collides with the V1 created above -> V1_7
        ];
        jasp_column_names(&mut names);
        assert_eq!(
            names,
            vec!["V1", "cont", "V3", "cont_4", "007", "1hahaha", "V1_7"]
        );
    }

    // The encoding-torture dataset (test_data/encoding_torture.csv; HANDOVER-runner-data-
    // pruning.md §8.6 special-character dataset): headers with spaces, CJK, reserved words,
    // leading digits, dots, an alias-shaped raw name, plus lane-normalization cases. Lane
    // contract: everything passes through RAW except empty -> V{n} and pure-integer ->
    // V{name}. The runner's alias codec round-trips exactly these names
    // (refactor_design/tests/walk_test.R, section 10).
    #[test]
    fn encoding_torture_csv_lane_contract() {
        let src = format!(
            "{}/../../../test_data/encoding_torture.csv",
            env!("CARGO_MANIFEST_DIR")
        );
        let dir = std::env::temp_dir().join("csv2arrow-tests-torture");
        fs::create_dir_all(&dir).unwrap();
        let dst = dir.join("torture.arrow");
        let out = convert(&src, dst.to_str().unwrap(), &IngestParams::default())
            .expect("torture CSV converts");
        let names: Vec<&str> = out.columns.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "subject id",
                "reaction time",
                "국어 점수",
                "weight.kg",
                "T",
                "if",
                "3rd measurement",
                "treatment_group",
                "score",
                "jasp_enc_hex_61_scale",
                "V2020", // pure-integer header 2020
                "V12",   // empty header, 1-based position 12
                "groep",
            ],
            "lane normalization: raw names untouched, 2020 -> V2020, empty -> V12"
        );
        assert_eq!(out.rows, 24);
        let level = |n: &str| {
            out.columns
                .iter()
                .find(|c| c.name == n)
                .unwrap_or_else(|| panic!("column {n}"))
                .level
        };
        assert_eq!(level("subject id"), "nominal");
        assert_eq!(level("reaction time"), "scale");
        assert_eq!(level("국어 점수"), "scale");
        assert_eq!(level("if"), "nominal"); // reserved-word name, string values
        assert_eq!(level("jasp_enc_hex_61_scale"), "scale"); // alias-shaped raw name
        assert_eq!(level("groep"), "nominal");
    }

    // ── Constraint-check stats (data-model-design.md §2) ────────────────────────────────

    /// Write `csv` to a per-test temp file, convert it, return the lane's output.
    fn convert_csv(test: &str, csv: &str, ingest: IngestParams) -> ConvertOutput {
        let dir = std::env::temp_dir().join(format!("csv2arrow-tests-{test}"));
        fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.csv");
        let dst = dir.join("out.arrow");
        fs::write(&src, csv).unwrap();
        convert(src.to_str().unwrap(), dst.to_str().unwrap(), &ingest)
            .unwrap_or_else(|e| panic!("convert failed: {e}"))
    }

    fn col<'a>(out: &'a ConvertOutput, name: &str) -> &'a ColumnInfo {
        out.columns
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("column {name} missing"))
    }

    #[test]
    fn value_and_distinct_counts() {
        let out = convert_csv(
            "value_and_distinct_counts",
            "s,const,mixed_empty\n1.5,7,1\n2.5,7,NA\n3.5,7,\n4.5,7,3\n",
            IngestParams::default(),
        );
        assert_eq!(out.rows, 4);

        // Scale: every non-empty value counted, distinct exact.
        let s = col(&out, "s");
        assert_eq!(s.level, "scale");
        assert_eq!(s.value_count, 4);
        assert_eq!(s.distinct_count, 4);
        assert_eq!(
            s.numeric_levels, None,
            "scale columns carry no numeric_levels"
        );

        // Constant column: all cells hold the SAME value — 1 distinct, 4 non-empty.
        let c = col(&out, "const");
        assert_eq!(c.value_count, 4);
        assert_eq!(c.distinct_count, 1);

        // "NA" and blank are EMPTY — not values: 2 non-empty cells, 2 distinct.
        let m = col(&out, "mixed_empty");
        assert_eq!(m.value_count, 2);
        assert_eq!(m.distinct_count, 2);
    }

    #[test]
    fn zero_is_a_value_and_signed_zero_counts_once() {
        let out = convert_csv("signed_zero", "z\n0\n-0.0\n0.0\n", IngestParams::default());
        let z = col(&out, "z");
        assert_eq!(z.level, "scale");
        assert_eq!(z.value_count, 3, "zero IS a value");
        assert_eq!(z.distinct_count, 1, "0, -0.0, 0.0 are one number");
    }

    #[test]
    fn numeric_dedup_textually_different_equal_values() {
        // "1.5"/"1.50"/"1.500" are ONE number — legacy doubleset semantics. A constant
        // column written with mixed representations must NOT pass a >=2-distinct gate.
        let out = convert_csv(
            "numeric_dedup",
            "n\n1.5\n1.50\n1.500\n",
            IngestParams::default(),
        );
        let n = col(&out, "n");
        assert_eq!(n.value_count, 3);
        assert_eq!(n.distinct_count, 1);
    }

    #[test]
    fn distinct_count_cap_is_a_lower_bound() {
        // 1500 distinct scale values: the reported count caps at DISTINCT_CAP+1 — "at least
        // this many" (every real form threshold is <= 256, far below the cap).
        let mut csv = String::from("big\n");
        for i in 0..1500 {
            csv.push_str(&format!("{}.{:03}\n", i, i % 1000));
        }
        let out = convert_csv("distinct_cap", &csv, IngestParams::default());
        let b = col(&out, "big");
        assert_eq!(b.level, "scale");
        assert_eq!(b.value_count, 1500);
        // The set stops one insert past `cap = max(threshold, DISTINCT_CAP)+1`, so the
        // reported bound is DISTINCT_CAP+2. The exact bound is irrelevant — what matters is
        // that it means "at least this many" and sits far above every real threshold (≤ 256).
        assert_eq!(
            b.distinct_count,
            DISTINCT_CAP as u64 + 2,
            "the capped value means 'at least this many'"
        );
    }

    #[test]
    fn numeric_levels_locale_aware() {
        // Decimal-comma file: numeric levels must be recognized via the SOURCE locale
        // (the frontend never parses wire values — this count is made here).
        let ingest = IngestParams {
            decimal_sep: ',',
            ..Default::default()
        };
        let out = convert_csv(
            "numeric_levels_locale",
            "item;score\nalpha;1,5\nbeta;2,5\ngamma;text\n",
            ingest,
        );
        assert_eq!(out.rows, 3);
        let item = col(&out, "item");
        assert_eq!(item.distinct_count, 3);
        assert_eq!(item.numeric_levels, Some(0));
        // Mixed numeric/text column: 3 distinct values, 2 of them numeric ("1,5", "2,5").
        let score = col(&out, "score");
        assert_eq!(score.value_count, 3);
        assert_eq!(score.distinct_count, 3);
        assert_eq!(
            score.numeric_levels,
            Some(2),
            "locale-formatted numbers must count"
        );
    }

    #[test]
    fn scale_under_locale_counts_numeric_distincts() {
        // Scale distinct counting is numeric, locale-aware: "1,5" and "1,50" are one number.
        let ingest = IngestParams {
            decimal_sep: ',',
            ..Default::default()
        };
        let out = convert_csv("scale_locale", "n;tag\n1,5;a\n2,5;b\n1,50;a\n4;c\n", ingest);
        let n = col(&out, "n");
        assert_eq!(n.level, "scale");
        assert_eq!(n.value_count, 4);
        assert_eq!(
            n.distinct_count, 3,
            "1,5 and 1,50 are one number under the source locale"
        );
    }

    #[test]
    fn categorical_numeric_levels_dedup_numbers() {
        // Mixed column: "1.5" and "1.50" are distinct LEVELS but one NUMBER.
        let out = convert_csv(
            "cat_numeric_dedup",
            "m\n1.5\n1.50\nabc\n",
            IngestParams::default(),
        );
        let m = col(&out, "m");
        assert_eq!(m.distinct_count, 3);
        assert_eq!(m.numeric_levels, Some(1));
        assert_eq!(m.levels.as_ref().map(|l| l.len()), Some(3));
    }

    #[test]
    fn wire_levels_capped_distinct_count_stays_truth() {
        // 10_001 distinct nominal values: the wire levels stop at WIRE_LEVELS_CAP, but
        // distinct_count stays exact — it is the single source of truth for counts
        // (data-model-design.md §2, decision 15). Truncation is implicit:
        // distinct_count > levels.len().
        let mut csv = String::from("c\n");
        for i in 0..WIRE_LEVELS_CAP + 1 {
            csv.push_str(&format!("v{i:05}\n"));
        }
        let out = convert_csv("levels_cap", &csv, IngestParams::default());
        let c = col(&out, "c");
        assert_eq!(c.level, "nominal");
        assert_eq!(c.value_count, WIRE_LEVELS_CAP as u64 + 1);
        assert_eq!(
            c.distinct_count,
            WIRE_LEVELS_CAP as u64 + 1,
            "distinct_count is the exact truth"
        );
        assert_eq!(
            c.levels.as_ref().map(|l| l.len()),
            Some(WIRE_LEVELS_CAP),
            "wire levels are capped"
        );
        assert_eq!(c.numeric_levels, Some(0));
    }

    /// Benchmark/regression harness for big files (the bt.csv / eu_tall.csv class):
    ///   JASP_LANE_BENCH=/path/to/file.csv cargo test -- --ignored --nocapture lane_bench_file
    /// Prints wall time + per-column stats; a hang or absurd stat is a failure.
    #[test]
    #[ignore]
    fn lane_bench_file() {
        let Ok(src) = std::env::var("JASP_LANE_BENCH") else {
            eprintln!("set JASP_LANE_BENCH=/path/to/file.csv");
            return;
        };
        let dst = std::env::temp_dir().join("csv2arrow-bench.arrow");
        let t = std::time::Instant::now();
        let out = convert(&src, dst.to_str().unwrap(), &IngestParams::default())
            .unwrap_or_else(|e| panic!("conversion failed after {:?}: {e}", t.elapsed()));
        let dt = t.elapsed();
        let mb = fs::metadata(&src).map(|m| m.len() / (1 << 20)).unwrap_or(0);
        println!(
            "converted {mb} MiB / {} rows / {} cols in {:?} ({:.1} MiB/s)",
            out.rows,
            out.columns.len(),
            dt,
            mb as f64 / dt.as_secs_f64().max(0.001)
        );
        for c in &out.columns {
            println!(
                "  {:>12} {:>8} value_count={} distinct_count={} numeric_levels={:?}",
                c.name, c.level, c.value_count, c.distinct_count, c.numeric_levels
            );
        }
    }
}
