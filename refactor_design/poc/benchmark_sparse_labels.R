#!/usr/bin/env Rscript
# Sparse-labels benchmark: 1/10 of columns carry FULL labels (every level labelled);
# the other 9/10 have NO labels (value == label) -- the realistic case.
#
# Question: how much does skipping the relabel buy when value == label?
#
# Key subtlety being tested: the copy-on-modify that the guard avoids only happens on the
# OVERLAY path (columns extracted from a shared data.frame -> `levels(f) <-` copies the N
# codes). On the PER-COLUMN path, `tbl[[name]]$as_vector()` returns a FRESH uniquely-owned
# factor, so `levels(f) <-` modifies in place (no copy). So the guard's copy-avoidance should
# show up on the overlay path (tall data especially) and ~not on the per-column path.
#
# JSON parse is CACHED in every variant, so guarded-vs-unconditional isolates ONLY the relabel.

suppressMessages(library(arrow))
cat("=== sparse labels: 1/10 columns labelled (full), 9/10 value==label ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

time_ms <- function(f, reps = 5, warmup = 1) {
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

# generator: every `label_every`-th column has FULL labels; others have none
gen <- function(nrow, ncol, nlevels, ordered_flag, label_every, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels))
  lj <- jsonlite::toJSON(as.list(setNames(paste0("Lab", seq_len(nlevels)), lvl)), auto_unbox = TRUE)
  cols <- lapply(seq_len(ncol), function(j) { f <- factor(sample(seq_len(nlevels), nrow, replace = TRUE), levels = seq_len(nlevels)); if (ordered_flag) ordered(f) else f })
  nm <- paste0("c", seq_len(ncol)); names(cols) <- nm
  dt <- dictionary(index_type = int32(), value_type = utf8(), ordered = ordered_flag)
  fields <- lapply(seq_len(ncol), function(j) {
    meta <- if (j %% label_every == 1) list("jasp:display_name" = nm[j], "jasp:labels" = lj)
            else list("jasp:display_name" = nm[j])            # no labels -> value == label
    field(nm[j], dt, metadata = meta)
  })
  tbl <- do.call(Table$create, c(cols, list(schema = do.call(schema, fields))))
  tag <- sprintf("%s_%d_%d_le%d", if (ordered_flag) "ord" else "nom", nrow, ncol, label_every)
  path <- file.path(dir, paste0(tag, ".arrow")); write_feather(tbl, path, compression = "lz4")
  list(path = path, spec = lapply(nm, function(n) list(name = n, as = if (ordered_flag) "ordinal" else "nominal")))
}

# per-column read; guarded skips the relabel when labels empty
read_perc <- function(path, spec, guarded) {
  tbl <- read_feather(path, as_data_frame = FALSE); n <- tbl$num_rows
  cache <- new.env(parent = emptyenv()); out <- vector("list", length(spec))
  for (i in seq_along(spec)) {
    s <- spec[[i]]; field <- tbl$schema$GetFieldByName(s$name); f <- tbl[[s$name]]$as_vector()
    if (s$as == "scale") { out[[i]] <- as.numeric(levels(f))[as.integer(f)]; next }
    js <- field$metadata[["jasp:labels"]]
    labels <- if (!is.null(js) && nzchar(js)) { if (is.null(cache[[js]])) cache[[js]] <- jsonlite::fromJSON(js); cache[[js]] } else NULL
    if (guarded) { if (!is.null(labels) && length(labels) > 0) levels(f) <- label_or_value(levels(f), labels) }
    else         { levels(f) <- label_or_value(levels(f), labels) }
    if (s$as == "ordinal" && !is.ordered(f)) class(f) <- c("ordered","factor")
    out[[i]] <- f
  }
  names(out) <- vapply(spec, function(s) s$name, character(1)); structure(out, class = "data.frame", row.names = c(NA_integer_, n))
}
# overlay read: one batched C++ read, then per-column overlay (columns shared with df -> copy-on-modify on relabel)
read_overlay <- function(path, spec, guarded) {
  sch <- read_feather(path, as_data_frame = FALSE)$schema
  df <- suppressWarnings(read_feather(path, as_data_frame = TRUE, col_select = tidyselect::all_of(vapply(spec, function(s) s$name, character(1)))))
  cache <- new.env(parent = emptyenv())
  lf <- function(nm) { js <- sch$GetFieldByName(nm)$metadata[["jasp:labels"]]; if (is.null(js) || !nzchar(js)) return(NULL)
    if (is.null(cache[[js]])) cache[[js]] <- jsonlite::fromJSON(js); cache[[js]] }
  out <- as.list(df)
  for (i in seq_along(spec)) {
    s <- spec[[i]]; f <- out[[s$name]]
    if (s$as == "scale") { out[[s$name]] <- as.numeric(levels(f))[as.integer(f)]; next }
    labels <- lf(s$name)
    if (guarded) { if (!is.null(labels) && length(labels) > 0) levels(f) <- label_or_value(levels(f), labels) }
    else         { levels(f) <- label_or_value(levels(f), labels) }
    if (s$as == "ordinal" && !is.ordered(f)) class(f) <- c("ordered","factor")
    out[[s$name]] <- f
  }
  structure(out, class = "data.frame", row.names = c(NA_integer_, nrow(df)))
}

dir <- file.path(tempdir(), "jasp_sparse"); dir.create(dir, showWarnings = FALSE)

# correctness: guarded == unconditional (sparse + dense), both paths
cat("--- correctness: guarded == unconditional ---\n")
g_chk <- gen(200, 30, 5, TRUE, 10, dir)
cat("  percol  identical:", identical(read_perc(g_chk$path, g_chk$spec, TRUE), read_perc(g_chk$path, g_chk$spec, FALSE)), "\n")
cat("  overlay identical:", isTRUE(all.equal(read_overlay(g_chk$path, g_chk$spec, TRUE), read_overlay(g_chk$path, g_chk$spec, FALSE), check.attributes = FALSE)), "\n\n")

run_tall <- function(type_label, ordered_flag) {
  cat(sprintf("--- TALL 1,000,000 x 10  (%s, 5 levels) ---\n", type_label))
  gd <- gen(1e6, 10, 5, ordered_flag, 1,  dir)   # dense: all 10 labelled
  gs <- gen(1e6, 10, 5, ordered_flag, 10, dir)   # sparse: 1/10 labelled (= 1 col)
  pc_d <- time_ms(function() read_perc(gd$path, gd$spec, FALSE))
  pc_s_u <- time_ms(function() read_perc(gs$path, gs$spec, FALSE))
  pc_s_g <- time_ms(function() read_perc(gs$path, gs$spec, TRUE))
  ov_d <- time_ms(function() read_overlay(gd$path, gd$spec, FALSE))
  ov_s_u <- time_ms(function() read_overlay(gs$path, gs$spec, FALSE))
  ov_s_g <- time_ms(function() read_overlay(gs$path, gs$spec, TRUE))
  cat(sprintf("  per-column  dense (all labelled)        : %8.2f ms\n", pc_d))
  cat(sprintf("  per-column  sparse, unconditional       : %8.2f ms\n", pc_s_u))
  cat(sprintf("  per-column  sparse, guarded             : %8.2f ms   (guard vs uncond: %.2fx)\n", pc_s_g, pc_s_u / pc_s_g))
  cat(sprintf("  overlay     dense (all labelled)        : %8.2f ms\n", ov_d))
  cat(sprintf("  overlay     sparse, unconditional       : %8.2f ms\n", ov_s_u))
  cat(sprintf("  overlay     sparse, guarded             : %8.2f ms   (guard vs uncond: %.2fx; vs dense: %.2fx)\n\n",
              ov_s_g, ov_s_u / ov_s_g, ov_d / ov_s_g))
  unlink(c(gd$path, gs$path)); gc(FALSE)
}

run_wide <- function(type_label, ordered_flag) {
  cat(sprintf("--- WIDE 100 x 1,000  (%s, 5 levels; 1/10 = 100 labelled) ---\n", type_label))
  gd <- gen(100, 1000, 5, ordered_flag, 1,  dir)
  gs <- gen(100, 1000, 5, ordered_flag, 10, dir)
  ov_d   <- time_ms(function() read_overlay(gd$path, gd$spec, FALSE), reps = 2)
  ov_s_u <- time_ms(function() read_overlay(gs$path, gs$spec, FALSE), reps = 3)
  ov_s_g <- time_ms(function() read_overlay(gs$path, gs$spec, TRUE),  reps = 3)
  pc_s_g <- time_ms(function() read_perc(gs$path, gs$spec, TRUE), reps = 3)
  cat(sprintf("  overlay     dense (all labelled)        : %8.2f ms\n", ov_d))
  cat(sprintf("  overlay     sparse, unconditional       : %8.2f ms\n", ov_s_u))
  cat(sprintf("  overlay     sparse, guarded             : %8.2f ms   (guard vs uncond: %.2fx; vs dense: %.2fx)\n", ov_s_g, ov_s_u / ov_s_g, ov_d / ov_s_g))
  cat(sprintf("  per-column  sparse, guarded (default)   : %8.2f ms\n\n", pc_s_g))
  unlink(c(gd$path, gs$path)); gc(FALSE)
}

run_tall("nominal", FALSE)
run_tall("ordinal", TRUE)
run_wide("nominal", FALSE)
run_wide("ordinal", TRUE)

unlink(c(g_chk$path)); unlink(dir, recursive = TRUE); cat("Done.\n")
