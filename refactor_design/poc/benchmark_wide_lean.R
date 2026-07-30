#!/usr/bin/env Rscript
# Wide-data fix, take 2 (lean).
#
# Take 1 showed the overlay loop is itself O(ncol^2): 10,000 `df[[nm]] <-` data.frame
# assignments (each re-validates the frame) + 10,000 jsonlite::fromJSON calls on the same
# labels string. This version removes both:
#   - batched read with col_select  -> ONE C++ call, only the NEEDED columns materialized
#   - metadata via GetFieldByName per NEEDED column -> O(num_needed), not O(ncol_in_file)
#   - parse each unique labels JSON ONCE (cached)
#   - assemble the result as a plain list, build the data.frame ONCE at the end
#
# The point: read_jasp_data takes a columns_spec (the columns the analysis actually needs).
# For a wide dataset that list is short, so with C++ pruning + lean overlay the load is fast.
# The ~13 s catastrophe only happens if you naively read ALL 10,000 columns.

suppressMessages(library(arrow))
cat("=== wide-data fix (lean): prune at C++ + cached labels + list assembly ===\n")
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

# per-column impl (8.3 as documented) -- the slow one
read_percol <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) { s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    pj <- function(meta) if (!is.null(meta[["jasp:labels"]]) && nzchar(meta[["jasp:labels"]])) jsonlite::fromJSON(meta[["jasp:labels"]]) else NULL
    if (inherits(field$type, "DictionaryType")) { f <- col$as_vector()
      if (s$as == "scale") out[[i]] <- as.numeric(levels(f))[as.integer(f)]
      else { levels(f) <- label_or_value(levels(f), pj(field$metadata))
        if (s$as == "ordinal") class(f) <- c("ordered","factor"); out[[i]] <- f }
    } else { v <- col$as_vector(); out[[i]] <- switch(s$as, scale = v, nominal = factor(v), ordinal = ordered(factor(v))) } }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}

# LEAN: prune at C++, per-needed-column metadata, cached label parse, list assembly.
read_lean <- function(path, spec) {
  sel <- vapply(spec, function(s) s$name, character(1))
  df <- suppressWarnings(read_feather(path, as_data_frame = TRUE, col_select = tidyselect::all_of(sel)))  # ONE batched C++ call
  sch <- read_feather(path, as_data_frame = FALSE)$schema
  # per-needed-column metadata (O(num_needed) R6 calls) + cache parsed labels by JSON string
  label_cache <- new.env(parent = emptyenv())
  labels_for <- function(nm) {
    js <- sch$GetFieldByName(nm)$metadata[["jasp:labels"]]
    if (is.null(js) || !nzchar(js)) return(NULL)
    if (is.null(label_cache[[js]])) label_cache[[js]] <- jsonlite::fromJSON(js)
    label_cache[[js]]
  }
  out <- as.list(df)                       # plain list; element assignment is O(1)
  for (i in seq_along(spec)) {
    s <- spec[[i]]; col <- out[[s$name]]
    if (is.factor(col)) {
      if (s$as == "scale") out[[s$name]] <- as.numeric(levels(col))[as.integer(col)]
      else { levels(col) <- label_or_value(levels(col), labels_for(s$name))
        if (s$as == "ordinal" && !is.ordered(col)) class(col) <- c("ordered","factor"); out[[s$name]] <- col }
    } else out[[s$name]] <- switch(s$as, scale = col, nominal = factor(col), ordinal = ordered(factor(col)))
  }
  structure(out, class = "data.frame", row.names = c(NA_integer_, nrow(df)))
}

# generators
gen_scalar <- function(nrow, ncol, dir) {
  set.seed(42); m <- matrix(rnorm(nrow * ncol, 100, 15), nrow = nrow)
  df <- as.data.frame(m); names(df) <- paste0("v", seq_len(ncol))
  fields <- lapply(names(df), function(nm) field(nm, float64(), metadata = list("jasp:display_name" = nm)))
  tbl <- do.call(Table$create, c(as.list(df), list(schema = do.call(schema, fields))))
  path <- file.path(dir, sprintf("scalar_%d_%d.arrow", nrow, ncol)); write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(names(df), function(nm) list(name = nm, as = "scale")))
}
gen_cat <- function(nrow, ncol, nlevels, ordered_flag, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels))
  labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl); lj <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)
  cols <- lapply(seq_len(ncol), function(j) { f <- factor(sample(seq_len(nlevels), nrow, replace = TRUE), levels = seq_len(nlevels)); if (ordered_flag) ordered(f) else f })
  nm <- paste0("c", seq_len(ncol)); names(cols) <- nm
  dt <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(nm, function(n) field(n, dt, metadata = list("jasp:display_name" = n, "jasp:labels" = lj)))
  tbl <- do.call(Table$create, c(cols, list(schema = do.call(schema, fields))))
  path <- file.path(dir, sprintf("%s_%d_%d.arrow", if (ordered_flag) "ordinal" else "nominal", nrow, ncol)); write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = if (ordered_flag) "ordinal" else "nominal")))
}

dir <- file.path(tempdir(), "jasp_lean"); dir.create(dir, showWarnings = FALSE)

# correctness: lean == percol (data + per-column; ignore row.names representation)
cat("--- correctness (10 x 200 ordinal) ---\n")
g0 <- gen_cat(10, 200, 5, TRUE, dir)
a <- read_percol(g0$path, g0$spec); b <- read_lean(g0$path, g0$spec)
cat("  all.equal(check.attributes=FALSE):", isTRUE(all.equal(a, b, check.attributes = FALSE)), "\n")
cat("  every column identical:           ", all(vapply(names(a), function(nm) identical(a[[nm]], b[[nm]]), logical(1))), "\n")
cat("  column order identical:           ", identical(names(a), names(b)), "\n\n")

# WIDE 100 x 10000
cat("Generating 100 x 10,000 (not timed)...\n")
gw_s <- gen_scalar(100, 10000, dir); gw_n <- gen_cat(100, 10000, 5, FALSE, dir); gw_o <- gen_cat(100, 10000, 5, TRUE, dir)
cat("  done\n\n")

wide <- function(label, g) {
  s5 <- g$spec[1:5]; s100 <- g$spec[1:100]
  t_perc  <- time_ms(function() read_percol(g$path, g$spec), reps = 3)
  t_full  <- time_ms(function() read_lean(g$path, g$spec), reps = 3)
  t_100   <- time_ms(function() read_lean(g$path, s100))
  t_5     <- time_ms(function() read_lean(g$path, s5))
  cat(sprintf("%-26s\n", label))
  cat(sprintf("    per-column, all 10,000 (8.3) : %10.2f ms\n", t_perc))
  cat(sprintf("    lean, all 10,000             : %10.2f ms   (%.1fx vs per-column)\n", t_full, t_perc / t_full))
  cat(sprintf("    lean, 100 cols               : %10.2f ms\n", t_100))
  cat(sprintf("    lean, 5 cols                 : %10.2f ms   (%.0fx vs all 10,000)\n\n", t_5, t_full / t_5))
}
cat("--- WIDE: 100 rows x 10,000 columns (median ms) ---\n\n")
wide("scalar 100x10000", gw_s)
wide("nominal 100x10000 (5lv)", gw_n)
wide("ordinal 100x10000 (5lv)", gw_o)
unlink(c(gw_s$path, gw_n$path, gw_o$path, g0$path)); gc(FALSE)

# TALL 1M x 10 -- confirm lean is fine on the normal shape too
cat("Generating 1,000,000 x 10 (not timed)...\n")
gt_n <- gen_cat(1e6, 10, 200, FALSE, dir); gt_o <- gen_cat(1e6, 10, 50, TRUE, dir)
cat("  done\n\n")
cat("--- TALL: 1,000,000 rows x 10 columns (median ms) ---\n\n")
for (g in list(gt_n, gt_o)) {
  tp <- time_ms(function() read_percol(g$path, g$spec), reps = 5)
  tl <- time_ms(function() read_lean(g$path, g$spec), reps = 5)
  cat(sprintf("  per-column=%8.2f ms | lean=%8.2f ms (%.1fx)\n", tp, tl, tp / tl))
}
unlink(dir, recursive = TRUE); cat("\nDone.\n")
