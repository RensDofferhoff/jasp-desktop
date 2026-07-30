#!/usr/bin/env Rscript
# Decisive wide-data question: read_jasp_data takes a columns_spec (the columns the
# analysis actually uses). Real analyses name a handful. The ~13 s "catastrophe" in
# benchmark_wide_short.R came from passing ALL 10,000 columns. Does the existing
# per-column impl already stay fast on wide data when the spec is SHORT (the realistic
# case)? This times the per-column impl across spec sizes on a 100 x 10,000 file.
#
# Mechanism: read_feather(as_data_frame=FALSE) mmaps lazily (one footer read, ~the open
# cost); tbl[[name]]$as_vector() then materializes ONLY that column. So cost ~ open +
# O(spec_size), independent of the 10,000 columns NOT in the spec.

suppressMessages(library(arrow))
cat("=== per-column read_jasp_data across spec sizes (100 x 10,000 file) ===\n")
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
read_percol <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  pj <- function(meta) if (!is.null(meta[["jasp:labels"]]) && nzchar(meta[["jasp:labels"]])) jsonlite::fromJSON(meta[["jasp:labels"]]) else NULL
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) { s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) { f <- col$as_vector()
      if (s$as == "scale") out[[i]] <- as.numeric(levels(f))[as.integer(f)]
      else { levels(f) <- label_or_value(levels(f), pj(field$metadata)); if (s$as == "ordinal") class(f) <- c("ordered","factor"); out[[i]] <- f }
    } else { v <- col$as_vector(); out[[i]] <- switch(s$as, scale = v, nominal = factor(v), ordinal = ordered(factor(v))) } }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}
gen <- function(nrow, ncol, nlevels, ordered_flag, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels)); labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl)
  lj <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)
  cols <- lapply(seq_len(ncol), function(j) { f <- factor(sample(seq_len(nlevels), nrow, replace = TRUE), levels = seq_len(nlevels)); if (ordered_flag) ordered(f) else f })
  nm <- paste0("c", seq_len(ncol)); names(cols) <- nm
  dt <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(nm, function(n) field(n, dt, metadata = list("jasp:display_name" = n, "jasp:labels" = lj)))
  tbl <- do.call(Table$create, c(cols, list(schema = do.call(schema, fields))))
  path <- file.path(dir, sprintf("%s_%d_%d.arrow", if (ordered_flag) "ordinal" else "nominal", nrow, ncol))
  write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = if (ordered_flag) "ordinal" else "nominal")))
}
dir <- file.path(tempdir(), "jasp_specsize"); dir.create(dir, showWarnings = FALSE)

g <- gen(100, 10000, 5, TRUE, dir); cat("Generated 100 x 10,000 ordinal (5 levels)\n\n")
t_open <- time_ms(function() read_feather(g$path, as_data_frame = FALSE))
cat(sprintf("open / mmap (footer with 10,000 fields): %.2f ms  <- fixed floor\n\n", t_open))
cat("| spec size (cols read) | per-column read_jasp_data |\n")
cat("|-----------------------|---------------------------|\n")
for (k in c(1, 5, 20, 100, 500, 10000)) {
  spec_k <- if (k >= 10000) g$spec else g$spec[1:k]
  reps <- if (k >= 10000) 3 else 7
  t <- time_ms(function() read_percol(g$path, spec_k), reps = reps)
  cat(sprintf("| %21d | %9.2f ms          |\n", k, t))
}
unlink(dir, recursive = TRUE); cat("\nDone.\n")
