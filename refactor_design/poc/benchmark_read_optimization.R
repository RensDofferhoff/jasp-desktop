#!/usr/bin/env Rscript
# Follow-up: the naive read_jasp_data (neo-jasp.md 8.3 reference impl) re-factors
# categorical columns with factor(f, levels=levels(f), labels=lab), an O(N) re-match
# in R. That made 1M-row categorical loads ~100-160x slower than Arrow's own
# conversion (see benchmark_arrow_load.R). This script:
#   (1) proves a fast variant is RESULT-IDENTICAL to the naive one, and
#   (2) benchmarks naive vs fast vs Arrow-native on the worst cases.
#
# The fix:
#   - relabel with  levels(f) <- lab            (O(k): replaces the levels attribute,
#                                                integer codes untouched)  instead of
#     factor(f, levels=levels(f), labels=lab)    (O(N): full re-match)
#   - mark ordinal with class(f) <- c("ordered","factor")  (O(1)) instead of ordered(factor(...))

suppressMessages(library(arrow))
cat("=== read_jasp_data: naive vs fast ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

time_ms <- function(f, reps, warmup = 1L) {
  for (i in seq_len(warmup)) { invisible(f()); gc(FALSE) }
  ts <- numeric(reps)
  for (i in seq_len(reps)) {
    gc(FALSE); t0 <- Sys.time(); invisible(f())
    ts[i] <- as.numeric(Sys.time() - t0, units = "secs") * 1000
  }
  c(min = min(ts), median = stats::median(ts))
}
label_or_value <- function(values, labels) {
  if (is.null(labels) || length(labels) == 0L) return(values)
  vapply(values, function(v) { l <- labels[[v]]; if (!is.null(l) && nzchar(l)) unname(l) else v },
         character(1))
}
get_labels <- function(field) {
  meta <- field$metadata
  if (!is.null(meta[["jasp:labels"]]) && nzchar(meta[["jasp:labels"]]))
    jsonlite::fromJSON(meta[["jasp:labels"]]) else NULL
}

# ── NAIVE: faithful 8.3 reference impl (O(N) re-factor for categoricals) ──
read_jasp_naive <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) {
    s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) {
      f <- col$as_vector()
      lab <- label_or_value(levels(f), get_labels(field))
      out[[i]] <- switch(s$as,
        scale   = as.numeric(levels(f))[as.integer(f)],
        nominal = factor(f, levels = levels(f), labels = lab),
        ordinal = ordered(factor(f, levels = levels(f), labels = lab)))
    } else {
      out[[i]] <- switch(s$as, scale = as.numeric(col$as_vector()),
                         nominal = factor(col$as_vector()), ordered(factor(col$as_vector())))
    }
  }
  names(out) <- vapply(spec, function(s) s$name, character(1))
  structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}

# ── FAST: O(k) relabel + O(1) ordered-class ───────────────────────────────
read_jasp_fast <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) {
    s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) {
      f <- col$as_vector()                       # R factor, levels = VALUES (Arrow builds in C++)
      if (s$as == "scale") {
        out[[i]] <- as.numeric(levels(f))[as.integer(f)]   # parse k levels once, index N rows
      } else {
        levels(f) <- label_or_value(levels(f), get_labels(field))  # O(k) relabel; codes untouched
        if (s$as == "ordinal") class(f) <- c("ordered", "factor")  # O(1)
        out[[i]] <- f
      }
    } else {
      v <- col$as_vector()                       # float64 -> numeric
      out[[i]] <- switch(s$as,
        scale   = v,
        nominal = factor(v),
        ordinal = ordered(factor(v)))
    }
  }
  names(out) <- vapply(spec, function(s) s$name, character(1))
  structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}

# ── Generators ────────────────────────────────────────────────────────────
gen_categorical <- function(nrows, ncols, nlevels, ordered_flag, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels))
  labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl)
  labels_json <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)
  cols <- lapply(seq_len(ncols), function(j) {
    f <- factor(sample(seq_len(nlevels), nrows, replace = TRUE), levels = seq_len(nlevels))
    if (ordered_flag) ordered(f) else f
  })
  nm <- paste0(if (ordered_flag) "ord" else "nom", seq_len(ncols)); names(cols) <- nm
  dict_type <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(nm, function(n) field(n, dict_type,
    metadata = list("jasp:display_name" = n, "jasp:labels" = labels_json)))
  sch <- do.call(schema, fields)
  tbl <- do.call(Table$create, c(cols, list(schema = sch)))
  tag <- if (ordered_flag) "ordinal" else "nominal"
  path <- file.path(dir, sprintf("%s_%d_%d.arrow", tag, nrows, nlevels))
  write_feather(tbl, path, compression = "lz4")
  as_type <- if (ordered_flag) "ordinal" else "nominal"
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = as_type)))
}

dir <- file.path(tempdir(), "jasp_opt"); dir.create(dir, showWarnings = FALSE)

# ── (1) Correctness: naive vs fast must be RESULT-IDENTICAL ───────────────
cat("--- Correctness (10k rows; nominal + ordinal + scale-read) ---\n")
g_nom <- gen_categorical(1e4, 6, 20, FALSE, dir)
g_ord <- gen_categorical(1e4, 6, 7,  TRUE, dir)
# a scale-read spec on the nominal data (numeric-coded values -> numbers)
spec_scale <- lapply(g_nom$spec, function(s) list(name = s$name, as = "scale"))

ok <- TRUE
for (g in list(g_nom, g_ord)) {
  a <- read_jasp_naive(g$path, g$spec); b <- read_jasp_fast(g$path, g$spec)
  same <- identical(a, b)
  ok <- ok && same
  cat(sprintf("  nominal/ordinal frame identical: %s\n", same))
}
a <- read_jasp_naive(g_nom$path, spec_scale); b <- read_jasp_fast(g_nom$path, spec_scale)
cat(sprintf("  scale-read-of-categorical identical: %s\n", identical(a, b)))
ok <- ok && identical(a, b)
# spot-check the semantics survived
f <- read_jasp_fast(g_ord$path, g_ord$spec)[[1]]
cat(sprintf("  ordinal is.ordered: %s | levels relabelled: %s\n",
            is.ordered(f), paste(utils::head(levels(f), 3), collapse = ",")))
if (!ok) { cat("CORRECTNESS FAILED\n"); quit(status = 1) }
cat("  -> fast path is result-identical to naive.\n\n")

# ── (2) Benchmark worst cases: naive vs fast vs arrow-native ──────────────
cases <- list(
  list(label = "nominal 1M x 10, 3 levels",   g = gen_categorical(1e6, 10, 3,  FALSE, dir)),
  list(label = "nominal 1M x 10, 200 levels", g = gen_categorical(1e6, 10, 200, FALSE, dir)),
  list(label = "ordinal 1M x 10, 5 levels",   g = gen_categorical(1e6, 10, 5,  TRUE, dir)),
  list(label = "ordinal 1M x 10, 50 levels",  g = gen_categorical(1e6, 10, 50, TRUE, dir))
)

cat("--- Benchmark: 1M rows x 10 categorical cols (median ms) ---\n\n")
cat("| dataset                     | naive 8.3 | fast (O(k)) | arrow->df | speedup |\n")
cat("|-----------------------------|-----------|-------------|-----------|---------|\n")
for (cs in cases) {
  g <- cs$g
  t_naive <- time_ms(function() read_jasp_naive(g$path, g$spec), 3)
  t_fast  <- time_ms(function() read_jasp_fast(g$path, g$spec),  3)
  t_adf   <- time_ms(function() read_feather(g$path, as_data_frame = TRUE), 3)
  cat(sprintf("| %-27s | %9.1f | %11.1f | %9.1f | %6.0fx |\n",
              cs$label, t_naive[["median"]], t_fast[["median"]], t_adf[["median"]],
              t_naive[["median"]] / t_fast[["median"]]))
  unlink(g$path); gc(FALSE)
}
unlink(dir, recursive = TRUE)
cat("\nDone.\n")
