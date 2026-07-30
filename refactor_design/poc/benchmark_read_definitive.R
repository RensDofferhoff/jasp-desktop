#!/usr/bin/env Rscript
# DEFINITIVE head-to-head of read_jasp_data implementations, to choose the
# jaspRunner::data.R approach. All four produce RESULT-IDENTICAL output; they
# differ only in how the R factors are materialized.
#
#   naive     : 8.3 reference -- factor(f, levels=levels(f), labels=lab)  (O(N) re-match)
#   percol    : per-column col$as_vector() + levels(f)<-lab (O(k)) + class<-ordered (O(1))
#   overlay   : as.data.frame(tbl) + O(k) overlay   (as.data.frame turns out per-column too)
#   batched   : read_feather(as_data_frame=TRUE)  [ONE batched C++ conversion, ~11ms]
#               + a cheap schema read for jasp:labels + O(k) overlay
#
# Expectation: batched ~ the floor (Arrow's native ~11ms + a little overlay).

suppressMessages(library(arrow))
cat("=== read_jasp_data: definitive comparison ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

time_ms <- function(f, reps, warmup = 1L) {
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
get_labels <- function(field) {
  meta <- field$metadata
  if (!is.null(meta[["jasp:labels"]]) && nzchar(meta[["jasp:labels"]])) jsonlite::fromJSON(meta[["jasp:labels"]]) else NULL
}
finish_factor <- function(col, as, labels) {           # O(k) relabel + O(1) ordered
  if (as == "scale") return(as.numeric(levels(col))[as.integer(col)])
  levels(col) <- label_or_value(levels(col), labels)
  if (as == "ordinal" && !is.ordered(col)) class(col) <- c("ordered", "factor")
  col
}

read_naive <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) { s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) { f <- col$as_vector(); lab <- label_or_value(levels(f), get_labels(field))
      out[[i]] <- switch(s$as, scale = as.numeric(levels(f))[as.integer(f)],
        nominal = factor(f, levels = levels(f), labels = lab), ordinal = ordered(factor(f, levels = levels(f), labels = lab)))
    } else out[[i]] <- switch(s$as, scale = as.numeric(col$as_vector()), nominal = factor(col$as_vector()), ordinal = ordered(factor(col$as_vector()))) }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}
read_percol <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) { s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) out[[i]] <- finish_factor(col$as_vector(), s$as, get_labels(field))
    else { v <- col$as_vector(); out[[i]] <- switch(s$as, scale = v, nominal = factor(v), ordinal = ordered(factor(v))) } }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}
read_overlay <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); sch <- tbl$schema; df <- as.data.frame(tbl)
  for (s in spec) { col <- df[[s$name]]
    if (is.factor(col)) df[[s$name]] <- finish_factor(col, s$as, get_labels(sch$GetFieldByName(s$name)))
    else df[[s$name]] <- switch(s$as, scale = col, nominal = factor(col), ordinal = ordered(factor(col))) }
  df
}
read_batched <- function(path, spec) {
  sch <- read_feather(path, as_data_frame = FALSE)$schema   # cheap footer/schema read for jasp:labels
  df  <- read_feather(path, as_data_frame = TRUE)           # ONE batched C++ conversion (the fast path)
  for (s in spec) { col <- df[[s$name]]
    if (is.factor(col)) df[[s$name]] <- finish_factor(col, s$as, get_labels(sch$GetFieldByName(s$name)))
    else df[[s$name]] <- switch(s$as, scale = col, nominal = factor(col), ordinal = ordered(factor(col))) }
  df
}

gen_categorical <- function(nrows, ncols, nlevels, ordered_flag, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels))
  labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl); labels_json <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)
  cols <- lapply(seq_len(ncols), function(j) { f <- factor(sample(seq_len(nlevels), nrows, replace = TRUE), levels = seq_len(nlevels)); if (ordered_flag) ordered(f) else f })
  nm <- paste0(if (ordered_flag) "ord" else "nom", seq_len(ncols)); names(cols) <- nm
  dict_type <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(nm, function(n) field(n, dict_type, metadata = list("jasp:display_name" = n, "jasp:labels" = labels_json)))
  tbl <- do.call(Table$create, c(cols, list(schema = do.call(schema, fields))))
  path <- file.path(dir, sprintf("%s_%d_%d.arrow", if (ordered_flag) "ordinal" else "nominal", nrows, nlevels))
  write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = if (ordered_flag) "ordinal" else "nominal")))
}

dir <- file.path(tempdir(), "jasp_def"); dir.create(dir, showWarnings = FALSE)
funs <- list(naive = read_naive, percol = read_percol, overlay = read_overlay, batched = read_batched)

# correctness: all four identical (incl. scale-read)
cat("--- Correctness (10k rows): all vs naive ---\n")
g <- gen_categorical(1e4, 6, 9, TRUE, dir); spec_scale <- lapply(g$spec, function(s) list(name = s$name, as = "scale"))
ref <- read_naive(g$path, g$spec); ref_s <- read_naive(g$path, spec_scale)
for (nm in names(funs)) {
  cat(sprintf("  %-8s ordinal identical: %s | scale identical: %s\n",
              nm, identical(funs[[nm]](g$path, g$spec), ref), identical(funs[[nm]](g$path, spec_scale), ref_s)))
}
cat("\n")

cases <- list(
  list(label = "nominal 1M x 10, 200 lv", g = gen_categorical(1e6, 10, 200, FALSE, dir)),
  list(label = "ordinal 1M x 10, 50 lv",  g = gen_categorical(1e6, 10, 50,  TRUE,  dir))
)
cat("--- Benchmark: 1M rows x 10 categorical cols (median ms) ---\n\n")
cat("| dataset                 | naive  | percol | overlay | batched |\n")
cat("|-------------------------|--------|--------|---------|---------|\n")
for (cs in cases) {
  g <- cs$g
  t <- sapply(funs, function(fn) time_ms(function() fn(g$path, g$spec), if (identical(fn, read_naive)) 3 else 5))
  cat(sprintf("| %-23s | %6.1f | %6.1f | %7.1f | %7.1f |\n", cs$label, t[["naive"]], t[["percol"]], t[["overlay"]], t[["batched"]]))
  unlink(g$path); gc(FALSE)
}
unlink(dir, recursive = TRUE); cat("\nDone.\n")
