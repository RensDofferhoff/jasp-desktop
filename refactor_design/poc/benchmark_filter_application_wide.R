#!/usr/bin/env Rscript
# Filter-application benchmark, WIDE shape: 100 rows x 10,000 columns (request all columns --
# the direct analog of the tall run requesting all 10). Same three strategies as the tall bench:
#   no_filter : read_jasp_data(path, spec)                    -- real data.R, no filter
#   r_end     : read_jasp_data(path, spec, filters=flagcol)    -- real data.R (Arrow read-time filter)
#   r_end_old : materialize ALL rows + type-convert, THEN subset x[mask] in R (the OLD approach,
#               inlined here for comparison since data.R no longer does it)
# Question: on the wide shape (few rows, MANY columns), is filtering viable, and does the Arrow
# read-time filter still beat subsetting in R after the fact?

suppressMessages(library(arrow))
source("jaspRunner/R/data.R")   # read_jasp_data (real, Arrow read-time filter) + label_or_value
cat("=== filter-application benchmark (WIDE: 100 x 10,000) ===\n")
cat("arrow", as.character(packageVersion("arrow")), "\n\n")

# the OLD R-end approach (materialize all, subset in R) -- for comparison only
read_jasp_data_rend <- function(path, columns_spec, filters = NULL) {
  sel <- vapply(columns_spec, function(s) s$name, character(1))
  df <- arrow::read_feather(path, as_data_frame = TRUE, col_select = tidyselect::all_of(sel))
  sch <- arrow::read_feather(path, as_data_frame = FALSE)$schema
  cache <- new.env(parent = emptyenv())
  labels_for <- function(name) { lj <- sch$GetFieldByName(name)$metadata[["jasp:labels"]]
    if (is.null(lj) || !nzchar(lj)) return(NULL); if (is.null(cache[[lj]])) cache[[lj]] <- jsonlite::fromJSON(lj); cache[[lj]] }
  out <- as.list(df)
  for (s in columns_spec) { vals <- out[[s$name]]
    if (is.factor(vals)) { if (s$as == "scale") out[[s$name]] <- as.numeric(levels(vals))[as.integer(vals)]
      else { lb <- labels_for(s$name); if (!is.null(lb) && length(lb) > 0L) levels(vals) <- label_or_value(levels(vals), lb)
        if (s$as == "ordinal" && !is.ordered(vals)) class(vals) <- c("ordered","factor"); out[[s$name]] <- vals } }
    else out[[s$name]] <- switch(s$as, scale = vals, nominal = factor(vals), ordinal = ordered(factor(vals))) }
  out <- out[sel]
  n_final <- nrow(df)
  if (length(filters)) { ftbl <- arrow::read_feather(path, as_data_frame = FALSE)
    mask <- Reduce(`&`, lapply(filters, function(f) as.data.frame(ftbl[f])[[1]])); out <- lapply(out, function(x) x[mask]); n_final <- sum(mask) }
  structure(out, class = "data.frame", row.names = c(NA_integer_, n_final))
}

# wide dataset: 100 rows, ncol_cat labelled categoricals + ncol_scale scale + 4 filters
gen_wide <- function(nrow, ncol_cat, ncol_scale, dir) {
  set.seed(42); k <- 5L; lvl <- as.character(seq_len(k))
  lj <- jsonlite::toJSON(as.list(setNames(paste0("Lab", seq_len(k)), lvl)), auto_unbox = TRUE)
  cols <- list(); fields <- list()
  for (i in seq_len(ncol_cat)) { nm <- sprintf("cat%04d", i)
    cols[[nm]] <- factor(sample(seq_len(k), nrow, replace = TRUE), levels = seq_len(k))
    fields[[nm]] <- field(nm, dictionary(int32(), utf8(), ordered = FALSE), metadata = list("jasp:display_name" = nm, "jasp:labels" = lj)) }
  m <- matrix(rnorm(nrow * ncol_scale, 100, 15), nrow = nrow)
  for (i in seq_len(ncol_scale)) { nm <- sprintf("num%04d", i); cols[[nm]] <- m[, i]
    fields[[nm]] <- field(nm, float64(), metadata = list("jasp:display_name" = nm)) }
  kept <- c()
  for (p in c(90, 50, 10, 1)) { nm <- sprintf("flag%02d", p); v <- runif(nrow) < p/100; cols[[nm]] <- v
    fields[[nm]] <- field(nm, boolean(), metadata = list("jasp:display_name" = nm)); kept[[nm]] <- sum(v) }
  tbl <- do.call(arrow::Table$create, c(cols, list(schema = do.call(schema, fields))))
  path <- file.path(dir, "filterdata_wide.arrow"); write_feather(tbl, path, compression = "lz4")
  list(path = path, kept = kept, nrow = nrow,
       spec = c(lapply(sprintf("cat%04d", seq_len(ncol_cat)), function(nm) list(name = nm, as = "nominal")),
                lapply(sprintf("num%04d", seq_len(ncol_scale)), function(nm) list(name = nm, as = "scale"))))
}

time_ms <- function(f, reps = 3, warmup = 1) {
  for (i in seq_len(warmup)) { invisible(f()); gc(FALSE) }
  ts <- numeric(reps)
  for (i in seq_len(reps)) { gc(FALSE); t0 <- Sys.time(); invisible(f()); ts[i] <- as.numeric(Sys.time() - t0, "secs") * 1000 }
  stats::median(ts)
}

dir <- tempdir(); NROW <- 100L; NCAT <- 1000L; NSCALE <- 9000L
cat(sprintf("Generating %d x %d dataset (%d labelled categoricals + %d scale + 4 filters)...\n",
            NROW, NCAT + NSCALE, NCAT, NSCALE))
d <- gen_wide(NROW, NCAT, NSCALE, dir)
cat(sprintf("  file: %.1f MB\n", file.info(d$path)$size / 1024^2))

# Request a RANDOM subset of min(#columns, 200) columns. A real analysis touches a subset (pruned
# by col_select), and RANDOM selection gives a representative categorical/scale mix rather than the
# biased "first N" (the generator lays out all categoricals first, then all scale).
set.seed(7)
n_select <- min(length(d$spec), 200L)
spec <- d$spec[sort(sample.int(length(d$spec), n_select))]
n_cat <- sum(vapply(spec, function(s) s$as != "scale", logical(1)))
cat(sprintf("  requesting a RANDOM %d of %d columns (%d categorical + %d scale)\n\n",
            n_select, length(d$spec), n_cat, n_select - n_cat))

# correctness: Arrow read-time (data.R) vs old R-end must agree (at 10%)
cat_nm <- spec[[which(vapply(spec, function(s) s$as != "scale", logical(1)))[1]]]$name
num_nm <- spec[[which(vapply(spec, function(s) s$as == "scale", logical(1)))[1]]]$name
r_new <- read_jasp_data(d$path, spec, filters = "flag10")
r_old <- read_jasp_data_rend(d$path, spec, filters = "flag10")
cat("correctness @10%: nrow arrow =", nrow(r_new), "| r_end_old =", nrow(r_old), "| expected =", d$kept[["flag10"]], "\n")
cat("  ", cat_nm, "identical:", identical(r_new[[cat_nm]], r_old[[cat_nm]]),
    "|", num_nm, "identical:", identical(r_new[[num_nm]], r_old[[num_nm]]), "\n\n")

flags <- list(flag90 = "90%", flag50 = "50%", flag10 = "10%", flag01 = "1%")
cat(sprintf("%-6s | %-9s | %-11s | %-12s | %-11s | %-14s\n",
            "kept", "no_filter", "r_end(OLD)", "+filter(R)", "arrow(new)", "arrow vs OLD"))
cat(strrep("-", 80), "\n")
for (fc in names(flags)) {
  t_no  <- time_ms(function() read_jasp_data(d$path, spec))
  t_old <- time_ms(function() read_jasp_data_rend(d$path, spec, filters = fc))
  t_new <- time_ms(function() read_jasp_data(d$path, spec, filters = fc))
  cat(sprintf("%-6s | %7.2f ms | %9.2f ms | %+10.2f ms | %9.2f ms | %12.2fx\n",
              flags[[fc]], t_no, t_old, t_old - t_no, t_new, t_old / t_new))
}
unlink(d$path)
