//! Analysis-view materialization (orchestrator-v2 §8 + analysis-views AV4/AV5/§5).
//!
//! **Naming hazard (analysis-views §11):** this is NOT the grid's `data_view` (chunked
//! TSV reads — [`crate::arrowview`]). An analysis view is a *typed Arrow projection* of
//! one dataset, content-addressed by `view_id = hash(VIEW_FORMAT_VERSION, spec,
//! base_revision)`, built deterministically from a router-ordered `cache_fill` (the
//! implied build — never a frontend-visible work unit).
//!
//! The worker owns ALL casts and relabeling (AV4) — the §8.3 semantics as SYSTEM
//! law, not any language's folklore (multi-runtime: a future Julia/Python runner
//! family reads these blobs; legacy JASP cast in C++ anyway, so no language's
//! cosmetics are the contract). The migration-era R fallback (`read_jasp_data`)
//! CONFORMS to this matrix (its mirror of `level_string` dies with it, slice D):
//!
//! | Stored as | Requested | Materialized |
//! |---|---|---|
//! | dictionary | scale | the **values** — the k dictionary values parsed to f64 ONCE, rows indexed by key (never per row); a value that doesn't parse → null (`Male` as scale) |
//! | dictionary | nominal | dictionary, levels relabelled value→label via `jasp:labels` (order preserved); nominal is UNORDERED, always |
//! | dictionary | ordinal | same, `ordered=1` — the dictionary sequence order is the ranking |
//! | float64 | scale | copied directly (NaN included) — no factor round-trip |
//! | float64 | nominal/ordinal | dictionary of the distinct values, levels **numerically** sorted; level strings via [`level_string`] (shortest round-trip, collision-free); `ordered=1` for ordinal; non-finite (NaN/±Inf) → null — missing is missing, not a category |
//!
//! The artifact (AV5/§6/§5): Feather V2 + LZ4 (the caches' codec stack), fields named
//! `<real name>__<type>` (AV6 — the type vocabulary never contains `__`, so parsing
//! anchors at the LAST `__`), a base-row index column (`__base_row`, int32, 0-based),
//! and `jasp:view` schema metadata carrying the decode authority: the real↔token map
//! (token = hex of the display name's UTF-8 bytes — name-derived, stable under
//! insert/reorder, AV6), the base revision, the spec, and the row count.
//!
//! Determinism is load-bearing (AV8): same `(spec, base)` ⇒ byte-identical rebuild —
//! fixed field order, deterministic dictionary orders (base order preserved /
//! numerically sorted), no timestamps, the same LZ4 IPC writer options as the caches.
//! The differential test (build → delete → rebuild → byte-compare) is the gate.
//!
//! Scope guards (v1): filters are BLOCKED until the derived-columns design lands (a
//! filter must BE data; there is no R-expression evaluator in Rust — rejected, AV §14);
//! a `filter` in the spec is refused visibly by the ROUTER, and again here (defense in
//! depth). Only the cache's own column shapes (float64 / dictionary<int32,utf8>) are
//! castable — anything else is refused visibly rather than guessed. The caches are
//! written with ONE shared dictionary per categorical field (csv2arrow/dataedit
//! contract), so the first batch's dictionary is authoritative for the whole file.

use crate::csv2arrow;
use crate::messages;
use arrow::array::{
    Array, ArrayRef, DictionaryArray, Float64Array, Float64Builder, Int32Builder, StringArray,
};
use arrow::datatypes::{DataType, Field, Int32Type, Schema, SchemaRef};
use arrow::record_batch::RecordBatch;
use arrow_ipc::reader::FileReader;
use serde_json::json;
use std::collections::HashMap;
use std::fs::File;
use std::sync::Arc;

/// The base-row index column's field name (§5: "pinned as a must, exact spelling
/// open"). Also duplicated in the `jasp:view` metadata so consumers never hard-code it.
pub(crate) const BASE_ROW_FIELD: &str = "__base_row";

// ── Entry ──────────────────────────────────────────────────────────────────────

/// Materialize one view blob per a router order. Every `Err` is a deterministic,
/// user-visible failure (unknown column, unsupported stored type, filter present) —
/// the router fails the waiting works with it; retrying an unbuildable spec can never
/// succeed, so it is never retried. Returns the written blob's size in bytes.
///
/// One batch, in memory: the spec IS the prune (a view names its read-set; wide-all
/// shapes are the router's pass-through and never reach a build), so a view is small
/// by construction. Streaming is a later knob if a shape ever demands it.
pub(crate) fn build(fill: &messages::CacheFill) -> Result<u64, String> {
    if fill.spec.filter.is_some() {
        return Err(
            "view filters are not supported yet (awaiting the derived-columns design)".to_string(),
        );
    }
    let Some(columns) = fill.spec.columns.as_ref().filter(|c| !c.is_empty()) else {
        return Err(
            "a buildable view spec must name at least one column (all/no columns is the \
             router's pass-through shape, which never reaches a build)"
                .to_string(),
        );
    };

    let mut reader = FileReader::try_new(
        File::open(&fill.source).map_err(|e| format!("open '{}': {e}", fill.source))?,
        None,
    )
    .map_err(|e| format!("cannot open base cache '{}': {e}", fill.source))?;
    let schema = reader.schema();

    // Plan every spec entry up front (resolution + cast decision — data-independent),
    // refusing duplicates (same name at the same type) before any I/O-heavy work.
    let mut plans: Vec<(usize, Plan)> = Vec::with_capacity(columns.len());
    for (i, c) in columns.iter().enumerate() {
        if columns[..i]
            .iter()
            .any(|o| o.name == c.name && o.as_type == c.as_type)
        {
            return Err(format!(
                "duplicate spec entry: '{}' as {} appears twice",
                c.name,
                c.as_type.as_str()
            ));
        }
        let idx = resolve_column(&schema, &c.name)?;
        plans.push((idx, Plan::new(&schema, idx, c)?));
    }

    // Stream the base, collecting per-row raw values into the plans.
    let mut rows: usize = 0;
    while let Some(batch) = reader
        .next()
        .transpose()
        .map_err(|e| format!("reading base cache '{}': {e}", fill.source))?
    {
        for (idx, plan) in &mut plans {
            plan.absorb(&batch, *idx)?;
        }
        rows += batch.num_rows();
    }

    // Finalize arrays + fields in spec order, plus the base-row index column.
    let mut fields: Vec<Field> = Vec::with_capacity(plans.len() + 1);
    let mut arrays: Vec<ArrayRef> = Vec::with_capacity(plans.len() + 1);
    let mut token_map = serde_json::Map::new();
    for ((_idx, plan), c) in plans.into_iter().zip(columns.iter()) {
        let (field, array) = plan.finish(&c.name, c.as_type)?;
        token_map.insert(c.name.clone(), json!(hex_token(&c.name)));
        fields.push(field);
        arrays.push(array);
    }
    let mut base_row = Int32Builder::with_capacity(rows);
    for i in 0..rows {
        base_row.append_value(i as i32);
    }
    fields.push(Field::new(BASE_ROW_FIELD, DataType::Int32, false));
    arrays.push(Arc::new(base_row.finish()));

    // The `jasp:view` metadata block — the decode authority (AV5: the map is the
    // protocol; nobody re-derives tokens).
    let view_meta = json!({
        "format_version": messages::VIEW_FORMAT_VERSION,
        "view_id": fill.view_id,
        "base_revision": fill.base_revision,
        "dataset_id": fill.spec.dataset_id,
        "rows": rows,
        "base_row_column": BASE_ROW_FIELD,
        "token_map": token_map,
        "spec": fill.spec,
    });
    let out_schema: SchemaRef = Arc::new(
        Schema::new(fields).with_metadata(
            vec![("jasp:view".to_string(), view_meta.to_string())]
                .into_iter()
                .collect(),
        ),
    );

    // Write to a temp sibling then rename, so a reader never sees a partial blob (the
    // content address means a rename over an identical blob is a no-op).
    let target = std::path::Path::new(&fill.target);
    if let Some(parent) = target.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(|e| format!("create {}: {e}", parent.display()))?;
    }
    let tmp = target.with_extension(format!("tmp.{}", std::process::id()));
    {
        let mut writer = csv2arrow::make_feather_writer(tmp.to_str().unwrap(), &out_schema)
            .map_err(|e| format!("create view writer: {e}"))?;
        let batch = RecordBatch::try_new(Arc::clone(&out_schema), arrays)
            .map_err(|e| format!("assemble view batch: {e}"))?;
        writer
            .write(&batch)
            .map_err(|e| format!("write view batch: {e}"))?;
        writer.finish().map_err(|e| format!("finalize view: {e}"))?;
    }
    std::fs::rename(&tmp, target).map_err(|e| format!("publish view: {e}"))?;
    let bytes = std::fs::metadata(target)
        .map_err(|e| format!("stat view: {e}"))?
        .len();
    Ok(bytes)
}

// ── The cast plans ────────────────────────────────────────────────────────────

/// One spec entry's cast. All four shapes collect per-row raw values while streaming;
/// dictionaries/lookups are computed once from the first batch (single-dictionary
/// caches) and applied at finish. Field name and type per AV6/§8.3.
enum Plan {
    /// float64 → scale: verbatim copy (NaN included — a scale read keeps it).
    ScaleF64 { raw: Vec<Option<f64>> },
    /// float64 → nominal/ordinal: raw values (non-finite → null at absorb); the
    /// dictionary = distinct values sorted NUMERICALLY, levels via `level_string`,
    /// finalized at end.
    CatF64 { raw: Vec<Option<f64>> },
    /// dictionary → scale: the k dictionary values parsed once (unparseable → null);
    /// rows index the parse by key. AV4's "values parsed once, never per row".
    ScaleDict {
        parsed: Vec<Option<f64>>, // by dictionary index
        raw: Vec<Option<i32>>,    // per-row keys
    },
    /// dictionary → nominal/ordinal: keys pass through; the output dictionary = the
    /// base dictionary relabelled value→label via `jasp:labels` (order preserved —
    /// the ranking for ordinals); nominal is UNORDERED, ordinal is ordered.
    CatDict {
        out_values: Vec<String>, // relabelled, by dictionary index
        raw: Vec<Option<i32>>,
    },
}

impl Plan {
    fn new(schema: &SchemaRef, idx: usize, c: &messages::ViewColumn) -> Result<Self, String> {
        let field = schema.field(idx);
        match field.data_type() {
            DataType::Float64 => Ok(match c.as_type {
                messages::ViewLevel::Scale => Plan::ScaleF64 { raw: Vec::new() },
                messages::ViewLevel::Nominal | messages::ViewLevel::Ordinal => {
                    Plan::CatF64 { raw: Vec::new() }
                }
            }),
            DataType::Dictionary(_, _) => Ok(match c.as_type {
                messages::ViewLevel::Scale => Plan::ScaleDict {
                    parsed: Vec::new(),
                    raw: Vec::new(),
                },
                messages::ViewLevel::Nominal | messages::ViewLevel::Ordinal => Plan::CatDict {
                    out_values: Vec::new(),
                    raw: Vec::new(),
                },
            }),
            other => Err(format!(
                "column '{}' has unsupported stored type {other} (views cast only the \
                 cache's float64 / dictionary shapes)",
                c.name
            )),
        }
    }

    fn absorb(&mut self, batch: &RecordBatch, idx: usize) -> Result<(), String> {
        match self {
            Plan::ScaleF64 { raw } => {
                let col = batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or("stored float64 column is not float64")?;
                for i in 0..col.len() {
                    raw.push(if col.is_null(i) {
                        None
                    } else {
                        Some(col.value(i))
                    });
                }
            }
            Plan::CatF64 { raw } => {
                let col = batch
                    .column(idx)
                    .as_any()
                    .downcast_ref::<Float64Array>()
                    .ok_or("stored float64 column is not float64")?;
                for i in 0..col.len() {
                    if col.is_null(i) {
                        raw.push(None);
                    } else {
                        let v = col.value(i);
                        // Non-finite is MISSING, not a category (missing is missing;
                        // the scale cast keeps NaN, the categorical one drops it).
                        raw.push(if v.is_finite() { Some(v) } else { None });
                    }
                }
            }
            Plan::ScaleDict { parsed, raw } => {
                let dict = dict_of(batch.column(idx))?;
                if parsed.is_empty() {
                    let values = dict
                        .values()
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or("dictionary values are not utf8")?;
                    *parsed = (0..values.len())
                        .map(|vi| values.value(vi).parse::<f64>().ok())
                        .collect();
                }
                push_keys(dict, raw);
            }
            Plan::CatDict {
                out_values, raw, ..
            } => {
                let dict = dict_of(batch.column(idx))?;
                if out_values.is_empty() {
                    let values = dict
                        .values()
                        .as_any()
                        .downcast_ref::<StringArray>()
                        .ok_or("dictionary values are not utf8")?;
                    // `jasp:labels` overlay: label where set, else the value (§8.3 —
                    // sparse, often empty entirely; value == label is the common case).
                    let batch_schema = batch.schema();
                    let base_field = batch_schema.field(idx);
                    let labels = base_field
                        .metadata()
                        .get("jasp:labels")
                        .and_then(|lj| serde_json::from_str::<serde_json::Value>(lj).ok())
                        .and_then(|v| v.as_object().cloned());
                    *out_values = (0..values.len())
                        .map(|vi| {
                            let value = values.value(vi);
                            match &labels
                                .as_ref()
                                .and_then(|m| m.get(value))
                                .and_then(|l| l.as_str())
                            {
                                Some(l) if !l.is_empty() => l.to_string(),
                                _ => value.to_string(),
                            }
                        })
                        .collect();
                }
                push_keys(dict, raw);
            }
        }
        Ok(())
    }

    fn finish(self, name: &str, level: messages::ViewLevel) -> Result<(Field, ArrayRef), String> {
        let field_name = format!("{}__{}", name, level.as_str());
        match self {
            Plan::ScaleF64 { raw } => {
                let mut b = Float64Builder::with_capacity(raw.len());
                for v in raw {
                    b.append_option(v);
                }
                Ok((
                    Field::new(field_name, DataType::Float64, true),
                    Arc::new(b.finish()),
                ))
            }
            Plan::CatF64 { raw } => {
                // Levels = distinct values sorted NUMERICALLY, ±0.0 unified, rendered
                // via `level_string` and DEDUPED ON THE STRING — values agreeing at
                // 15 significant digits are one category (the %.15g grouping rule;
                // after the numeric sort, equal renderings are adjacent).
                let norm = |v: f64| if v == 0.0 { 0.0 } else { v }; // -0.0 ≡ 0.0
                let mut distinct: Vec<f64> = raw.iter().filter_map(|v| v.map(norm)).collect();
                distinct.sort_by(|a, b| a.partial_cmp(b).unwrap());
                distinct.dedup();
                let mut levels: Vec<String> = Vec::with_capacity(distinct.len());
                for v in &distinct {
                    let s = level_string(*v);
                    if levels.last() != Some(&s) {
                        levels.push(s); // first occurrence in numeric order wins
                    }
                }
                let lookup: HashMap<&str, i32> = levels
                    .iter()
                    .enumerate()
                    .map(|(i, s)| (s.as_str(), i as i32))
                    .collect();
                let mut keys = Int32Builder::with_capacity(raw.len());
                for v in &raw {
                    match v {
                        None => keys.append_null(),
                        Some(v) => keys.append_value(lookup[level_string(norm(*v)).as_str()]),
                    }
                }
                let values: Vec<Option<String>> = levels.into_iter().map(Some).collect();
                let arr = DictionaryArray::<Int32Type>::try_new(
                    keys.finish(),
                    Arc::new(StringArray::from(values)),
                )
                .map_err(|e| format!("dictionary keys out of range: {e}"))?;
                Ok((
                    cat_field(&field_name, level == messages::ViewLevel::Ordinal),
                    Arc::new(arr),
                ))
            }
            Plan::ScaleDict { parsed, raw } => {
                let mut b = Float64Builder::with_capacity(raw.len());
                for k in &raw {
                    b.append_option(k.and_then(|k| parsed.get(k as usize).copied().flatten()));
                }
                Ok((
                    Field::new(field_name, DataType::Float64, true),
                    Arc::new(b.finish()),
                ))
            }
            Plan::CatDict { out_values, raw } => {
                let values: Vec<Option<String>> = out_values.into_iter().map(Some).collect();
                let mut keys = Int32Builder::with_capacity(raw.len());
                for k in &raw {
                    match k {
                        None => keys.append_null(),
                        Some(k) => keys.append_value(*k),
                    }
                }
                let arr = DictionaryArray::<Int32Type>::try_new(
                    keys.finish(),
                    Arc::new(StringArray::from(values)),
                )
                .map_err(|e| format!("dictionary keys out of range: {e}"))?;
                Ok((
                    cat_field(&field_name, level == messages::ViewLevel::Ordinal),
                    Arc::new(arr),
                ))
            }
        }
    }
}

fn push_keys(dict: &DictionaryArray<Int32Type>, raw: &mut Vec<Option<i32>>) {
    let keys = dict.keys();
    for i in 0..dict.len() {
        if dict.is_null(i) {
            raw.push(None);
        } else {
            raw.push(Some(keys.value(i)));
        }
    }
}

fn cat_field(name: &str, ordered: bool) -> Field {
    Field::new(
        name,
        DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
        true,
    )
    .with_dict_is_ordered(ordered)
}

fn dict_of(array: &dyn Array) -> Result<&DictionaryArray<Int32Type>, String> {
    array
        .as_any()
        .downcast_ref::<DictionaryArray<Int32Type>>()
        .ok_or_else(|| "stored categorical column is not a dictionary".to_string())
}

/// DISPLAY name → field index: `jasp:display_name` field metadata first, canonical-name
/// fallback — arrowview's §24.4 resolution, same rule (one convention, no copies).
fn resolve_column(schema: &SchemaRef, display: &str) -> Result<usize, String> {
    schema
        .fields()
        .iter()
        .position(|f| {
            f.metadata().get("jasp:display_name").map(String::as_str) == Some(display)
                || f.name() == display
        })
        .ok_or_else(|| format!("unknown column '{display}'"))
}

/// token = lowercase hex of the display name's UTF-8 bytes (AV6: name-derived, stable
/// under insert/reorder — index-derived tokens would churn every view hash).
fn hex_token(name: &str) -> String {
    name.bytes().map(|b| format!("{b:02x}")).collect()
}

// ── The system level-string format (f64 → nominal/ordinal level labels) ──────

/// Render an f64 as a level string — **system law, not a language's cosmetics**
/// (AV4: the worker owns casts; legacy JASP cast in C++, and future runner
/// families must not inherit anyone's formatting folklore). The contract:
///
/// 1. **15 significant digits, trailing zeros trimmed** (`%.15g` grouping):
///    values agreeing at 15 significant digits are ONE category — deliberate,
///    documented semantics (they are indistinguishable at display precision;
///    R/classic have always grouped this way). Consequence: cast code must
///    DEDUPE ON THE RENDERED STRING (factors may never carry duplicate levels).
/// 2. **Shape — the C `%g` range rule**: scientific iff the decimal exponent is
///    < −4 or ≥ 15, otherwise fixed. Exponents explicit-signed, ≥2 digits
///    (`1e+15`, `1e-05`).
/// 3. `±0.0` → `"0"`; locale-free by design — level strings are data identity
///    keys, not presentation (locale lives at ingest parsing and grid rendering).
///
/// The migration-era R fallback mirrors this in `jaspRunner/R/data.R`
/// (`jasp_level_string`, dying at slice D); the coercion-parity gate
/// (`refactor_design/tests/view_parity.R`) pins the two implementations together.
/// Every quirk stays ISOLATED HERE so a change is one-place.
// ── The system level-string format (f64 → nominal/ordinal level labels) ──────

/// Render an f64 as a level string — **system law, not a language's cosmetics**
/// (AV4: the worker owns casts; legacy JASP cast in C++, and future runner
/// families must not inherit anyone's formatting folklore). The contract is
/// plain C `%.15g` semantics — implementable as one `sprintf` in any language:
///
/// 1. **15 significant digits, trailing zeros trimmed** — the grouping rule is
///    deliberately lossy at the fringe: values that agree at 15 significant
///    digits are ONE category (they are indistinguishable at any display
///    precision anyway; R and classic JASP have always grouped this way).
///    Callers must therefore **dedupe on the rendered string** (see `CatF64`) —
///    a factor may never carry duplicate level names.
/// 2. **Shape** (C `%g` range rule): scientific iff the decimal exponent is
///    < −4 or ≥ 15; otherwise fixed. Exponents explicit-signed, ≥2 digits
///    (`1e+15`, `1e-05`).
/// 3. `±0.0` → `"0"`; locale-free by design — level strings are data identity
///    keys, not presentation (locale is a display-time concern: the grid
///    already re-renders per viewer via `ViewRender`; results-table label
///    localization, if ever, is a frontend render concern — design doc D10).
///
/// The migration-era R fallback mirrors this in `jaspRunner/R/data.R`
/// (`jasp_level_string` — literally one `sprintf`; dying at slice D); the
/// coercion-parity gate (`refactor_design/tests/view_parity.R`) pins the two
/// implementations together. Every quirk stays ISOLATED HERE so a change is
/// one-place.
pub(crate) fn level_string(v: f64) -> String {
    if v == 0.0 {
        return "0".to_string(); // ±0.0 canonicalize to "0"
    }
    let neg = v < 0.0;
    let a = v.abs();
    if !a.is_finite() {
        // Unreachable from the casts (non-finite is nulled before formatting), kept
        // total for safety.
        return match (a.is_nan(), neg) {
            (true, _) => "NaN".to_string(),
            (false, true) => "-Inf".to_string(),
            (false, false) => "Inf".to_string(),
        };
    }
    // 15 significant digits in scientific form, then re-shape per the %g rule.
    let sci = format!("{:.*e}", 14, a);
    let (mant, exp_s) = sci.split_once('e').expect("{:e} always has an exponent");
    let exp: i32 = exp_s.parse().expect("{:e} exponent is an integer");
    let mut dg: String = mant.chars().filter(|c| *c != '.').collect();
    while dg.len() > 1 && dg.ends_with('0') {
        dg.pop(); // trailing zeros are not significant
    }

    let scientific = || -> String {
        let mantissa = if dg.len() == 1 {
            dg.to_string()
        } else {
            format!("{}.{}", &dg[..1], &dg[1..])
        };
        let sign = if exp < 0 { '-' } else { '+' };
        format!("{mantissa}e{sign}{:02}", exp.abs())
    };
    let fixed = || -> String {
        if exp >= 0 {
            let int_len = (exp + 1) as usize;
            if dg.len() <= int_len {
                // whole number, zero-padded to its magnitude: 2, 15, 123456789012345
                let mut s = dg.to_string();
                while s.len() < int_len {
                    s.push('0');
                }
                s
            } else {
                // mixed magnitude: the point sits after exp+1 digits — 1.5, 12.5
                format!("{}.{}", &dg[..int_len], &dg[int_len..])
            }
        } else {
            format!("0.{}{}", "0".repeat((-exp - 1) as usize), dg)
        }
    };

    // C %g at precision 15: scientific iff exponent < -4 or >= 15.
    let body = if exp < -4 || exp >= 15 {
        scientific()
    } else {
        fixed()
    };
    if neg { format!("-{body}") } else { body }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use arrow::array::Int32Array;
    use serde_json::Value;

    // The fixture base cache (§8.3 shapes): four columns, five rows.
    //
    //   score  Float64        display "Score (points)"  values [1.5, null, 3.0, NaN, 2.0]
    //   group  Dictionary     display "Group"           keys [0,1,1,null,0] values ["ctrl","trt"]
    //                        labels {"ctrl":"Control"} (sparse overlay)
    //   level  Dictionary ord display "Level"          keys [0,2,1,null,1] values ["low","high","med"]
    //   code   Dictionary     display "Code"           keys [0,1,null,2,0] values ["10","20","30"]
    //
    // Every matrix cell and both null paths (float null, dict null, NaN, text-as-scale)
    // are reachable from this one fixture.
    fn write_base(dir: &std::path::Path) -> String {
        let mut score = Float64Builder::new();
        for v in [Some(1.5), None, Some(3.0), Some(f64::NAN), Some(2.0)] {
            score.append_option(v);
        }
        let score = score.finish();

        let group_values = Arc::new(StringArray::from(vec!["ctrl", "trt"]));
        let mut group_keys = Int32Builder::new();
        for k in [Some(0), Some(1), Some(1), None, Some(0)] {
            group_keys.append_option(k);
        }
        let group =
            DictionaryArray::<Int32Type>::try_new(group_keys.finish(), group_values).unwrap();

        let level_values = Arc::new(StringArray::from(vec!["low", "high", "med"]));
        let mut level_keys = Int32Builder::new();
        for k in [Some(0), Some(2), Some(1), None, Some(1)] {
            level_keys.append_option(k);
        }
        let level =
            DictionaryArray::<Int32Type>::try_new(level_keys.finish(), level_values).unwrap();

        let code_values = Arc::new(StringArray::from(vec!["10", "20", "30"]));
        let mut code_keys = Int32Builder::new();
        for k in [Some(0), Some(1), None, Some(2), Some(0)] {
            code_keys.append_option(k);
        }
        let code = DictionaryArray::<Int32Type>::try_new(code_keys.finish(), code_values).unwrap();

        let f_score = Field::new("score", DataType::Float64, true).with_metadata(
            vec![(
                "jasp:display_name".to_string(),
                "Score (points)".to_string(),
            )]
            .into_iter()
            .collect(),
        );
        let f_group = Field::new(
            "group",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            true,
        )
        .with_metadata(
            vec![
                ("jasp:display_name".to_string(), "Group".to_string()),
                (
                    "jasp:labels".to_string(),
                    r#"{"ctrl":"Control"}"#.to_string(),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let f_level = Field::new(
            "level",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            true,
        )
        .with_dict_is_ordered(true)
        .with_metadata(
            vec![("jasp:display_name".to_string(), "Level".to_string())]
                .into_iter()
                .collect(),
        );
        let f_code = Field::new(
            "code",
            DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
            true,
        )
        .with_metadata(
            vec![("jasp:display_name".to_string(), "Code".to_string())]
                .into_iter()
                .collect(),
        );

        let schema = Schema::new(vec![f_score, f_group, f_level, f_code]);
        let batch = RecordBatch::try_new(
            Arc::new(schema.clone()),
            vec![
                Arc::new(score),
                Arc::new(group),
                Arc::new(level),
                Arc::new(code),
            ],
        )
        .unwrap();
        let path = dir.join("base.arrow");
        let mut w =
            csv2arrow::make_feather_writer(path.to_str().unwrap(), &Arc::new(schema)).unwrap();
        w.write(&batch).unwrap();
        w.finish().unwrap();
        path.to_string_lossy().into_owned()
    }

    fn fill(source: &str, target: &str, spec: messages::ViewSpec) -> messages::CacheFill {
        messages::CacheFill {
            view_id: messages::view_id(&spec, 7),
            spec,
            source: source.to_string(),
            target: target.to_string(),
            base_revision: 7,
        }
    }

    fn spec(columns: Vec<(&str, messages::ViewLevel)>) -> messages::ViewSpec {
        messages::ViewSpec {
            dataset_id: "ds-1".into(),
            columns: Some(
                columns
                    .into_iter()
                    .map(|(name, as_type)| messages::ViewColumn {
                        name: name.into(),
                        as_type,
                    })
                    .collect(),
            ),
            filter: None,
            all: false,
        }
    }

    /// Read the built view: (schema, the single batch).
    fn read_view(target: &str) -> (SchemaRef, RecordBatch) {
        let reader = FileReader::try_new(File::open(target).unwrap(), None).unwrap();
        let schema = reader.schema();
        let batch = reader.into_iter().next().unwrap().unwrap();
        (schema, batch)
    }

    fn f64s(batch: &RecordBatch, name: &str) -> Vec<Option<f64>> {
        let col = batch
            .column(batch.schema().index_of(name).unwrap())
            .as_any()
            .downcast_ref::<Float64Array>()
            .unwrap();
        (0..col.len())
            .map(|i| {
                if col.is_null(i) {
                    None
                } else {
                    Some(col.value(i))
                }
            })
            .collect()
    }

    /// NaN-aware equality (NaN survives the f64→scale copy — `assert_eq!` on f64
    /// can't see it as equal to itself).
    fn same_f64s(a: &[Option<f64>], b: &[Option<f64>]) -> bool {
        a.len() == b.len()
            && a.iter().zip(b).all(|(x, y)| match (x, y) {
                (None, None) => true,
                (Some(x), Some(y)) => x.to_bits() == y.to_bits(),
                _ => false,
            })
    }

    fn assert_f64s(batch: &RecordBatch, name: &str, want: &[Option<f64>]) {
        let got = f64s(batch, name);
        assert!(
            same_f64s(&got, want),
            "column {name}: got {got:?} want {want:?}"
        );
    }

    /// The dictionary of a categorical view column: (values, per-row keys, ordered flag).
    fn cats(batch: &RecordBatch, name: &str) -> (Vec<String>, Vec<Option<i32>>, bool) {
        let idx = batch.schema().index_of(name).unwrap();
        let col = batch
            .column(idx)
            .as_any()
            .downcast_ref::<DictionaryArray<Int32Type>>()
            .unwrap();
        let values = col.values().as_any().downcast_ref::<StringArray>().unwrap();
        let vals = (0..values.len())
            .map(|i| values.value(i).to_string())
            .collect();
        let keys = (0..col.len())
            .map(|i| {
                if col.is_null(i) {
                    None
                } else {
                    Some(col.keys().value(i))
                }
            })
            .collect();
        let ordered = batch.schema().field(idx).dict_is_ordered().unwrap_or(false);
        (vals, keys, ordered)
    }

    fn tmp(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("analysisview-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// THE determinism gate (AV8): same (spec, base_revision) ⇒ byte-identical
    /// rebuild — delete the blob, rebuild, compare every byte.
    #[test]
    fn determinism_gate_same_spec_same_bytes() {
        let dir = tmp("determinism");
        let source = write_base(&dir);
        let target = dir.join("view.arrow");
        let s = spec(vec![
            ("Score (points)", messages::ViewLevel::Scale),
            ("Group", messages::ViewLevel::Nominal),
            ("Code", messages::ViewLevel::Scale),
            ("Score (points)", messages::ViewLevel::Nominal),
        ]);
        let f = fill(&source, target.to_str().unwrap(), s);
        let bytes1 = build(&f).unwrap();
        let blob1 = std::fs::read(&target).unwrap();
        std::fs::remove_file(&target).unwrap();
        let bytes2 = build(&f).unwrap();
        let blob2 = std::fs::read(&target).unwrap();
        assert_eq!(bytes1, bytes2);
        assert_eq!(blob1, blob2, "rebuild must be byte-identical (AV8)");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The §8.3 matrix, cell by cell, on the fixture base.
    #[test]
    fn coercion_matrix() {
        let dir = tmp("matrix");
        let source = write_base(&dir);
        let target = dir.join("view.arrow");
        let s = spec(vec![
            ("Score (points)", messages::ViewLevel::Scale), // f64 → scale
            ("Score (points)", messages::ViewLevel::Nominal), // f64 → nominal
            ("Group", messages::ViewLevel::Nominal),        // dict → nominal (labels)
            ("Group", messages::ViewLevel::Ordinal),        // dict → ordinal (labels)
            ("Group", messages::ViewLevel::Scale),          // dict → scale, text → null
            ("Code", messages::ViewLevel::Scale),           // dict → scale, values parse
            ("Level", messages::ViewLevel::Ordinal),        // dict → ordinal, order kept
            ("Level", messages::ViewLevel::Nominal),        // dict → nominal, ordered INHERITED
        ]);
        build(&fill(&source, target.to_str().unwrap(), s)).unwrap();
        let (_schema, batch) = read_view(target.to_str().unwrap());

        // f64 → scale: verbatim — null stays null, NaN stays NaN (as.numeric keeps it).
        assert_f64s(
            &batch,
            "Score (points)__scale",
            &[Some(1.5), None, Some(3.0), Some(f64::NAN), Some(2.0)],
        );

        // f64 → nominal: finite levels numerically sorted via the system
        // level_string; non-finite (NaN) is MISSING (a null), never a category.
        {
            let (vals, keys, ordered) = cats(&batch, "Score (points)__nominal");
            assert_eq!(vals, vec!["1.5", "2", "3"]);
            assert_eq!(keys, vec![Some(0), None, Some(2), None, Some(1)]);
            assert!(!ordered);
        }

        // dict → nominal: label overlay applied ("ctrl"→"Control"), order preserved,
        // keys pass through, null stays null.
        {
            let (vals, keys, ordered) = cats(&batch, "Group__nominal");
            assert_eq!(vals, vec!["Control", "trt"]);
            assert_eq!(keys, vec![Some(0), Some(1), Some(1), None, Some(0)]);
            assert!(!ordered);
        }

        // dict → ordinal: same levels, ordered=1 (the ranking is the dictionary order).
        {
            let (vals, keys, ordered) = cats(&batch, "Group__ordinal");
            assert_eq!(vals, vec!["Control", "trt"]);
            assert_eq!(keys, vec![Some(0), Some(1), Some(1), None, Some(0)]);
            assert!(ordered);
        }

        // dict → scale, text values: nothing parses ⇒ all null (AV4 text-as-scale).
        assert_f64s(&batch, "Group__scale", &[None, None, None, None, None]);

        // dict → scale, numeric-coded values: the k values parsed once, rows indexed.
        assert_f64s(
            &batch,
            "Code__scale",
            &[Some(10.0), Some(20.0), None, Some(30.0), Some(10.0)],
        );

        // dict → ordinal passthrough of a base ordinal: ranking (dict order) kept.
        {
            let (vals, keys, ordered) = cats(&batch, "Level__ordinal");
            assert_eq!(vals, vec!["low", "high", "med"]);
            assert_eq!(keys, vec![Some(0), Some(2), Some(1), None, Some(1)]);
            assert!(ordered);
        }

        // dict → nominal of a base ORDINAL dictionary: nominal is UNORDERED, always
        // (system law — the wire vocabulary says "plain dictionary").
        {
            let (vals, keys, ordered) = cats(&batch, "Level__nominal");
            assert_eq!(vals, vec!["low", "high", "med"]);
            assert_eq!(keys, vec![Some(0), Some(2), Some(1), None, Some(1)]);
            assert!(
                !ordered,
                "nominal is never ordered, even from an ordered base"
            );
        }

        // The base-row index: 0-based, dense, never null.
        let base_row = batch
            .column(batch.schema().index_of(BASE_ROW_FIELD).unwrap())
            .as_any()
            .downcast_ref::<Int32Array>()
            .unwrap();
        assert_eq!(base_row.len(), 5);
        assert!((0..5).all(|i| base_row.value(i) == i as i32));
        assert_eq!(base_row.null_count(), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The `jasp:view` metadata block is the decode authority (AV5): view id, base
    /// revision, spec, rows, and the name-derived hex token map.
    #[test]
    fn artifact_metadata_is_the_protocol() {
        let dir = tmp("meta");
        let source = write_base(&dir);
        let target = dir.join("view.arrow");
        let s = spec(vec![("Group", messages::ViewLevel::Nominal)]);
        let f = fill(&source, target.to_str().unwrap(), s.clone());
        let expected_id = messages::view_id(&s, 7);
        build(&f).unwrap();
        let (schema, _batch) = read_view(target.to_str().unwrap());
        let meta: Value =
            serde_json::from_str(schema.metadata().get("jasp:view").unwrap()).unwrap();
        assert_eq!(meta["format_version"], messages::VIEW_FORMAT_VERSION);
        assert_eq!(meta["view_id"], expected_id);
        assert_eq!(meta["base_revision"], 7);
        assert_eq!(meta["dataset_id"], "ds-1");
        assert_eq!(meta["rows"], 5);
        assert_eq!(meta["base_row_column"], BASE_ROW_FIELD);
        assert_eq!(meta["token_map"]["Group"], hex_token("Group"));
        assert_eq!(
            meta["token_map"]["Group"],
            "47726f7570", // "Group" UTF-8 bytes, hex
        );
        assert_eq!(meta["spec"]["columns"][0]["as"], "nominal");
        // The spec round-trips through the metadata (it IS the hash input).
        let spec_back: messages::ViewSpec = serde_json::from_value(meta["spec"].clone()).unwrap();
        assert_eq!(spec_back, s);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Deterministic refusals — visible errors, never silent guesses.
    #[test]
    fn refuses_filter_unknown_duplicate_and_empty() {
        let dir = tmp("refuse");
        let source = write_base(&dir);
        let target = dir.join("view.arrow");

        let with_filter = messages::ViewSpec {
            filter: Some("some_mask".into()),
            ..spec(vec![("Group", messages::ViewLevel::Nominal)])
        };
        let err = build(&fill(&source, target.to_str().unwrap(), with_filter)).unwrap_err();
        assert!(err.contains("filters are not supported"), "{err}");

        let err = build(&fill(
            &source,
            target.to_str().unwrap(),
            spec(vec![("Nope", messages::ViewLevel::Scale)]),
        ))
        .unwrap_err();
        assert!(err.contains("unknown column 'Nope'"), "{err}");

        let err = build(&fill(
            &source,
            target.to_str().unwrap(),
            spec(vec![
                ("Group", messages::ViewLevel::Nominal),
                ("Group", messages::ViewLevel::Nominal),
            ]),
        ))
        .unwrap_err();
        assert!(err.contains("duplicate spec entry"), "{err}");

        let err = build(&fill(
            &source,
            target.to_str().unwrap(),
            messages::ViewSpec {
                columns: None,
                all: true,
                ..spec(vec![])
            },
        ))
        .unwrap_err();
        assert!(err.contains("pass-through"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The system level-string contract (plain %.15g: 15 significant digits,
    /// trailing zeros trimmed, %g range rule). These are re-judged end-to-end by
    /// the coercion-parity harness against the R mirror in jaspRunner/R/data.R.
    #[test]
    fn level_string_is_percent_15g() {
        assert_eq!(level_string(2.0), "2");
        assert_eq!(level_string(0.5), "0.5");
        assert_eq!(level_string(0.05), "0.05");
        assert_eq!(level_string(1.0 / 3.0), "0.333333333333333");
        assert_eq!(level_string(-0.25), "-0.25");
        assert_eq!(level_string(0.0), "0");
        assert_eq!(level_string(-0.0), "0");
        assert_eq!(level_string(1e20), "1e+20");
        assert_eq!(level_string(1e-5), "1e-05");
        assert_eq!(level_string(1e-4), "0.0001"); // exp -4: still fixed
        assert_eq!(level_string(1e14), "100000000000000"); // exp 14: still fixed
        assert_eq!(level_string(1e15), "1e+15"); // exp 15: scientific
        assert_eq!(level_string(120000.0), "120000");
        assert_eq!(level_string(0.001), "0.001");
        assert_eq!(level_string(123456789012345.0), "123456789012345");
        assert_eq!(level_string(f64::powi(2.0, 60)), "1.15292150460685e+18");
        assert_eq!(level_string(12.5), "12.5");
        assert_eq!(level_string(6.02214076e23), "6.02214076e+23"); // zeros trimmed
        // The documented %.15g GROUPING merges (fringe values agreeing at 15
        // significant digits become one category — the cast dedupes on the string):
        assert_eq!(level_string(0.1 + 0.2), "0.3");
        assert_eq!(level_string(0.1 + 0.2), level_string(0.3));
        assert_eq!(
            level_string(1234567890123456.0),
            level_string(1234567890123457.0)
        );
        assert_eq!(level_string(1234567890123456.0), "1.23456789012346e+15");
    }
}
