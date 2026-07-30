# jaspRunner/R/data.R
#
# Arrow/Feather dataset loading for the JASP runner — replaces the legacy
# rbridge_readDataSet / rbridge_getColumnData C++ bridge (see neo-jasp.md §5.3, §8.3).
#
# Single unified read path (decided): ONE batched C++ read materializes the requested columns,
# then per-column type conversion + a guarded O(k) label relabel is applied in R. Measured on
# R arrow 25.0.0 (refactor_design/poc/benchmark_*.R):
#   * common tall/narrow (1M x 10, sparse labels): ~34 ms
#   * wide, realistic subset (100 x 10,000 file, ~200 cols via col_select): ~100 ms
#   * wide, ALL 10,000 columns (rare extreme): ~2.3 s — per-column conversion dominates; analyses
#     request a column subset (col_select), so this extreme is avoided in practice.
# The realistic cases are negligible next to an analysis. A per-column read is ~10 ms faster on
# tall data, but we keep ONE code path (no shape heuristic) for simplicity.
# Filters are applied in Arrow C++ (Table$Filter) BEFORE the R conversion, so excluded rows are
# never materialized or type-converted — 2.3-4x faster than materializing all rows and subsetting
# in R, and even faster than no filter at high selectivity (benchmark_filter_application.R).
#
# Dependencies: arrow (brings tidyselect), jsonlite.

#' Overlay value labels on the k levels: label where set & non-empty, else the value itself.
#' Operates on the levels (length k), never the N rows.
label_or_value <- function(values, labels) {
  if (is.null(labels) || length(labels) == 0L) return(values)
  vapply(values, function(v) {
    l <- labels[[v]]
    if (!is.null(l) && nzchar(l)) unname(l) else v
  }, character(1))
}

#' Load the requested columns from a Feather (Arrow IPC + LZ4) cache file, each as its requested
#' type, with display labels applied and ordinals ordered.
#'
#' @param path         Path to the dataset's Feather cache file (orchestrator-owned; read-only).
#' @param columns_spec list of `list(name = <canonical name>, as = "scale"|"nominal"|"ordinal")`.
#' @param filters      optional names of stored boolean filter columns, ANDed into a row mask.
#' @return a data.frame of the requested columns (in `columns_spec` order).
read_jasp_data <- function(path, columns_spec, filters = NULL) {
  sel <- vapply(columns_spec, function(s) s$name, character(1))

  # ONE batched C++ read as a Table; col_select prunes to the requested columns (plus the filter
  # columns, if any) at the C++ level, so a wide file materializes only what is needed.
  need <- if (length(filters)) unique(c(sel, filters)) else sel
  tbl <- arrow::read_feather(path, as_data_frame = FALSE,
                             col_select = tidyselect::all_of(need))

  # Apply filters in Arrow C++ BEFORE converting to R, so excluded rows are never materialized or
  # type-converted. Validated 2.3-4x faster than materializing all rows and subsetting in R — and
  # even faster than NO filter at high selectivity, since it skips converting excluded rows (see
  # refactor_design/poc/benchmark_filter_application.R). Feather has no per-batch statistics, so
  # this saves CPU rather than disk I/O (true I/O pushdown would need Parquet row-group stats).
  if (!is.null(filters) && length(filters) > 0L) {
    mask <- NULL
    for (f in filters) {
      m <- tbl[[f]]$as_vector()                       # boolean column -> R logical (cheap)
      mask <- if (is.null(mask)) m else (mask & m)    # AND the filter columns
    }
    mask[is.na(mask)] <- FALSE                        # a filter that yields NA excludes the row
    tbl <- tbl$Filter(mask)                           # C++: keep only the passing rows
  }

  # The schema (footer) travels with the Table; parse each distinct labels JSON once.
  sch <- tbl$schema
  label_cache <- new.env(parent = emptyenv())
  labels_for <- function(name) {
    lj <- sch$GetFieldByName(name)$metadata[["jasp:labels"]]
    if (is.null(lj) || !nzchar(lj)) return(NULL)            # value == label; nothing to map
    if (is.null(label_cache[[lj]])) label_cache[[lj]] <- jsonlite::fromJSON(lj)
    label_cache[[lj]]
  }

  df <- as.data.frame(tbl)                             # convert ONLY the (filtered) rows to R
  out <- as.list(df)
  for (s in columns_spec) {
    vals <- out[[s$name]]
    if (is.factor(vals)) {                                  # categorical (Arrow dictionary)
      if (s$as == "scale") {
        # read-as-scale returns the VALUES: parse the k levels once, index the N rows.
        out[[s$name]] <- as.numeric(levels(vals))[as.integer(vals)]
      } else {
        # Relabel only where a label overlay exists. When value == label (the common case: string
        # categoricals, unlabelled numeric codes) the levels are already the display values; SKIP
        # the assignment, because `levels(vals) <-` forces a copy-on-modify of the N codes
        # (~7 ms/col at 1M rows) even when it changes nothing (~3.6x on tall sparse data). Never
        # rebuild with factor(vals, levels=levels(vals), labels=lab) — that re-matches all N rows.
        labels <- labels_for(s$name)
        if (!is.null(labels) && length(labels) > 0L)
          levels(vals) <- label_or_value(levels(vals), labels)
        if (s$as == "ordinal" && !is.ordered(vals))
          class(vals) <- c("ordered", "factor")              # O(1): Arrow `ordered` flag => ordinal
        out[[s$name]] <- vals
      }
    } else {                                                # scale (float64) column
      out[[s$name]] <- switch(s$as,
        scale   = vals,                                      # already numeric — no factor round-trip
        nominal = factor(vals),                              # R sorts numeric levels numerically
        ordinal = ordered(factor(vals)))
    }
  }
  out <- out[sel]                                            # drop filter cols, keep requested order

  structure(out, class = "data.frame", row.names = c(NA_integer_, nrow(df)))
}
