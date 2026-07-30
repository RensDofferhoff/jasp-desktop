#!/usr/bin/env Rscript
# Benchmark: cost of loading Arrow (Feather V2 + LZ4) data into the runner.
#
# Because the work unit is INVARIANT, the runner re-reads the dataset on every
# execution. So data-load latency is a recurring, fixed cost per work unit. This
# benchmarks that load path across the type / size / cardinality space:
#
#   - all-scalar (float64)   at several row counts
#   - all-nominal (dict, ordered=FALSE) at several row counts x level counts
#   - all-ordinal (dict, ordered=TRUE)  at several row counts x level counts
#   - a width sweep (column count) for scalar
#
# For each dataset we time THREE load operations (warm, median of N reps):
#   1. open/mmap     : read_feather(as_data_frame=FALSE)  -> Table (lazy mmap; no materialization)
#   2. arrow->df     : read_feather(as_data_frame=TRUE)   -> Arrow's built-in R data.frame conversion
#   3. jasp load     : read_jasp_data()                    -> the runner's requested-type path from neo-jasp.md 8.3
#
# (1) shows the zero-copy open is near-free; the real cost is materialization, which
# (2) and (3) capture. Comparing (2) vs (3) isolates the cost of JASP's per-column
# requested-type handling (labels overlay, ordered factors, per-level scale conversion).

suppressMessages(library(arrow))

cat("=== Arrow load benchmark ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "| R:", R.version.string, "\n\n")

# ── Timing helper (no extra deps; sub-ms Sys.time resolution) ────────────
time_ms <- function(f, reps, warmup = 1L) {
  for (i in seq_len(warmup)) { invisible(f()); gc(FALSE) }
  ts <- numeric(reps)
  for (i in seq_len(reps)) {
    gc(FALSE)
    t0 <- Sys.time()
    invisible(f())
    ts[i] <- as.numeric(Sys.time() - t0, units = "secs") * 1000
  }
  c(min = min(ts), median = stats::median(ts), mean = mean(ts))
}
reps_for <- function(nrows) if (nrows >= 1e6) 3L else if (nrows >= 1e5) 5L else 7L

# ── The runner's load path (faithful port of neo-jasp.md 8.3) ────────────
label_or_value <- function(values, labels) {
  if (is.null(labels) || length(labels) == 0L) return(values)
  vapply(values, function(v) {
    l <- labels[[v]]
    if (!is.null(l) && nzchar(l)) unname(l) else v
  }, character(1))
}
# spec: list of list(name=, as=) ; as %in% c("scale","nominal","ordinal")
read_jasp_data <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE)   # mmap, zero-copy open
  n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) {
    s <- spec[[i]]
    field <- tbl$schema$GetFieldByName(s$name)
    col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) {     # categorical
      f <- col$as_vector()                            # factor; levels = the data VALUES
      meta <- field$metadata
      labels <- if (!is.null(meta[["jasp:labels"]]) && nzchar(meta[["jasp:labels"]]))
        jsonlite::fromJSON(meta[["jasp:labels"]]) else NULL
      lab <- label_or_value(levels(f), labels)        # overlay (k levels, not N rows)
      out[[i]] <- switch(s$as,
        scale   = as.numeric(levels(f))[as.integer(f)],          # parse k levels ONCE, index N rows
        nominal = factor(f, levels = levels(f), labels = lab),
        ordinal = ordered(factor(f, levels = levels(f), labels = lab)))
    } else {                                          # scale (float64)
      out[[i]] <- switch(s$as,
        scale   = as.numeric(col$as_vector()),
        nominal = factor(col$as_vector()),
        ordinal = ordered(factor(col$as_vector())))
    }
  }
  names(out) <- vapply(spec, function(s) s$name, character(1))
  structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}

# ── Dataset generators (write Feather V2 + LZ4, return path + spec) ──────
gen_scalar <- function(nrows, ncols, dir) {
  set.seed(42)
  m <- matrix(rnorm(nrows * ncols, 100, 15), nrow = nrows)
  df <- as.data.frame(m); names(df) <- paste0("v", seq_len(ncols))
  fields <- lapply(names(df), function(nm)
    field(nm, float64(), metadata = list("jasp:display_name" = nm, "jasp:all_integer" = "false")))
  sch <- do.call(schema, fields)
  tbl <- do.call(Table$create, c(as.list(df), list(schema = sch)))
  path <- file.path(dir, sprintf("scalar_%d_%d.arrow", nrows, ncols))
  write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(names(df), function(nm) list(name = nm, as = "scale")))
}
gen_categorical <- function(nrows, ncols, nlevels, ordered_flag, dir) {
  set.seed(42)
  lvl <- as.character(seq_len(nlevels))
  labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl)
  labels_json <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)   # {"1":"Lab1",...}
  cols <- lapply(seq_len(ncols), function(j) {
    f <- factor(sample(seq_len(nlevels), nrows, replace = TRUE), levels = seq_len(nlevels))
    if (ordered_flag) ordered(f) else f
  })
  nm <- paste0(if (ordered_flag) "ord" else "nom", seq_len(ncols)); names(cols) <- nm
  dict_type <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(nm, function(n)
    field(n, dict_type, metadata = list("jasp:display_name" = n, "jasp:labels" = labels_json)))
  sch <- do.call(schema, fields)
  tbl <- do.call(Table$create, c(cols, list(schema = sch)))
  tag <- if (ordered_flag) "ordinal" else "nominal"
  path <- file.path(dir, sprintf("%s_%d_%d_%d.arrow", tag, nrows, ncols, nlevels))
  write_feather(tbl, path, compression = "lz4")
  as_type <- if (ordered_flag) "ordinal" else "nominal"
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = as_type)))
}

# ── Config matrix ─────────────────────────────────────────────────────────
configs <- list()
add <- function(...) configs[[length(configs) + 1L]] <<- list(...)
sizes <- c(1e3, 1e4, 1e5, 1e6)
for (n in sizes) add(kind = "scalar",  nrows = n, ncols = 20, nlevels = NA)      # size sweep
for (nc in c(5, 100)) add(kind = "scalar", nrows = 1e5, ncols = nc, nlevels = NA) # width sweep @100k
for (n in sizes) for (lv in c(3, 20, 200)) add(kind = "nominal", nrows = n, ncols = 10, nlevels = lv)
for (n in sizes) for (lv in c(5, 10, 50))  add(kind = "ordinal", nrows = n, ncols = 10, nlevels = lv)

dir <- file.path(tempdir(), "jasp_bench"); dir.create(dir, showWarnings = FALSE)
results <- list()

cat(sprintf("Benchmarking %d datasets...\n\n", length(configs)))

for (ci in seq_along(configs)) {
  cfg <- configs[[ci]]
  cat(sprintf("[%2d/%d] %-8s rows=%-8s cols=%-4s levels=%-4s ... ",
              ci, length(configs), cfg$kind, format(cfg$nrows, big.mark = ","),
              cfg$ncols, ifelse(is.na(cfg$nlevels), "-", cfg$nlevels)))

  gen <- if (cfg$kind == "scalar")
    gen_scalar(cfg$nrows, cfg$ncols, dir) else
    gen_categorical(cfg$nrows, cfg$ncols, cfg$nlevels, cfg$kind == "ordinal", dir)

  fsize_kb <- file.info(gen$path)$size / 1024
  reps <- reps_for(cfg$nrows)

  t_open <- time_ms(function() read_feather(gen$path, as_data_frame = FALSE), reps)
  t_df   <- time_ms(function() read_feather(gen$path, as_data_frame = TRUE),  reps)
  t_jasp <- time_ms(function() read_jasp_data(gen$path, gen$spec),            reps)

  results[[ci]] <- data.frame(
    kind = cfg$kind, nrows = cfg$nrows, ncols = cfg$ncols,
    nlevels = ifelse(is.na(cfg$nlevels), NA_real_, cfg$nlevels),
    file_kb = round(fsize_kb, 1),
    open_ms = round(t_open[["median"]], 3),
    arrow_df_ms = round(t_df[["median"]], 3),
    jasp_load_ms = round(t_jasp[["median"]], 3),
    jasp_load_min = round(t_jasp[["min"]], 3),
    stringsAsFactors = FALSE
  )
  cat(sprintf("file=%9s KB | open=%7.2f ms | arrow->df=%8.2f ms | jasp=%8.2f ms\n",
              format(round(fsize_kb, 1), big.mark = ","),
              t_open[["median"]], t_df[["median"]], t_jasp[["median"]]))

  unlink(gen$path); rm(gen); gc(FALSE)
}

# ── Column-pruning extra: 100k x 100 scalar, read all vs 5 cols ──────────
cat("\n--- Column pruning (100k rows x 100 scalar cols) ---\n")
gen <- gen_scalar(1e5, 100, dir)
spec_all <- gen$spec
spec_5   <- gen$spec[1:5]
t_all <- time_ms(function() read_jasp_data(gen$path, spec_all), 5)
t_5   <- time_ms(function() read_jasp_data(gen$path, spec_5),   5)
cat(sprintf("  read all 100 cols: %8.2f ms (median)\n", t_all[["median"]]))
cat(sprintf("  read  5 of 100   : %8.2f ms (median)  <- %.1fx faster\n",
            t_5[["median"]], t_all[["median"]] / t_5[["median"]]))
unlink(gen$path)

# ── Assemble + emit ───────────────────────────────────────────────────────
res <- do.call(rbind, results)
csv_path <- file.path(dirname(dir), "arrow_load_benchmark.csv")
write.csv(res, csv_path, row.names = FALSE)

fmt_n <- function(x) format(x, big.mark = ",", trim = TRUE, scientific = FALSE)
cat("\n=== RESULTS (median ms, warm; file = LZ4 Feather V2) ===\n\n")
cat("| kind    | rows      | cols | levels | file KB   | open(mmap) | arrow->df | jasp load |\n")
cat("|---------|-----------|------|--------|-----------|------------|-----------|-----------|\n")
for (i in seq_len(nrow(res))) {
  r <- res[i, ]
  cat(sprintf("| %-7s | %9s | %4d | %6s | %9s | %10.2f | %9.2f | %9.2f |\n",
              r$kind, fmt_n(r$nrows), r$ncols,
              ifelse(is.na(r$nlevels), "-", fmt_n(r$nlevels)),
              fmt_n(r$file_kb), r$open_ms, r$arrow_df_ms, r$jasp_load_ms))
}
cat(sprintf("\nCSV written: %s\n", csv_path))
unlink(dir, recursive = TRUE)
