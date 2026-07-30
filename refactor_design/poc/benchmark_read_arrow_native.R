#!/usr/bin/env Rscript
# Final variant: let Arrow do the batch C++ factor materialization
# (read_feather -> data.frame, ~11 ms at 1M rows), then apply ONLY the O(k)
# labels overlay + scale conversion in R. This combines Arrow's fast batch
# conversion with JASP's per-column requested-type handling, and should be the
# basis for jaspRunner::data.R.
#
#   read_jasp_arrow:
#     tbl <- read_feather(path, as_data_frame=FALSE)   # mmap open + footer (cheap)
#     df  <- as.data.frame(tbl)                         # ONE batched C++ conversion
#     for each requested column: O(k) relabel / O(N)-but-vectorized scale / O(1) ordered

suppressMessages(library(arrow))
cat("=== read_jasp_data: arrow-native batch + O(k) overlay ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

time_ms <- function(f, reps, warmup = 1L) {
  for (i in seq_len(warmup)) { invisible(f()); gc(FALSE) }
  ts <- numeric(reps)
  for (i in seq_len(reps)) { gc(FALSE); t0 <- Sys.time(); invisible(f())
    ts[i] <- as.numeric(Sys.time() - t0, units = "secs") * 1000 }
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

# reference (slow) for correctness comparison
read_jasp_naive <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  out <- vector("list", length(spec))
  for (i in seq_along(spec)) {
    s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); col <- tbl[[s$name]]
    if (inherits(field$type, "DictionaryType")) {
      f <- col$as_vector(); lab <- label_or_value(levels(f), get_labels(field))
      out[[i]] <- switch(s$as, scale = as.numeric(levels(f))[as.integer(f)],
        nominal = factor(f, levels = levels(f), labels = lab),
        ordinal = ordered(factor(f, levels = levels(f), labels = lab)))
    } else out[[i]] <- switch(s$as, scale = as.numeric(col$as_vector()),
        nominal = factor(col$as_vector()), ordinal = ordered(factor(col$as_vector())))
  }
  names(out) <- vapply(spec, function(s) s$name, character(1))
  structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}

# RECOMMENDED: Arrow batch conversion + O(k) overlay
read_jasp_arrow <- function(path, spec) {
  tbl <- read_feather(path, as_data_frame = FALSE)   # mmap open + footer
  sch <- tbl$schema
  df  <- as.data.frame(tbl)                          # ONE batched C++ conversion
  for (s in spec) {
    col <- df[[s$name]]
    if (is.factor(col)) {
      if (s$as == "scale") {
        df[[s$name]] <- as.numeric(levels(col))[as.integer(col)]   # parse k once, index N
      } else {
        levels(col) <- label_or_value(levels(col), get_labels(sch$GetFieldByName(s$name)))  # O(k)
        if (s$as == "ordinal" && !is.ordered(col)) class(col) <- c("ordered", "factor")     # O(1)
        df[[s$name]] <- col
      }
    } else {
      df[[s$name]] <- switch(s$as, scale = col, nominal = factor(col), ordinal = ordered(factor(col)))
    }
  }
  df
}

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

dir <- file.path(tempdir(), "jasp_arrow"); dir.create(dir, showWarnings = FALSE)

# correctness on 10k (incl. scale-read)
cat("--- Correctness (10k rows) ---\n")
g <- gen_categorical(1e4, 6, 12, TRUE, dir)
spec_scale <- lapply(g$spec, function(s) list(name = s$name, as = "scale"))
cat(sprintf("  ordinal identical:        %s\n", identical(read_jasp_naive(g$path, g$spec), read_jasp_arrow(g$path, g$spec))))
cat(sprintf("  scale-read identical:     %s\n", identical(read_jasp_naive(g$path, spec_scale), read_jasp_arrow(g$path, spec_scale))))
f <- read_jasp_arrow(g$path, g$spec)[[1]]
cat(sprintf("  ordered + relabelled ok:  %s (%s)\n\n", is.ordered(f), paste(utils::head(levels(f),3), collapse=",")))

cases <- list(
  list(label = "nominal 1M x 10, 200 levels", g = gen_categorical(1e6, 10, 200, FALSE, dir)),
  list(label = "ordinal 1M x 10, 50 levels",  g = gen_categorical(1e6, 10, 50,  TRUE,  dir))
)
cat("--- Benchmark: 1M rows x 10 categorical cols (median ms) ---\n\n")
cat("| dataset                     | naive 8.3 | arrow+overlay | vs naive |\n")
cat("|-----------------------------|-----------|---------------|----------|\n")
for (cs in cases) {
  g <- cs$g
  t_naive <- time_ms(function() read_jasp_naive(g$path, g$spec), 3)
  t_arrow <- time_ms(function() read_jasp_arrow(g$path, g$spec), 5)
  cat(sprintf("| %-27s | %9.1f | %13.1f | %7.0fx |\n",
              cs$label, t_naive[["median"]], t_arrow[["median"]],
              t_naive[["median"]] / t_arrow[["median"]]))
  unlink(g$path); gc(FALSE)
}
unlink(dir, recursive = TRUE)
cat("\nDone.\n")
