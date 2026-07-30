#!/usr/bin/env Rscript
# NEO JASP frontend — independent-samples t-test round-trip driver (verification harness).
#
# Does the frontend half of the new handshake (REQ `hello` -> `welcome{session_id, channel_url}` ->
# PAIR-dial the channel), then submits a `work` for jaspTTests::TTestIndependentSamples
# (dependent = x, group = group, on test_data/debug.arrow) and waits for the `result` that
# runner_jaspbase.R computes via jaspBase::runJaspResults(). Retries the submission until a runner
# is registered and routes it (the runner loads jaspBase before registering, so once routed the
# analysis returns quickly).
#
# Usage: Rscript refactor_design/frontend_ttest.R [control_url]

args <- commandArgs(trailingOnly = TRUE)
url <- if (length(args) >= 1) args[1] else Sys.getenv("JASP_ORCH_URL", "tcp://127.0.0.1:9555")

suppressMessages({ library(nanonext); library(jsonlite) })

pack <- function(json_text) {
  jb <- charToRaw(json_text)
  c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb)
}
unframe <- function(msg) {
  len <- readBin(msg[1:4], "integer", n = 1L, size = 4L, endian = "big")
  rawToChar(msg[5:(4 + len)])
}
is_err <- function(x) if (exists("is_error_value", asNamespace("nanonext"))) nanonext::is_error_value(x) else inherits(x, "errorValue")

# ── 1. Frontend handshake on the control endpoint (REQ/REP, §17.2/§19.5) ─────
req <- socket("req", dial = url)
hello <- list(v = 1L, id = "fe-hello-1", type = "hello", client_id = "ttest-frontend")
send(req, pack(toJSON(hello, auto_unbox = TRUE, null = "null")), mode = "raw", block = 3000L)
welcome_raw <- recv(req, mode = "raw", block = 5000L)
if (is_err(welcome_raw) || length(welcome_raw) == 0) stop("[frontend] no welcome (is the orchestrator up?)")
welcome <- fromJSON(unframe(welcome_raw), simplifyVector = FALSE)
if (!isTRUE(welcome$ok)) stop("[frontend] hello rejected: ", if (is.null(welcome$error)) "?" else welcome$error)
session_id  <- welcome$session_id
channel_url <- welcome$channel_url
cat(sprintf("[frontend] session=%s channel=%s\n", session_id, channel_url))
close(req)

# ── 2. Data channel (PAIR v1) ─────────────────────────────────────────────────
ch <- socket("poly", dial = channel_url)
on.exit(close(ch), add = TRUE)

# ── 3. The t-test work (options: dependent = x, group = group; Student's t) ───
options <- list(
  `.meta` = list(dependent = list(shouldEncode = TRUE), group = list(shouldEncode = TRUE)),
  alternative = "twoSided",
  barPlot = FALSE, barPlotCiLevel = 0.95, barPlotErrorType = "ci", barPlotYAxisFixedToZero = TRUE,
  dependent = list(types = list(), value = list("x")),
  descriptives = FALSE, descriptivesPlot = FALSE, descriptivesPlotCiLevel = 0.95,
  effectSize = FALSE, effectSizeCi = FALSE, effectSizeCiLevel = 0.95, effectSizeType = "cohen",
  equalityOfVariancesTest = FALSE, equalityOfVariancesTestType = "brownForsythe",
  group = list(types = list(), value = "group"),
  mannWhitneyU = FALSE, meanDifference = FALSE, meanDifferenceCi = FALSE, meanDifferenceCiLevel = 0.95,
  naAction = "perDependent", normalityTest = FALSE,
  plotHeight = 300L, plotWidth = 350L,
  qqPlot = FALSE, qqPlotCi = FALSE, qqPlotCiLevel = 0.95,
  raincloudPlot = FALSE, raincloudPlotHorizontal = FALSE,
  student = TRUE, vovkSellke = FALSE, welch = FALSE
)
work <- list(
  v = 1L, type = "work", id = "work-ttest-1",
  work_id = "w-ttest", revision = 0L,
  dataset_ids = list("ds-001"),
  kind = "analysis",
  payload = list(
    module = "jaspTTests", module_version = "0.95.5",
    analysis = "TTestIndependentSamples",
    options = options,
    settings = list(ppi = 96L, numDecimals = 3L)
  )
)
work_raw <- pack(toJSON(work, auto_unbox = TRUE, null = "null"))

# ── 4. Submit (retry until a runner routes it), then wait for the result ──────
result <- NULL
for (attempt in 1:60) {
  send(ch, work_raw, mode = "raw", block = 3000L)
  res <- recv(ch, mode = "raw", block = 3000L)
  if (!is_err(res) && length(res) > 0) {
    result <- fromJSON(unframe(res), simplifyVector = FALSE)
    if (identical(result$type, "result")) break
    cat(sprintf("[frontend] skipping type=%s frame (not a result)\n", result$type))
  }
  cat(sprintf("[frontend] attempt %d: no result yet (runner not ready?), retrying...\n", attempt))
}

if (is.null(result)) { cat("[frontend] FAIL: no result received\n"); quit(status = 1) }

cat(sprintf("[frontend] result work_id=%s status=%s\n", result$work_id, result$status))
res_json <- toJSON(result$payload$results, auto_unbox = TRUE, digits = NA)
if (!identical(result$status, "complete")) {
  cat("[frontend] FAIL: status is not complete\n")
  cat(sprintf("[frontend] results: %s\n", res_json))
  quit(status = 1)
}

cat("[frontend] SUCCESS — real t-test result received.\n")
cat(sprintf("[frontend] results (%d bytes): %s\n", nchar(res_json), substr(res_json, 1, 1500)))
quit(status = 0)
