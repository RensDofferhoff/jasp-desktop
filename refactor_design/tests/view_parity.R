#!/usr/bin/env Rscript
# Coercion-parity gate (runner-views-read-design.md slice A / analysis-views §9 gate 2).
#
# Judges the VIEW BUILDER (the Rust worker, crates/data_runner/src/analysisview.rs —
# §8.3's single owner of coercion under AV4) against the MIGRATION-ERA R FALLBACK
# (read_jasp_data, jaspRunner/R/data.R) over REAL orchestrator + worker binaries.
# Both sides implement the SYSTEM coercion matrix and the system level-string
# format (shortest round-trip, collision-free, locale-free) — this gate pins the
# two implementations together (and the matrix semantics: types, values, NAs,
# factor levels AND level order). It BLOCKS slice B (the runner read seam) and
# stays as a permanent regression test.
#
# What it does, per battery:
#   1. spawns jasp-orchestrator-v2 (the data worker is provisioned on demand);
#   2. registers a FAKE analysis runner socket (works dispatch and park correctly,
#      and the dispatched envelope hands us `view_refs` + the `dataset_paths`
#      bridge directly — no dir-layout guessing);
#   3. opens a real CSV through the real lane, staples a full-matrix spec
#      (every column x {scale, nominal, ordinal} — dual-role two-types-one-column
#      included by construction), lets the implied build run;
#   4. reads the blob (read_feather) and renames fields to aliases with the REAL
#      codec pulled from runner_jaspbase.R (the slice-B read, exercised here);
#   5. compares per column against read_jasp_data(base, {name, as}) with
#      identical().
#
# Batteries: debug.csv (rev 0), encoding_torture.csv (rev 0), a generated
# numeric torture CSV (NaN + numeric-string parse semantics; opened with
# nulls = ["", "NA"] so a literal NaN cell parses as a real f64 NaN), then both
# CSVs again AFTER an edit-lane schema_change (label overlay on debug/group;
# level remap to a NON-sorted order on torture/groep) — the only real ways
# `jasp:labels` and a hand-ordered dictionary get into a base.
#
# Known out-of-scope parse edges (deliberately NOT in the fixtures — no real
# dataset produces them through the lane): whitespace-padded numerics (" 7"),
# hex floats, inf spellings (R as.numeric and Rust str::parse disagree there).
#
# Run from anywhere: Rscript refactor_design/tests/view_parity.R
# (needs the built binaries: orchestrator/target/debug/{jasp-orchestrator-v2,
# jasp-data-runner}.)

library(nanonext)
library(jsonlite)
library(arrow)

`%||%` <- function(x, y) if (is.null(x)) y else x

# ── locate everything script-relative ─────────────────────────────────────────
args <- commandArgs(trailingOnly = FALSE)
file_arg <- sub("^--file=", "", args[grep("^--file=", args)])
here <- if (length(file_arg)) dirname(normalizePath(file_arg[1L])) else "refactor_design/tests"
repo_root <- normalizePath(file.path(here, "..", ".."))
orch_bin <- file.path(repo_root, "orchestrator", "target", "debug", "jasp-orchestrator-v2")
worker_bin <- file.path(repo_root, "orchestrator", "target", "debug", "jasp-data-runner")
runner_file <- normalizePath(file.path(here, "..", "runner_jaspbase.R"))
data_r <- file.path(repo_root, "jaspRunner", "R", "data.R")
for (p in c(orch_bin, worker_bin, runner_file, data_r))
  stopifnot(file.exists(p))

# The alias codec, straight from the runner's parse tree (walk_test.R pattern —
# the runner file cannot be source()d: it dials the orchestrator at top level).
want <- c("ALIAS_PREFIX", "ALIAS_TYPES", "alias_encode")
exprs <- parse(runner_file)
for (e in exprs) {
  if (is.call(e) && length(e) >= 3L && as.character(e[[1L]]) %in% c("<-", "=")) {
    nm <- tryCatch(as.character(e[[2L]]), error = function(err) character(0))
    if (length(nm) == 1L && nm %in% want) eval(e, envir = globalenv())
  }
}
stopifnot(exists("alias_encode"))

# The retiring engine (AV4's R-side reference): read_jasp_data + label_or_value.
source(data_r)
stopifnot(exists("read_jasp_data"))

hex_of <- function(nm) paste(sprintf("%02x", as.integer(charToRaw(nm))), collapse = "")

# ── check bookkeeping ─────────────────────────────────────────────────────────
nfail <- 0L; npass <- 0L
check <- function(label, cond) {
  ok <- isTRUE(cond)
  cat(sprintf("%s %s\n", if (ok) "PASS" else "FAIL", label))
  if (ok) npass <<- npass + 1L else nfail <<- nfail + 1L
  invisible(ok)
}
check_with_dump <- function(label, expected, actual) {
  ok <- isTRUE(identical(expected, actual))
  cat(sprintf("%s %s\n", if (ok) "PASS" else "FAIL", label))
  if (ok) { npass <<- npass + 1L; return(invisible(TRUE)) }
  nfail <<- nfail + 1L
  cat(paste(capture.output(print(expected)), collapse = "\n"), "  <- expected\n")
  cat(paste(capture.output(print(actual)),   collapse = "\n"), "  <- actual\n")
  invisible(FALSE)
}
note <- function(...) cat(sprintf(...))

# ── spawn the orchestrator ────────────────────────────────────────────────────
port <- 19800 + (Sys.getpid() %% 500)
url <- sprintf("tcp://127.0.0.1:%d", port)
root <- sprintf("/tmp/jasp-view-parity-%d", Sys.getpid())
unlink(root, recursive = TRUE)
orch <- system2(orch_bin, NULL,
                stdout = "/tmp/view-parity-orch.log", stderr = "/tmp/view-parity-orch.log",
                env = c(paste0("JASP_ORCH_URL=", url), paste0("JASP_ORCH_DIR_ROOT=", root)),
                wait = FALSE)
Sys.sleep(0.8)
on.exit({ tools::pskill(orch); Sys.sleep(0.2); unlink(root, recursive = TRUE) }, add = TRUE)

# 4-byte BE length prefix + JSON (§18.1) — the only framing the parity lane needs
# (no binary parts: no grid views, no edit-cell tails, no inverse blobs).
frame <- function(lst) {
  jb <- charToRaw(toJSON(lst, auto_unbox = TRUE, null = "null"))
  as.raw(c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb))
}
send_json <- function(sock, lst) invisible(send(sock, frame(lst), mode = "raw", block = 5000L))

recv_json <- function(sock, pred, timeout_ms = 60000L, label = "frame") {
  deadline <- Sys.time() + timeout_ms / 1000
  repeat {
    raw <- tryCatch(recv(sock, mode = "raw", block = 1000L), error = function(e) NULL)
    if (!is.null(raw) && length(raw) >= 4L) {
      len <- readBin(raw[1:4], "integer", n = 1L, size = 4L, endian = "big")
      if (length(raw) >= 4L + len && len > 0) {
        msg <- tryCatch(fromJSON(rawToChar(raw[5:(4 + len)]), simplifyVector = FALSE),
                        error = function(e) NULL)
        if (!is.null(msg) && isTRUE(pred(msg))) return(msg)
      }
    }
    if (Sys.time() > deadline)
      stop(sprintf("recv_json: no %s before timeout (%.0fs)", label, timeout_ms / 1000))
  }
}

# ── handshakes ────────────────────────────────────────────────────────────────
# Frontend: REQ hello -> welcome -> PAIR dial (the catalog push is skipped by
# every pred-based recv below, so it needs no explicit drain).
req <- socket("req", dial = url)
hello_at <- Sys.time()
repeat {
  ok <- tryCatch({
    send(req, frame(list(v = 1L, id = "parity-hello", type = "hello", client_id = "view-parity")),
         mode = "raw", block = 3000L); TRUE
  }, error = function(e) FALSE)
  if (ok) break
  if (Sys.time() - hello_at > 15) stop("orchestrator never came up")
  Sys.sleep(0.2)
}
ack <- recv(req, mode = "raw", block = 10000L)
len <- readBin(ack[1:4], "integer", n = 1L, size = 4L, endian = "big")
welcome <- fromJSON(rawToChar(ack[5:(4 + len)]), simplifyVector = FALSE)
stopifnot(isTRUE(welcome$ok))
session <- welcome$session_id
fe <- socket("poly", dial = welcome$channel_url)
close(req)

# Fake analysis runner: REQ register -> ack -> PAIR dial. One capability whose
# routing key exactly matches the parity works' module.
rreq <- socket("req", dial = url)
send_json(rreq, list(
  v = 1L, id = "parity-reg", type = "register",
  runner_id = sprintf("parity-fake-%d", Sys.getpid()),
  capabilities = list(list(kind = "analysis_r_classic_jaspbase",
                           name = "jaspParity", version = "1")),
  priority = 0L, slots = 1L, environment = list()))
rack <- recv(rreq, mode = "raw", block = 10000L)
len <- readBin(rack[1:4], "integer", n = 1L, size = 4L, endian = "big")
rack_msg <- fromJSON(rawToChar(rack[5:(4 + len)]), simplifyVector = FALSE)
stopifnot(isTRUE(rack_msg$ok), !is.null(rack_msg$channel_url))
run_ch <- socket("poly", dial = rack_msg$channel_url)
close(rreq)
note("session %s; fake runner %s up\n", session, rack_msg$runner_id %||% "?")

# ── lane helpers ──────────────────────────────────────────────────────────────
open_dataset <- function(tag, csv, ingest = NULL) {
  work <- list(
    v = 1L, id = paste0(tag, "-id"), type = "work", work_id = tag, revision = 0L,
    dataset_ids = list(), kind = "data",
    payload = list(op = "data_open", source = normalizePath(csv), cache_path = "",
                   format = "csv", ingest = if (is.null(ingest)) list() else ingest))
  send_json(fe, work)
  msg <- recv_json(fe, function(m) m$type == "result" && m$work_id == tag &&
                                 m$status %in% c("complete", "fatalError", "validationError"),
                   timeout_ms = 60000L, label = "data_open result")
  if (msg$status != "complete")
    stop(sprintf("open %s failed: %s", tag,
                 msg$payload$error_message %||% msg$message %||% "?"))
  list(dataset_id = msg$payload$dataset_id,
       revision = msg$payload$dataset_revision %||% 0)
}

base_path_of <- function(dataset_id, revision)
  file.path(root, session, "datasets", sprintf("%s_%d.arrow", dataset_id, revision))

edit_schema <- function(tag, dataset_id, revision, entries) {
  work <- list(
    v = 1L, id = paste0(tag, "-id"), type = "work", work_id = tag,
    revision = revision, dataset_ids = list(dataset_id), kind = "data",
    payload = list(op = "data_edit", source = "", cache_path = "", format = "csv",
                   ingest = list(), row_offset = 0L,
                   edit = list(op = "schema_change", target_schema = entries)))
  send_json(fe, work)
  msg <- recv_json(fe, function(m) m$type == "result" && m$work_id == tag &&
                                 m$status %in% c("complete", "fatalError", "validationError"),
                   timeout_ms = 60000L, label = "schema_change result")
  if (msg$status != "complete")
    stop(sprintf("edit %s failed: %s", tag,
                 msg$payload$error_message %||% msg$message %||% "?"))
  msg$payload$dataset_revision %||% (revision + 1L)
}

# ── the battery: staple a full-matrix spec, then judge the built blob ─────────
LEVELS <- c("scale", "nominal", "ordinal")

run_battery <- function(tag, dataset_id, revision, cols) {
  spec_cols <- unlist(lapply(cols, function(nm) lapply(LEVELS, function(ty)
    list(name = nm, as = ty))), recursive = FALSE)
  spec <- list(dataset_id = dataset_id, columns = spec_cols, all = FALSE)
  work <- list(
    v = 1L, id = paste0(tag, "-id"), type = "work", work_id = tag, revision = 1L,
    dataset_ids = list(dataset_id), kind = "analysis_r_classic_jaspbase",
    views = list(spec),
    payload = list(module = "jaspParity", module_version = "1", analysis = "Parity",
                   options = list(), settings = list(ppi = 96L, numDecimals = 3L)))
  send_json(fe, work)

  # The dispatched work lands on the fake runner — with the injected envelope
  # fields: one view_ref per spec + the dataset_paths migration bridge.
  disp <- recv_json(run_ch, function(m) m$type == "work" && m$work_id == tag,
                    timeout_ms = 90000L, label = "dispatched work")
  refs <- disp$view_refs
  stopifnot(is.list(refs), length(refs) == 1L, refs[[1]]$dataset_id == dataset_id,
            file.exists(refs[[1]]$path))
  base <- disp$dataset_paths[[dataset_id]]
  stopifnot(!is.null(base), file.exists(base))
  send_json(run_ch, list(
    v = 1L, id = paste0(tag, "-done"), reply_to = disp$id, session_id = disp$session_id,
    type = "result", work_id = tag, revision = disp$revision, status = "complete",
    kind = "analysis_r_classic_jaspbase", payload = list(results = list(title = "parity"))))
  msg <- recv_json(fe, function(m) m$type == "result" && m$work_id == tag &&
                                 m$status %in% c("complete", "fatalError", "validationError"),
                   timeout_ms = 30000L, label = "analysis terminal")
  if (msg$status != "complete")
    stop(sprintf("battery %s failed at the router: %s", tag,
                 msg$message %||% msg$payload$results$errorMessage %||% "?"))

  # ── the blob, as slice B will read it ──
  blob <- refs[[1]]$path
  tbl <- arrow::read_feather(blob, as_data_frame = FALSE)
  meta <- fromJSON(tbl$schema$metadata[["jasp:view"]], simplifyVector = FALSE)
  df <- as.data.frame(tbl)

  expected_fields <- c(vapply(spec_cols, function(c) sprintf("%s__%s", c$name, c$as), ""),
                       "__base_row")
  check(sprintf("[%s] field names/order + __base_row last", tag),
        identical(names(df), expected_fields))
  check(sprintf("[%s] meta: format/dataset/rows/base_row col/revision", tag),
        identical(meta$format_version, "av1") &&
        identical(meta$view_id, tools::file_path_sans_ext(basename(blob))) &&
        identical(meta$dataset_id, dataset_id) &&
        identical(as.integer(meta$rows), as.integer(nrow(df))) &&
        identical(meta$base_row_column, "__base_row") &&
        identical(as.integer(meta$base_revision), as.integer(revision)))
  check(sprintf("[%s] __base_row dense 0-based", tag),
        identical(df[["__base_row"]], seq_len(nrow(df)) - 1L))
  check(sprintf("[%s] token_map == derived hex (D2 cross-check)", tag),
        length(meta$token_map) == length(unique(vapply(spec_cols, function(c) c$name, ""))) &&
        all(vapply(spec_cols, function(c)
          identical(meta$token_map[[c$name]], hex_of(c$name)), TRUE)))
  check(sprintf("[%s] view_id == hash-named blob (content address)", tag),
        grepl("^[0-9a-f]{64}$", meta$view_id))

  # Slice-B rename: split each field at the LAST "__" -> alias via the REAL codec.
  fields <- setdiff(names(df), "__base_row")
  parts <- strsplit(fields, "__", fixed = TRUE)
  real <- vapply(parts, function(p) paste(p[-length(p)], collapse = "__"), "")
  ty   <- vapply(parts, function(p) p[length(p)], "")
  aliased <- mapply(alias_encode, real, ty, USE.NAMES = FALSE)
  check(sprintf("[%s] slice-B rename: aliases == codec(real, type)", tag),
        identical(unname(aliased), vapply(spec_cols, function(c)
          alias_encode(c$name, c$as), "")) &&
        !any(aliased == fields))  # aliasing must never be a no-op (collision probe)

  for (i in seq_along(spec_cols)) {
    nm <- spec_cols[[i]]$name; ty <- spec_cols[[i]]$as
    expected <- suppressWarnings(read_jasp_data(base, list(list(name = nm, as = ty))))[[1L]]
    actual <- df[[fields[i]]]
    # Direct identical(): R-arrow's dict→factor conversion faithfully carries the
    # `ordered` flag (probe-pinned), so a builder-side flag error shows up here as
    # a class mismatch — never papered over, no introspection needed.
    check_with_dump(sprintf("[%s] parity %s as %s", tag, nm, ty), expected, actual)
  }

  list(df = df, meta = meta, base = base, spec_cols = spec_cols)
}

# ── battery 1: debug.csv (rev 0) ──────────────────────────────────────────────
cat("\n== debug.csv: full matrix (4 cols x 3, dual-role by construction) ==\n")
d1 <- open_dataset("open-debug", file.path(repo_root, "test_data", "debug.csv"))
debug_cols <- names(arrow::read_feather(base_path_of(d1$dataset_id, d1$revision)))
A0 <- run_battery("debug-r0", d1$dataset_id, d1$revision, debug_cols)

# ── battery 2: encoding_torture.csv (rev 0) ───────────────────────────────────
cat("\n== encoding_torture.csv: full matrix (13 cols x 3) ==\n")
d2 <- open_dataset("open-torture", file.path(repo_root, "test_data", "encoding_torture.csv"))
torture_cols <- names(arrow::read_feather(base_path_of(d2$dataset_id, d2$revision)))
note("torture columns (post-lane): %s\n", paste(torture_cols, collapse = ", "))
T0 <- run_battery("torture-r0", d2$dataset_id, d2$revision, torture_cols)

# ── battery 3: numeric torture (NaN, parse semantics, the level-string format) ─
cat("\n== rchar: system level_string + NaN/null trichotomy + dict parse ==\n")
rchar_vals <- c(1e14, 1e5, 0.001, 1/3, 123456789012345, 1234567890123456,
                1234567890123457, 0.3, 0.30000000000000004,
                1e15, 1e16, 1e-4, 1e-5, 2/3, 6.02214076e23, 1.602176634e-19,
                0.1, 0.2, 1.5, 100, 98, 12345.6789, 7, 3e6, 2.5e-10, 1e6, 42,
                -1.5, -1e14, -0.001, 0.5, 0.25, 1e-2)
mix_pool <- c("1e5", "0.001", "abc", "\u63a7\u5236", "1.50", "1.5", "7", "NA", "",
              "2.5", "x1", "1e-4")
v_cells <- c(sprintf("%.17g", rchar_vals), "NaN", "NA", "")   # NaN=real NaN; NA/""=null
n_rows <- length(v_cells)
mix_cells <- mix_pool[((seq_len(n_rows) - 1L) %% length(mix_pool)) + 1L]
rchar_csv <- file.path(tempdir(), sprintf("rchar-parity-%d.csv", Sys.getpid()))
writeLines(c("v,mix", paste0(v_cells, ",", mix_cells)), rchar_csv)
d3 <- open_dataset("open-rchar", rchar_csv,
                   ingest = list(nulls = c("", "NA")))  # keep NaN out of the null spellings
R0 <- run_battery("rchar-r0", d3$dataset_id, d3$revision, c("v", "mix"))

# NaN vs NA in the BASE itself (the trichotomy the scale/nominal casts pin).
v_base <- arrow::read_feather(R0$base)$v
check("[rchar] base v: 1 NaN + 2 null rows",
      sum(is.nan(v_base)) == 1L && sum(is.na(v_base) & !is.nan(v_base)) == 2L)
# The system level-string table: the WORKER's rendering (via the blob's levels)
# judged against the R MIRROR (jasp_level_string, data.R) over the same doubles —
# two implementations of one documented format, not any language's cosmetics.
finite_sorted <- sort(unique(v_base[is.finite(v_base)]))
labs_all <- vapply(finite_sorted, jasp_level_string, character(1))
exp_levels <- labs_all[!duplicated(labs_all)]   # the %.15g grouping dedupe
act_levels <- levels(R0$df[["v__nominal"]])
check("[rchar] %.15g grouping: merge pairs collapse (0.3==0.1+0.2; 16-digit ints)",
      identical(jasp_level_string(0.3), jasp_level_string(0.1 + 0.2)) &&
      identical(jasp_level_string(1234567890123456), jasp_level_string(1234567890123457)) &&
      !identical(levels(R0$df[["v__nominal"]]) %in% "0.3", logical(0)) &&
      sum(act_levels == "0.3") == 1L && sum(act_levels == "1.23456789012346e+15") == 1L)
if (!check_with_dump("[rchar] level_string: f64->nominal levels == the system format",
                     exp_levels, act_levels)) {
  cmp <- data.frame(v = sprintf("%.17g", finite_sorted),
                    r_mirror = exp_levels, worker = act_levels)
  print(cmp)
}
# Row values through the nominal cast: labels are the system level strings
# (non-finite rows read NA — missing is missing, never a category).
want_vals <- rep(NA_character_, length(v_base))
finite <- is.finite(v_base)
want_vals[finite] <- vapply(v_base[finite], jasp_level_string, character(1))
check("[rchar] f64->nominal row values == jasp_level_string(v) (non-finite -> NA)",
      identical(as.character(R0$df[["v__nominal"]]), want_vals))
# scale keeps NaN as NaN (never collapsed to NA).
check("[rchar] f64->scale: NaN row survives as NaN",
      sum(is.nan(R0$df[["v__scale"]])) == 1L &&
      identical(which(is.nan(R0$df[["v__scale"]])), which(is.nan(v_base))))

# ── battery 4: label overlay via schema_change (debug/group, rev 1) ───────────
cat("\n== label overlay: schema_change labels on debug/group ==\n")
entries <- lapply(debug_cols, function(nm)
  if (nm == "group") list(name = nm, labels = list(A = "Alpha A")) else list(name = nm))
d1_rev <- edit_schema("edit-labels", d1$dataset_id, d1$revision, entries)
A1 <- run_battery("debug-r1", d1$dataset_id, d1_rev, debug_cols)
check("[labels] group nominal levels carry the overlay",
      identical(levels(A1$df[["group__nominal"]]), c("Alpha A", "B")))

# ── battery 5: level remap via schema_change (torture/groep, NON-sorted order) ─
cat("\n== level remap: schema_change levels [B, A] on torture/groep ==\n")
entries <- lapply(torture_cols, function(nm)
  if (nm == "groep") list(name = nm, levels = c("B", "A")) else list(name = nm))
d2_rev <- edit_schema("edit-remap", d2$dataset_id, d2$revision, entries)
T1 <- run_battery("torture-r1", d2$dataset_id, d2_rev, torture_cols)
check("[remap] groep nominal level ORDER preserved (B before A — never re-sorted)",
      identical(levels(T1$df[["groep__nominal"]]), c("B", "A")))

# ── summary ───────────────────────────────────────────────────────────────────
cat(sprintf("\n==== view parity: %d passed, %d failed ====\n", npass, nfail))
quit(status = if (nfail > 0L) 1L else 0L, save = "no")
