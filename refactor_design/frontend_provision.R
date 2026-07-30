#!/usr/bin/env Rscript
# NEO JASP frontend — provisioner end-to-end test.
#
# Submits a single TTest work with NO runner running. The orchestrator must:
#   1. Park the work (no live runner).
#   2. Ask the provisioner for a runner for jaspTTests.
#   3. The provisioner spawns a real Rscript runner bound to the jaspTTests libpath.
#   4. The runner registers, the parked work is dispatched, and the runner completes the analysis.
#   5. The frontend receives a `complete` result.
#
# PASS criterion: the frontend receives a `complete` result within the timeout.
# This proves the full provisioner → spawn → register → dispatch → complete path works end-to-end.
#
# Usage: Rscript refactor_design/frontend_provision.R [control_url]
# Env:   JASP_ORCH_URL  control endpoint (default tcp://127.0.0.1:9555)

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

# ── 1. Frontend handshake on the control endpoint ────────────────────────────
req <- socket("req", dial = url)
hello <- list(v = 1L, id = "fe-hello-provision", type = "hello", client_id = "provision-frontend")
send(req, pack(toJSON(hello, auto_unbox = TRUE, null = "null")), mode = "raw", block = 3000L)
welcome_raw <- recv(req, mode = "raw", block = 5000L)
if (is_err(welcome_raw) || length(welcome_raw) == 0) stop("[frontend] no welcome (is the orchestrator up?)")
welcome <- fromJSON(unframe(welcome_raw), simplifyVector = FALSE)
if (!isTRUE(welcome$ok)) stop("[frontend] hello rejected: ", if (is.null(welcome$error)) "?" else welcome$error)
session_id  <- welcome$session_id
channel_url <- welcome$channel_url
cat(sprintf("[frontend] session=%s channel=%s\n", session_id, channel_url))
close(req)

# ── 2. Data channel ───────────────────────────────────────────────────────────
ch <- socket("poly", dial = channel_url)
on.exit(close(ch), add = TRUE)

# ── 3. Submit a single TTest work (jaspTTests, TTestIndependentSamples) ───────
# Minimal options: one dependent variable "x", one grouping variable "group", no plots.
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
  v = 1L, type = "work", id = "work-provision",
  work_id = "w-provision", revision = 0L,
  dataset_ids = list("ds-001"),
  kind = "analysis",
  payload = list(
    module = "jaspTTests", module_version = "0.95.5",
    analysis = "TTestIndependentSamples",
    options = options,
    settings = list(ppi = 96L, numDecimals = 3L)
  )
)

raw <- pack(toJSON(work, auto_unbox = TRUE, null = "null"))
cat("[frontend] submitting work w-provision (jaspTTests / TTestIndependentSamples)...\n")
send(ch, raw, mode = "raw", block = 3000L)

# ── 4. Wait for a `complete` result (tolerating `running` markers) ───────────
# The provisioner may take several seconds to spawn a runner and complete the analysis.
# We retry up to 60 times with a 5s recv timeout each (up to 5 minutes total).
for (attempt in 1:60) {
  res <- recv(ch, mode = "raw", block = 5000L)
  if (is_err(res) || length(res) == 0) {
    cat(sprintf("[frontend] attempt %d: no result yet (runner still starting?), retrying...\n", attempt))
    next
  }
  r <- fromJSON(unframe(res), simplifyVector = FALSE)
  if (identical(r$type, "modules")) {   # connect-time catalog push — not a result
    cat(sprintf("[frontend] module catalog received: %d module(s)\n", length(r$modules)))
    next
  }
  if (identical(as.integer(r$revision), 0L)) {
    if (identical(r$status, "complete")) {
      cat(sprintf("[frontend] SUCCESS: work_id=%s rev=%s status=%s\n", r$work_id, r$revision, r$status))
      quit(status = 0)
    } else if (identical(r$status, "running")) {
      cat(sprintf("[frontend] work is running (provisioner spawned a runner), waiting for completion...\n"))
      next
    } else {
      stop(sprintf("[frontend] FAIL: work_id=%s rev=%s status=%s (expected complete)", r$work_id, r$revision, r$status))
    }
  }
}
stop("[frontend] FAIL: no complete result received after 60 attempts (5 minutes)")
