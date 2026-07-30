#!/usr/bin/env Rscript
# Wide-short diagnosis + fix.
#
# benchmark_wide_short.R showed the per-column read_jasp_data loop is CATASTROPHIC
# on wide data: ~13 s for 100 x 10,000, while Arrow's own read_feather(as_data_frame=TRUE)
# of the SAME table is ~89 ms. The cost is not the relabel and not GetFieldByName --
# it is the 10,000 R->C++ round-trips (tbl[[name]] + $as_vector() + schema calls per
# column, ~1 ms each).
#
# The fix for wide data: let Arrow materialize the whole table in ONE batched C++ call
# (read_feather(as_data_frame=TRUE)), then apply the O(k) relabel / scale conversion in R
# on the resulting data.frame -- no per-column Arrow round-trips. Column pruning uses
# read_feather(col_select=...) at the C++ level.
#
# This script validates that fix and also checks it on TALL data (1M x 10) to confirm it
# is a good universal implementation for jaspRunner::data.R.

suppressMessages(library(arrow))
cat("=== wide-data fix: batched as_data_frame + O(k) overlay ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

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

# the per-column impl (8.3 as documented) -- the slow one on wide data
read_percol <- function(path, spec) {
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

# RECOMMENDED: ONE batched C++ read + O(k) overlay in R. col_names prunes at the C++ level.
read_batched <- function(path, spec, col_names = NULL) {
  sch <- read_feather(path, as_data_frame = FALSE)$schema          # cheap footer read for jasp:labels
  sel <- if (is.null(col_names)) vapply(spec, function(s) s$name, character(1)) else col_names
  df  <- read_feather(path, as_data_frame = TRUE, col_select = sel) # ONE batched C++ conversion
  meta_by_name <- stats::setNames(lapply(sch$fields, function(f) f$metadata),
                                  vapply(sch$fields, function(f) f$name, character(1)))
  want <- stats::setNames(lapply(spec, function(s) s$as), vapply(spec, function(s) s$name, character(1)))
  for (nm in names(df)) {
    as <- want[[nm]]; if (is.null(as)) next
    col <- df[[nm]]
    if (is.factor(col)) {
      if (as == "scale") df[[nm]] <- as.numeric(levels(col))[as.integer(col)]
      else { levels(col) <- label_or_value(levels(col), parse_labels(meta_by_name[[nm]]))
        if (as == "ordinal" && !is.ordered(col)) class(col) <- c("ordered","factor"); df[[nm]] <- col }
    } else df[[nm]] <- switch(as, scale = col, nominal = factor(col), ordinal = ordered(factor(col)))
  }
  df
}

# ── generators ─────────────────────────────────────────────────────────────
gen_wide_scalar <- function(nrow, ncol, dir) {
  set.seed(42); m <- matrix(rnorm(nrow * ncol, 100, 15), nrow = nrow)
  df <- as.data.frame(m); names(df) <- paste0("v", seq_len(ncol))
  fields <- lapply(names(df), function(nm) field(nm, float64(), metadata = list("jasp:display_name" = nm)))
  tbl <- do.call(Table$create, c(as.list(df), list(schema = do.call(schema, fields))))
  path <- file.path(dir, sprintf("scalar_%d_%d.arrow", nrow, ncol)); write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(names(df), function(nm) list(name = nm, as = "scale")))
}
gen_wide_cat <- function(nrow, ncol, nlevels, ordered_flag, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels))
  labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl); lj <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)
  cols <- lapply(seq_len(ncol), function(j) { f <- factor(sample(seq_len(nlevels), nrow, replace = TRUE), levels = seq_len(nlevels)); if (ordered_flag) ordered(f) else f })
  nm <- paste0("c", seq_len(ncol)); names(cols) <- nm
  dt <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(nm, function(n) field(n, dt, metadata = list("jasp:display_name" = n, "jasp:labels" = lj)))
  tbl <- do.call(Table$create, c(cols, list(schema = do.call(schema, fields))))
  tag <- if (ordered_flag) "ordinal" else "nominal"
  path <- file.path(dir, sprintf("%s_%d_%d.arrow", tag, nrow, ncol)); write_feather(tbl, path, compression = "lz4")
  as_type <- if (ordered_flag) "ordinal" else "nominal"
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = as_type)))
}

dir <- file.path(tempdir(), "jasp_widefix"); dir.create(dir, showWarnings = FALSE)

# correctness on a small wide table (10 x 200): batched == percol
cat("--- correctness (10 x 200 ordinal): batched == percol ---\n")
gc0 <- gen_wide_cat(10, 200, 5, TRUE, dir)
cat("  identical:", identical(read_percol(gc0$path, gc0$spec), read_batched(gc0$path, gc0$spec)), "\n")
cat("  prune-5 row count ok:", nrow(read_batched(gc0$path, gc0$spec[1:5], col_names = vapply(gc0$spec[1:5], function(s) s$name, character(1)))) == 10, "\n\n")

# WIDE: 100 x 10000
cat("Generating 100 x 10,000 (not timed)...\n")
gw_s <- gen_wide_scalar(100, 10000, dir)
gw_n <- gen_wide_cat(100, 10000, 5, FALSE, dir)
gw_o <- gen_wide_cat(100, 10000, 5, TRUE, dir)
cat("  done\n\n")

wide <- function(label, g) {
  all5 <- vapply(g$spec[1:5], function(s) s$name, character(1))
  t_adf    <- time_ms(function() read_feather(g$path, as_data_frame = TRUE))
  t_perc   <- time_ms(function() read_percol(g$path, g$spec), reps = 3)
  t_batch  <- time_ms(function() read_batched(g$path, g$spec))
  t_prune  <- time_ms(function() read_batched(g$path, g$spec[1:5], col_names = all5))
  cat(sprintf("%-26s\n", label))
  cat(sprintf("    arrow->df (batched read)   : %10.2f ms\n", t_adf))
  cat(sprintf("    per-column loop (8.3)      : %10.2f ms\n", t_perc))
  cat(sprintf("    batched + O(k) overlay     : %10.2f ms   <- %.0fx vs per-column\n", t_batch, t_perc / t_batch))
  cat(sprintf("    batched + overlay, 5 cols  : %10.2f ms   <- %.0fx vs all 10,000\n\n", t_prune, t_batch / t_prune))
}
cat("--- WIDE: 100 rows x 10,000 columns (median ms) ---\n\n")
wide("scalar 100x10000", gw_s)
wide("nominal 100x10000 (5lv)", gw_n)
wide("ordinal 100x10000 (5lv)", gw_o)
unlink(c(gw_s$path, gw_n$path, gw_o$path, gc0$path)); gc(FALSE)

# TALL: 1M x 10 -- confirm batched is also good on the tall shape
cat("Generating 1,000,000 x 10 (not timed)...\n")
gt_n <- gen_wide_cat(1e6, 10, 200, FALSE, dir)
gt_o <- gen_wide_cat(1e6, 10, 50, TRUE, dir)
cat("  done\n\n")
tall <- function(label, g) {
  t_perc  <- time_ms(function() read_percol(g$path, g$spec), reps = 5)
  t_batch <- time_ms(function() read_batched(g$path, g$spec), reps = 5)
  cat(sprintf("%-26s per-column=%8.2f ms | batched+overlay=%8.2f ms (%.1fx)\n",
              label, t_perc, t_batch, t_perc / t_batch))
}
cat("--- TALL: 1,000,000 rows x 10 columns (median ms) ---\n\n")
tall("nominal 1Mx10 (200lv)", gt_n)
tall("ordinal 1Mx10 (50lv)", gt_o)

unlink(dir, recursive = TRUE); cat("\nDone.\n")
