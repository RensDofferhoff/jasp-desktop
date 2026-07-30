#!/usr/bin/env Rscript
# Benchmark: cost of applying filters during data load (filters are part of the invariant
# work unit, so this cost is paid on EVERY re-read).
#
# Compares three strategies across filter selectivity (fraction of rows kept):
#   no_filter : read_jasp_data(path, spec)                       -- real data.R, no filter
#   r_end     : read_jasp_data(path, spec, filters=flagcol)       -- real data.R: materialize ALL
#               n rows + type-convert them, THEN subset x[mask] in R
#   arrow_rt  : read as Table, Table$Filter(mask) in C++ BEFORE converting to R, so only the
#               PASSING rows are materialized + type-converted
#
# Feather has no per-batch statistics, so Arrow's read-time filter saves CPU (conversion +
# subsetting of excluded rows), not disk I/O. Hypothesis: for selective filters arrow_rt beats
# r_end, and can even beat no_filter (it skips converting excluded rows entirely).

suppressMessages(library(arrow))
source("jaspRunner/R/data.R")   # read_jasp_data (real) + label_or_value
cat("=== filter-application benchmark ===\n")
cat("nanonext-free; arrow", as.character(packageVersion("arrow")), "\n\n")

# ---- candidate: Arrow read-time filter (filter in C++ before R conversion) ----
read_jasp_data_arrowfilter <- function(path, columns_spec, filters = NULL) {
  sel  <- vapply(columns_spec, function(s) s$name, character(1))
  need <- if (length(filters)) unique(c(sel, filters)) else sel
  tbl  <- arrow::read_feather(path, as_data_frame = FALSE, col_select = tidyselect::all_of(need))
  if (length(filters)) {
    mask <- NULL
    for (f in filters) { m <- tbl[[f]]$as_vector(); mask <- if (is.null(mask)) m else (mask & m) }
    tbl <- tbl$Filter(mask)                       # C++: keep only passing rows
  }
  sch <- tbl$schema
  cache <- new.env(parent = emptyenv())
  labels_for <- function(name) {
    lj <- sch$GetFieldByName(name)$metadata[["jasp:labels"]]
    if (is.null(lj) || !nzchar(lj)) return(NULL)
    if (is.null(cache[[lj]])) cache[[lj]] <- jsonlite::fromJSON(lj)
    cache[[lj]]
  }
  df  <- as.data.frame(tbl)                       # convert ONLY passing rows to R
  out <- as.list(df)
  for (s in columns_spec) {
    vals <- out[[s$name]]
    if (is.factor(vals)) {
      if (s$as == "scale") out[[s$name]] <- as.numeric(levels(vals))[as.integer(vals)]
      else {
        lb <- labels_for(s$name)
        if (!is.null(lb) && length(lb) > 0L) levels(vals) <- label_or_value(levels(vals), lb)
        if (s$as == "ordinal" && !is.ordered(vals)) class(vals) <- c("ordered", "factor")
        out[[s$name]] <- vals
      }
    } else out[[s$name]] <- switch(s$as, scale = vals, nominal = factor(vals), ordinal = ordered(factor(vals)))
  }
  out <- out[sel]                                  # drop filter cols, keep requested order
  structure(out, class = "data.frame", row.names = c(NA_integer_, nrow(df)))
}

# ---- dataset: 1M rows, 6 labelled categoricals + 4 scale + 4 boolean filters ----
gen <- function(nrow, dir) {
  set.seed(42); k <- 5L; lvl <- as.character(seq_len(k))
  lj <- jsonlite::toJSON(as.list(setNames(paste0("Lab", seq_len(k)), lvl)), auto_unbox = TRUE)
  mk_cat <- function(ord) { f <- factor(sample(seq_len(k), nrow, replace = TRUE), levels = seq_len(k)); if (ord) ordered(f) else f }
  cols <- list(); fields <- list()
  for (nm in c("nom1","nom2","nom3")) { cols[[nm]] <- mk_cat(FALSE); fields[[nm]] <- field(nm, dictionary(int32(), utf8(), ordered = FALSE), metadata = list("jasp:display_name" = nm, "jasp:labels" = lj)) }
  for (nm in c("ord1","ord2","ord3")) { cols[[nm]] <- mk_cat(TRUE);  fields[[nm]] <- field(nm, dictionary(int32(), utf8(), ordered = TRUE),  metadata = list("jasp:display_name" = nm, "jasp:labels" = lj)) }
  for (nm in c("num1","num2","num3","num4")) { cols[[nm]] <- rnorm(nrow, 100, 15); fields[[nm]] <- field(nm, float64(), metadata = list("jasp:display_name" = nm)) }
  kept <- c()
  for (p in c(90, 50, 10, 1)) { nm <- sprintf("flag%02d", p); v <- runif(nrow) < p/100; cols[[nm]] <- v; fields[[nm]] <- field(nm, boolean(), metadata = list("jasp:display_name" = nm)); kept[[nm]] <- sum(v) }
  tbl <- do.call(arrow::Table$create, c(cols, list(schema = do.call(schema, fields))))
  path <- file.path(dir, "filterdata.arrow"); write_feather(tbl, path, compression = "lz4")
  list(path = path, kept = kept, nrow = nrow)
}

time_ms <- function(f, reps = 5, warmup = 1) {
  for (i in seq_len(warmup)) { invisible(f()); gc(FALSE) }
  ts <- numeric(reps)
  for (i in seq_len(reps)) { gc(FALSE); t0 <- Sys.time(); invisible(f()); ts[i] <- as.numeric(Sys.time() - t0, "secs") * 1000 }
  stats::median(ts)
}

dir <- tempdir(); N <- 1e6L
cat("Generating", format(N, big.mark = ","), "row dataset (6 labelled categoricals + 4 scale + 4 filters)...\n")
d <- gen(N, dir)
spec <- c(lapply(c("nom1","nom2","nom3"), function(nm) list(name = nm, as = "nominal")),
          lapply(c("ord1","ord2","ord3"), function(nm) list(name = nm, as = "ordinal")),
          lapply(c("num1","num2","num3","num4"), function(nm) list(name = nm, as = "scale")))
cat(sprintf("  file: %.1f MB\n\n", file.info(d$path)$size / 1024^2))

# correctness: r_end and arrow_rt must agree (at 10% selectivity)
r_re <- read_jasp_data(d$path, spec, filters = "flag10")
r_af <- read_jasp_data_arrowfilter(d$path, spec, filters = "flag10")
cat("correctness @10%: nrow r_end =", nrow(r_re), "| arrow_rt =", nrow(r_af),
    "| expected =", d$kept[["flag10"]], "\n")
cat("  nom1 identical:", identical(r_re$nom1, r_af$nom1),
    "| ord2 identical:", identical(r_re$ord2, r_af$ord2),
    "| num3 identical:", identical(r_re$num3, r_af$num3), "\n\n")

flags <- list(flag90 = "90%", flag50 = "50%", flag10 = "10%", flag01 = "1%")
cat(sprintf("%-6s | %-9s | %-9s | %-9s | %-9s | %-12s | %-14s\n",
            "kept", "no_filter", "r_end", "+filter(R)", "arrow_rt", "arrow vs r_end", "arrow vs none"))
cat(strrep("-", 88), "\n")
for (fc in names(flags)) {
  t_no <- time_ms(function() read_jasp_data(d$path, spec))
  t_re <- time_ms(function() read_jasp_data(d$path, spec, filters = fc))
  t_af <- time_ms(function() read_jasp_data_arrowfilter(d$path, spec, filters = fc))
  cat(sprintf("%-6s | %7.2f ms | %7.2f ms | %+8.2f ms | %7.2f ms | %10.2fx    | %+12.2f ms\n",
              flags[[fc]], t_no, t_re, t_re - t_no, t_af, t_re / t_af, t_af - t_no))
}
cat("\n(kept = fraction of rows passing the filter; +filter(R) = extra cost of the current\n",
    " R-end approach vs no filter; arrow vs none = arrow_rt minus the no-filter baseline.)\n", sep = "")
unlink(d$path)
