#!/usr/bin/env Rscript
# Slice-B gate (runner-views-read-design.md §4 slice B): the view-read seam over the
# REAL stack — v2 orchestrator + provisioned data worker + the REAL jaspBase runner
# (runner_jaspbase.R). The same analysis runs TWICE over the same dataset:
#
#   run A ("fallback"): NO stapled views — the runner's lazy base path (today's code,
#                       the migration bridge; classic and unstapled v2 works look like this)
#   run B ("views"):    a hand-stapled typed spec — the router orders the implied build,
#                       the runner receives view_refs and serves the frame FRAME-FIRST
#
# The two jaspResults trees must be IDENTICAL — that is the whole claim of the seam:
# only WHO materialized the frame changed, never what the module sees.
#
# Dataset: test_data/encoding_torture.csv — dependent = `score` (stored ordinal
# dict; the scale cast exercises dict→scale values-parse), grouping = `T` (0/1 —
# exactly two levels; dict→nominal). D11: the options and the staple speak STORAGE
# TOKENS (the post-flip wire vocabulary — the base's field names); a final bridge
# pair re-runs with CLASSIC display-name options and must match byte-for-byte.
# The staple ALSO casts T as scale — the DUAL-ROLE superset (two casts of one column
# in one blob, unused by the options).
# Two comparisons: (1) preloadData TRUE — the view frame IS the preload frame;
# (2) preloadData FALSE — the on-demand frame-first natives (.readDatasetToEnd-
# Native's ladder: frame hit / sibling coerce / lazy rung). Every pair must be
# byte-identical — only WHO materialized the frame changed, never what the module
# sees. (A full jaspAnova e2e needs the GUI's complete default options object —
# jaspBase fills no defaults for absent keys — that rides with slice C, where the
# real frontend staples views with real options; the walk's interaction-array shape
# stays pinned by walk_test.R §3.)
#
# Run: Rscript refactor_design/test_v2_views_ttest_e2e.R
library(nanonext)
library(jsonlite)

`%||%` <- function(x, y) if (is.null(x)) y else x

args <- commandArgs(trailingOnly = FALSE)
file_arg <- sub("^--file=", "", args[grep("^--file=", args)])
here <- if (length(file_arg)) dirname(normalizePath(file_arg[1L])) else "refactor_design"
repo_root <- normalizePath(file.path(here, ".."))
orch_bin <- file.path(repo_root, "orchestrator", "target", "debug", "jasp-orchestrator-v2")
stopifnot(file.exists(orch_bin))

LIBSET <- Sys.getenv("JASP_ORCH_LIBSET", "/home/sp42/jaspModuleTools/workdir/lib")
# The libset's jaspAnova dir is a COMPLETE library (jaspBase + jaspTTests + deps):
# ONE runner advertises the module we run.
RUNNER_LIBDIR <- file.path(LIBSET, "jaspAnova")
stopifnot(dir.exists(RUNNER_LIBDIR),
          file.exists(file.path(RUNNER_LIBDIR, "jaspTTests", "Description.qml")))

port <- 19950 + (Sys.getpid() %% 500)
url <- sprintf("tcp://127.0.0.1:%d", port)
root <- sprintf("/tmp/jasp-views-e2e-%d", Sys.getpid())
unlink(root, recursive = TRUE)

orch <- system2(orch_bin, NULL,
                stdout = "/tmp/views-e2e-orch.log", stderr = "/tmp/views-e2e-orch.log",
                env = c(paste0("JASP_ORCH_URL=", url), paste0("JASP_ORCH_DIR_ROOT=", root)),
                wait = FALSE)
Sys.sleep(0.8)
on.exit({ tools::pskill(orch); Sys.sleep(0.2); unlink(root, recursive = TRUE) }, add = TRUE)

frame <- function(lst) {
  jb <- charToRaw(toJSON(lst, auto_unbox = TRUE, null = "null"))
  as.raw(c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb))
}
send_json <- function(sock, lst) invisible(send(sock, frame(lst), mode = "raw", block = 5000L))
recv_json <- function(sock, pred, timeout_ms = 240000L, label = "frame") {
  deadline <- Sys.time() + timeout_ms / 1000
  repeat {
    raw <- tryCatch(recv(sock, mode = "raw", block = 2000L), error = function(e) NULL)
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

# Frontend handshake.
req <- socket("req", dial = url)
hello_at <- Sys.time()
repeat {
  ok <- tryCatch({
    send(req, frame(list(v = 1L, id = "e2e-hello", type = "hello", client_id = "views-e2e")),
         mode = "raw", block = 3000L)
    TRUE
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
cat(sprintf("session %s\n", session))

# Open the torture dataset through the real lane.
csv <- normalizePath(file.path(repo_root, "test_data", "encoding_torture.csv"))
open_work <- list(
  v = 1L, id = "w-open-id", type = "work", work_id = "w-open", revision = 0L,
  dataset_ids = list(), kind = "data",
  payload = list(op = "data_open", source = csv, cache_path = "", format = "csv",
                 ingest = list()))
send_json(fe, open_work)
msg <- recv_json(fe, function(m) m$type == "result" && m$work_id == "w-open" &&
                               m$status %in% c("complete", "fatalError"),
                 timeout_ms = 60000L, label = "data_open result")
stopifnot(msg$status == "complete")
dataset_id <- msg$payload$dataset_id
cat(sprintf("dataset %s ready (%d rows)\n", dataset_id, msg$payload$rows %||% NA))

# The REAL runner (jaspBase; boot takes a minute+ — poll its log).
run_log <- "/tmp/views-e2e-runner.log"
runner <- system2("Rscript", c(file.path(here, "runner_jaspbase.R"), url),
                  stdout = run_log, stderr = run_log,
                  env = paste0("JASP_RUNNER_LIBDIR=", RUNNER_LIBDIR), wait = FALSE)
on.exit({ tools::pskill(runner) }, add = TRUE)
deadline <- Sys.time() + 240
repeat {
  log <- tryCatch(readLines(run_log, warn = FALSE), error = function(e) "")
  if (any(grepl("waiting for work", log))) break
  if (Sys.time() > deadline) { cat("E2E FAIL: runner never registered\n"); quit(status = 1) }
  Sys.sleep(2)
}
cat("runner is up\n")

# TTestIndependentSamples options: dependent = score (stored ordinal-dict → the
# dict→scale values-parse path), grouping = T (0/1 — exactly 2 levels; dict→nominal).
# D11: the options bind STORAGE TOKENS (the post-flip frontend binds columnName = the
# wire `name` = the token); the staple additionally casts T as scale (the DUAL-ROLE
# superset: two casts of one column in one blob; unused by the options — proves the
# frame holds both).
tok <- function(nm) {
  paste0("jasp_enc_hex_", paste(sprintf("%02x", as.integer(charToRaw(nm))), collapse = ""))
}
ttest_options <- list(
  `.meta` = list(dependent = list(shouldEncode = TRUE), group = list(shouldEncode = TRUE)),
  alternative = "twoSided",
  barPlot = FALSE, barPlotCiLevel = 0.95, barPlotErrorType = "ci",
  barPlotYAxisFixedToZero = TRUE,
  dependent = list(types = list("scale"), value = list(tok("score"))),
  descriptives = TRUE, descriptivesPlot = FALSE, descriptivesPlotCiLevel = 0.95,
  effectSize = FALSE, effectSizeCi = FALSE, effectSizeCiLevel = 0.95,
  effectSizeType = "cohen",
  equalityOfVariancesTest = FALSE, equalityOfVariancesTestType = "brownForsythe",
  group = list(types = list("nominal"), value = tok("T")),
  mannWhitneyU = FALSE, meanDifference = FALSE, meanDifferenceCi = FALSE,
  meanDifferenceCiLevel = 0.95,
  naAction = "perDependent", normalityTest = FALSE,
  plotHeight = 300L, plotWidth = 350L,
  qqPlot = FALSE, qqPlotCi = FALSE, qqPlotCiLevel = 0.95,
  raincloudPlot = FALSE, raincloudPlotHorizontal = FALSE,
  student = TRUE, vovkSellke = FALSE, welch = FALSE)

# The migration bridge: the SAME options with CLASSIC display names — the walk must
# produce identical results for either vocabulary during the migration window.
ttest_options_classic <- local({
  o <- ttest_options
  o$dependent <- list(types = list("scale"), value = list("score"))
  o$group <- list(types = list("nominal"), value = "T")
  o
})

submit <- function(work_id, module, analysis, options, preload = TRUE, views = NULL) {
  w <- list(
    v = 1L, id = paste0(work_id, "-id"), type = "work", work_id = work_id, revision = 1L,
    dataset_ids = list(dataset_id), kind = "analysis_r_classic_jaspbase",
    payload = list(module = module, module_version = "0.95.5",
                   analysis = analysis,
                   options = options, preloadData = preload,
                   settings = list(ppi = 96L, numDecimals = 3L)))
  if (!is.null(views)) w$views <- views
  w
}

# Recursively drop volatile keys (timestamps etc.) so the comparison is about CONTENT.
scrub <- function(x) {
  if (is.list(x)) {
    keep <- !names(x) %in% c("timestamp", "time", "date", "startTime", "endTime",
                             "created", "modified", "lastEdited")
    x <- x[keep]
    lapply(x, scrub)
  } else if (is.character(x)) x else x
}

run_one <- function(tag, module, analysis, options, preload, views) {
  send_json(fe, submit(tag, module, analysis, options, preload, views))
  msg <- recv_json(fe, function(m) m$type == "result" && m$work_id == tag &&
                                 m$status %in% c("complete", "fatalError", "validationError"),
                   timeout_ms = 240000L, label = paste0(tag, " terminal"))
  if (msg$status != "complete") {
    cat(sprintf("E2E FAIL: %s -> %s: %s\n", tag, msg$status,
                msg$payload$results$errorMessage %||% msg$message %||% "?"))
    quit(status = 1)
  }
  msg$payload$results
}

cat("== run A: fallback (no views — today's lazy path) ==\n")
resA <- run_one("w-fallback", "jaspTTests", "TTestIndependentSamples", ttest_options, TRUE, NULL)

cat("== run B: stapled views (frame-first seam) ==\n")
spec <- list(dataset_id = dataset_id,
             columns = list(list(name = tok("score"), as = "scale"),
                            list(name = tok("T"), as = "nominal"),
                            list(name = tok("T"), as = "scale")))
resB <- run_one("w-views", "jaspTTests", "TTestIndependentSamples", ttest_options, TRUE,
                list(spec))

jA <- toJSON(scrub(resA), auto_unbox = TRUE, null = "null", digits = NA)
jB <- toJSON(scrub(resB), auto_unbox = TRUE, null = "null", digits = NA)
cat(sprintf("results A: %d bytes; results B: %d bytes\n", nchar(jA), nchar(jB)))
if (!identical(jA, jB)) {
  writeLines(jA, "/tmp/views-e2e-resA.json")
  writeLines(jB, "/tmp/views-e2e-resB.json")
  cat("E2E FAIL: t-test results differ (fallback vs views) — dumped to /tmp/views-e2e-res{A,B}.json\n")
  quit(status = 1)
}
cat("E2E OK: t-test results IDENTICAL (fallback vs views)\n")

# ── The ON-DEMAND path (preloadData FALSE): jaspBase fetches columns through
# .readDatasetToEndNative — the frame-first ladder (frame hit / sibling coerce /
# lazy rung) instead of the preload frame. Same analysis, same complete options.
cat("== on-demand run A: fallback ==\n")
resC <- run_one("w-lazy-fallback", "jaspTTests", "TTestIndependentSamples", ttest_options, FALSE, NULL)
cat("== on-demand run B: views (frame-first natives) ==\n")
resD <- run_one("w-lazy-views", "jaspTTests", "TTestIndependentSamples", ttest_options, FALSE,
                list(spec))
jC <- toJSON(scrub(resC), auto_unbox = TRUE, null = "null", digits = NA)
jD <- toJSON(scrub(resD), auto_unbox = TRUE, null = "null", digits = NA)
if (!identical(jC, jD)) {
  writeLines(jC, "/tmp/views-e2e-lazyA.json")
  writeLines(jD, "/tmp/views-e2e-lazyB.json")
  cat("E2E FAIL: on-demand results differ — dumped to /tmp/views-e2e-lazy{A,B}.json\n")
  quit(status = 1)
}
cat("E2E OK: on-demand (frame-first natives) results IDENTICAL\n")
stopifnot(identical(jA, jC))   # preload and on-demand agree too (they always did)

# The runner log must show the seam actually served (not silently fell back):
# one view read per stapled work (t-test blob + ANOVA blob).
log <- tryCatch(readLines(run_log, warn = FALSE), error = function(e) "")
stopifnot(length(grep("view read", log)) >= 2L)
cat("E2E OK: runner log shows both view reads (frame-first served)\n")

# ── The D11 MIGRATION BRIDGE: classic display-name options must produce the IDENTICAL
# results as token-name options, through BOTH paths (fallback + views). The walk
# resolves either vocabulary to the same aliases — this pins that end to end.
cat("== bridge run: classic display-name options (fallback + views) ==\n")
resE <- run_one("w-bridge-fallback", "jaspTTests", "TTestIndependentSamples",
                ttest_options_classic, TRUE, NULL)
resF <- run_one("w-bridge-views", "jaspTTests", "TTestIndependentSamples",
                ttest_options_classic, TRUE, list(spec))
jE <- toJSON(scrub(resE), auto_unbox = TRUE, null = "null", digits = NA)
jF <- toJSON(scrub(resF), auto_unbox = TRUE, null = "null", digits = NA)
stopifnot(identical(jA, jE), identical(jA, jF))
cat("E2E OK: token options == classic display options (the migration bridge holds)\n")

quit(status = 0)
