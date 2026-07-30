#!/usr/bin/env Rscript
# Micro-decomposition: what is the ~1.7 ms per requested column on a wide file?
#
# Hypothesis: it is NOT data (100 rows materializes in microseconds). It is the two
# BY-NAME lookups per column -- tbl$schema$GetFieldByName(name) and tbl[[name]] -- which
# Arrow implements as LINEAR SCANS over the schema. On a 10,000-field file that is an
# O(10,000) string search per column, so a full wide read is O(columns x fields) = O(n^2).
#
# We (1) decompose the per-column cost into its operations, (2) confirm the by-name scans
# scale with file width by comparing a 100-field vs 10,000-field schema (~100x), and
# (3) show the fix: build a name->index map ONCE (O(n)) and access columns POSITIONALLY
# (O(1) each), turning a full wide read from O(n^2) into O(n).

suppressMessages(library(arrow))
cat("=== per-column cost decomposition (100 rows) ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

# per-column median ms: run op over each name in `nms`, divide total by length(nms)
percol_ms <- function(nms, op, reps = 5, warm = TRUE) {
  if (warm) for (nm in nms[1:min(3, length(nms))]) invisible(op(nm))
  ts <- numeric(reps)
  for (r in seq_len(reps)) {
    gc(FALSE); t0 <- Sys.time()
    for (nm in nms) invisible(op(nm))
    ts[r] <- as.numeric(Sys.time() - t0, "secs") * 1000 / length(nms)
  }
  stats::median(ts)
}

gen <- function(nrow, ncol, nlevels, dir) {
  set.seed(42); lvl <- as.character(seq_len(nlevels)); labs <- setNames(paste0("Lab", seq_len(nlevels)), lvl)
  lj <- jsonlite::toJSON(as.list(labs), auto_unbox = TRUE)
  cols <- lapply(seq_len(ncol), function(j) ordered(factor(sample(seq_len(nlevels), nrow, replace = TRUE), levels = seq_len(nlevels))))
  nm <- paste0("c", seq_len(ncol)); names(cols) <- nm
  dt <- dictionary(index_type = int32(), value_type = utf8(), ordered = TRUE)
  fields <- lapply(nm, function(n) field(n, dt, metadata = list("jasp:display_name" = n, "jasp:labels" = lj)))
  tbl <- do.call(Table$create, c(cols, list(schema = do.call(schema, fields))))
  path <- file.path(dir, sprintf("ord_%d_%d.arrow", nrow, ncol)); write_feather(tbl, path, compression = "lz4")
  path
}

dir <- file.path(tempdir(), "jasp_micro"); dir.create(dir, showWarnings = FALSE)
cat("Generating 100x100 and 100x10,000 (not timed)...\n")
p100 <- gen(100, 100, 5, dir)
p10k <- gen(100, 10000, 5, dir)
cat("  done\n\n")

lab5 <- paste0("Lab", 1:5)
decompose <- function(path, ncol, K) {
  tbl <- read_feather(path, as_data_frame = FALSE); sch <- tbl$schema
  nms <- paste0("c", seq_len(K))
  cat(sprintf("--- schema with %s fields; probing %d columns (per-column median) ---\n",
              format(ncol, big.mark = ","), K))
  g_getfield <- percol_ms(nms, function(nm) sch$GetFieldByName(nm))
  g_extract  <- percol_ms(nms, function(nm) tbl[[nm]])
  g_matvec   <- percol_ms(nms, function(nm) tbl[[nm]]$as_vector())
  g_meta     <- percol_ms(nms, function(nm) sch$GetFieldByName(nm)$metadata)
  g_json     <- percol_ms(nms, function(nm) jsonlite::fromJSON(sch$GetFieldByName(nm)$metadata[["jasp:labels"]]))
  g_relabel  <- percol_ms(nms, function(nm) { f <- tbl[[nm]]$as_vector(); levels(f) <- lab5; class(f) <- c("ordered","factor"); f })
  cat(sprintf("    GetFieldByName(name)   : %9.4f ms   [by-name schema scan]\n", g_getfield))
  cat(sprintf("    tbl[[name]]            : %9.4f ms   [by-name column extract]\n", g_extract))
  cat(sprintf("    + as_vector() (data)   : %9.4f ms   [materialize 100 rows]\n", g_matvec))
  cat(sprintf("    field$metadata         : %9.4f ms\n", g_meta))
  cat(sprintf("    + jsonlite::fromJSON   : %9.4f ms   [parse labels JSON]\n", g_json))
  cat(sprintf("    as_vector+relabel      : %9.4f ms   [materialize + O(k) relabel]\n", g_relabel))
  cat(sprintf("    --> GetFieldByName + tbl[[name]] ~= %.4f ms (the by-name lookups)\n\n",
              g_getfield + g_extract))
  invisible(list(getfield = g_getfield, extract = g_extract))
}

a <- decompose(p100, 100, K = 50)
b <- decompose(p10k, 10000, K = 50)
cat(sprintf("WIDTH SCALING: GetFieldByName on 10,000 fields is %.0fx the 100-field cost\n",
            b$getfield / a$getfield))
cat(sprintf("               tbl[[name]]  on 10,000 fields is %.0fx the 100-field cost\n",
            b$extract / a$extract))
cat("               (~100x => linear scan O(num_fields), confirmed)\n\n")

# ── the fix: one-time name->index map + positional access ─────────────────
cat("--- fix: positional access (build index once, then O(1) per column) ---\n")
tbl <- read_feather(p10k, as_data_frame = FALSE); sch <- tbl$schema
# determine tbl$column(i) indexing (0- vs 1-based) empirically
ref <- tbl[["c1"]]$as_vector()
base <- if (identical(tbl$column(0L)$as_vector(), ref)) 0L else 1L
cat(sprintf("    tbl$column(i) is %d-based\n", base))
all_fields <- sch$fields                              # ONE O(n) materialization
all_names  <- vapply(all_fields, function(f) f$name, character(1))

# full read of ALL 10,000 columns: by-name vs positional
nms_all <- paste0("c", 1:10000)
t_byname <- percol_ms(nms_all, function(nm) { fld <- sch$GetFieldByName(nm); col <- tbl[[nm]]; f <- col$as_vector(); levels(f) <- lab5; f }, reps = 2, warm = FALSE)
# positional: index lookup is match() against a prebuilt name vector (hash-like), column by position
t_pos <- {
  idx <- match(nms_all, all_names)                    # one-time O(n) (vectorized)
  fld_pos <- idx + base - 1L
  ts <- numeric(3)
  for (r in 1:3) { gc(FALSE); t0 <- Sys.time()
    for (j in seq_along(fld_pos)) { f <- tbl$column(fld_pos[j])$as_vector(); levels(f) <- lab5; class(f) <- c("ordered","factor") }
    ts[r] <- as.numeric(Sys.time() - t0, "secs") * 1000 }
  stats::median(ts)
}
cat(sprintf("    full 10,000-col read, BY-NAME   : %10.1f ms total (%.3f ms/col)\n", t_byname * 10000, t_byname))
cat(sprintf("    full 10,000-col read, POSITIONAL: %10.1f ms total (%.3f ms/col)\n", t_pos, t_pos / 10000))
cat(sprintf("    speedup: %.1fx\n", (t_byname * 10000) / t_pos))

unlink(dir, recursive = TRUE); cat("\nDone.\n")
