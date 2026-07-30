#!/usr/bin/env Rscript
# Benchmark: WIDE-SHORT data (100 rows x 10,000 columns).
#
# This shape flips the bottleneck. With only 100 rows, the per-row conversion is
# trivial; what dominates is the PER-COLUMN overhead of read_jasp_data -- the loop
# runs 10,000 times, each calling tbl$schema$GetFieldByName(name) (a possible linear
# scan over 10,000 fields) plus an as_vector() round-trip. It also makes column
# pruning dramatic (an analysis rarely needs all 10,000 columns).
#
# We compare:
#   open            : read_feather(as_data_frame=FALSE)             (mmap; note the big footer)
#   arrow->df       : read_feather(as_data_frame=TRUE)              (Arrow batch C++)
#   jasp fast       : O(k) relabel, GetFieldByName per column       (the 8.3 impl)
#   jasp lookup     : O(k) relabel, name->metadata map built ONCE   (wide-data optimization)
#   jasp naive      : factor() rebuild per column (for reference)
#   prune 5 / 10000 : read only 5 columns via the lookup path
#
# (Per the design decision: the R copy-on-modify cost is accepted for now; relabelling
#  inside Arrow's C++ is deferred. This benchmark characterizes the wide-data path.)

suppressMessages(library(arrow))
cat("=== Arrow load benchmark: wide-short (100 rows x 10,000 cols) ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

NROW <- 100L; NCOL <- 10000L
time_ms <- function(f, reps = 7, warmup = 1) {
  for (i in seq_len(warmup)) { invisible(f()); gc(FALSE) }
  ts <- numeric(reps)
  for (i in seq_len(reps)) { gc(FALSE); t0 <- Sys.time(); invisible(f())
    ts[i] <- as.numeric(Sys.time() - t0, units = "secs") * 1000 }
  stats::median(ts)
}
label_or_value <- function(values, labels) {
  if (is.null(labels) || length(labels) == 0L) return(values)
  vapply(values, function(v) { l <- labels[[v]]; if (!is.null(l) && nzchar(l)) unname(l) else v }, character(1))
}
parse_labels <- function(meta) {
  if (!is.null(meta[["jasp:labels"]]) && nzchar(meta[["jasp:labels"]])) jsonlite::fromJSON(meta[["jasp:labels"]]) else NULL
}

# ── read_jasp variants ────────────────────────────────────────────────────
# fast: O(k) relabel, GetFieldByName per column (the documented 8.3 impl)
read_fast <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) { s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) { f <- col$as_vector()
      if (s$as == "scale") out[[i]] <- as.numeric(levels(f))[as.integer(f)]
      else { levels(f) <- label_or_value(levels(f), parse_labels(field$metadata))
        if (s$as == "ordinal") class(f) <- c("ordered","factor"); out[[i]] <- f }
    } else { v <- col$as_vector(); out[[i]] <- switch(s$as, scale = v, nominal = factor(v), ordinal = ordered(factor(v))) } }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}
# lookup: build name -> metadata ONCE (O(ncol)), then O(1) per column
read_lookup <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows; sch <- tbl$schema
  flds <- sch$fields
  is_dict <- vapply(flds, function(f) inherits(f$type, "DictionaryType"), logical(1))
  meta_by_name <- stats::setNames(lapply(flds, function(f) f$metadata), vapply(flds, function(f) f$name, character(1)))
  dict_by_name <- stats::setNames(is_dict, vapply(flds, function(f) f$name, character(1)))
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) { s <- spec[[i]]; col <- tbl[[s$name]]
    if (isTRUE(dict_by_name[[s$name]])) { f <- col$as_vector()
      if (s$as == "scale") out[[i]] <- as.numeric(levels(f))[as.integer(f)]
      else { levels(f) <- label_or_value(levels(f), parse_labels(meta_by_name[[s$name]]))
        if (s$as == "ordinal") class(f) <- c("ordered","factor"); out[[i]] <- f }
    } else { v <- col$as_vector(); out[[i]] <- switch(s$as, scale = v, nominal = factor(v), ordinal = ordered(factor(v))) } }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}
# naive: factor() rebuild per column (reference)
read_naive <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) { s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) { f <- col$as_vector(); lab <- label_or_value(levels(f), parse_labels(field$metadata))
      out[[i]] <- switch(s$as, scale = as.numeric(levels(f))[as.integer(f)],
        nominal = factor(f, levels = levels(f), labels = lab), ordinal = ordered(factor(f, levels = levels(f), labels = lab)))
    } else { v <- col$as_vector(); out[[i]] <- switch(s$as, scale = v, nominal = factor(v), ordinal = ordered(factor(v))) } }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}

# ── generators (100 x 10000) ───────────────────────────────────────────────
gen_scalar <- function(dir) {
  set.seed(42); m <- matrix(rnorm(NROW * NCOL, 100, 15), nrow = NROW)
  df <- as.data.frame(m); names(df) <- paste0("v", seq_len(NCOL))
  fields <- lapply(names(df), function(nm) field(nm, float64(), metadata = list("jasp:display_name" = nm)))
  tbl <- do.call(Table$create, c(as.list(df), list(schema = do.call(schema, fields))))
  path <- file.path(dir, "wide_scalar.arrow"); write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(names(df), function(nm) list(name = nm, as = "scale")))
}
gen_categorical <- function(nlevels, ordered_flag, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels))
  labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl); lj <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)
  cols <- lapply(seq_len(NCOL), function(j) { f <- factor(sample(seq_len(nlevels), NROW, replace = TRUE), levels = seq_len(nlevels)); if (ordered_flag) ordered(f) else f })
  nm <- paste0("c", seq_len(NCOL)); names(cols) <- nm
  dt <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(nm, function(n) field(n, dt, metadata = list("jasp:display_name" = n, "jasp:labels" = lj)))
  tbl <- do.call(Table$create, c(cols, list(schema = do.call(schema, fields))))
  tag <- if (ordered_flag) "wide_ordinal" else "wide_nominal"
  path <- file.path(dir, sprintf("%s.arrow", tag)); write_feather(tbl, path, compression = "lz4")
  as_type <- if (ordered_flag) "ordinal" else "nominal"
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = as_type)))
}

dir <- file.path(tempdir(), "jasp_wide"); dir.create(dir, showWarnings = FALSE)
cat("Generating 100 x 10,000 datasets (not timed)...\n")
gs <- gen_scalar(dir); cat("  scalar done\n")
gn <- gen_categorical(5, FALSE, dir); cat("  nominal (5 levels) done\n")
go <- gen_categorical(5, TRUE,  dir); cat("  ordinal (5 levels) done\n\n")

# correctness: lookup == fast == naive
cat("--- correctness: lookup == fast == naive ---\n")
for (g in list(gn, go)) {
  cat(sprintf("  identical(fast, lookup): %s | identical(fast, naive): %s\n",
              identical(read_fast(g$path, g$spec), read_lookup(g$path, g$spec)),
              identical(read_fast(g$path, g$spec), read_naive(g$path, g$spec))))
}
cat("\n")

run_one <- function(label, g) {
  spec <- g$spec; spec5 <- spec[1:5]
  fsize <- file.info(g$path)$size / 1024
  t_open   <- time_ms(function() read_feather(g$path, as_data_frame = FALSE))
  t_adf    <- time_ms(function() read_feather(g$path, as_data_frame = TRUE))
  t_fast   <- time_ms(function() read_fast(g$path, spec))
  t_lookup <- time_ms(function() read_lookup(g$path, spec))
  t_naive  <- time_ms(function() read_naive(g$path, spec), reps = 3)
  t_prune  <- time_ms(function() read_lookup(g$path, spec5))
  cat(sprintf("%-22s file=%8.0f KB\n", label, fsize))
  cat(sprintf("    open(mmap)          : %9.2f ms\n", t_open))
  cat(sprintf("    arrow->df           : %9.2f ms\n", t_adf))
  cat(sprintf("    jasp fast (per-col) : %9.2f ms\n", t_fast))
  cat(sprintf("    jasp lookup (1 map) : %9.2f ms   <- %.1fx vs per-col\n", t_lookup, t_fast / t_lookup))
  cat(sprintf("    jasp naive          : %9.2f ms\n", t_naive))
  cat(sprintf("    prune 5 / 10000     : %9.2f ms   <- %.0fx vs all\n\n", t_prune, t_fast / t_prune))
}

cat("--- Benchmark: 100 rows x 10,000 columns (median ms) ---\n\n")
run_one("scalar 100x10000", gs)
run_one("nominal 100x10000 (5lv)", gn)
run_one("ordinal 100x10000 (5lv)", go)

unlink(dir, recursive = TRUE); cat("Done.\n")
