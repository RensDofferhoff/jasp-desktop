#!/usr/bin/env Rscript
# NEO JASP frontend — independent-samples t-test round-trip driver (verification harness).
#
# Does the frontend half of the new handshake (REQ `hello` -> `welcome{session_id, channel_url}` ->
# PAIR-dial the channel), then opens the dataset (`dataset_open` -> the Rust data lane converts
# test_data/debug.csv to an Arrow cache file -> `dataset_ready` carries the dataset_id), and
# submits a `work` for jaspTTests::TTestIndependentSamples (dependent = x, group = group) that
# references that dataset_id. The runner reads the CONVERTED Feather via dataset_paths. Retries
# the submission until a runner is registered and routes it (the runner loads jaspBase before
# registering, so once routed the analysis returns quickly).
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

# ── 2b. Open the dataset: submit a data_open WORK — the lane converts, the terminal result ──
# carries the dataset_id. An open IS a work (kind "data", op "data_open"): the orchestrator
# mints the dataset_id and assigns the cache path at dispatch, and the lane's terminal result
# comes back as a kind:"data" result — {dataset_id, schema, rows} on the typed Data payload
# (§19.2). Retries while the data lane has not registered yet (transient "No data lane"
# fatalError).
csv_path <- normalizePath("test_data/debug.csv")
dataset_id <- NULL
for (attempt in 1:50) {
  work_id <- sprintf("data-open-%d", attempt)
  open_work <- list(
    v = 1L, id = work_id, type = "work", work_id = work_id, revision = 0L,
    dataset_ids = list(),
    kind = "data",
    payload = list(op = "data_open", source = csv_path, cache_path = "", format = "csv",
                   ingest = list(decimal_sep = ".", threshold = 10L,
                                 nulls = list("", "NA", "NaN"), sort_limit = 2000L))
  )
  send(ch, pack(toJSON(open_work, auto_unbox = TRUE, null = "null")), mode = "raw", block = 3000L)
  # Read frames until the terminal result for THIS work arrives; skip unrelated frames.
  done <- FALSE
  repeat {
    open_raw <- recv(ch, mode = "raw", block = 10000L)
    if (is_err(open_raw) || length(open_raw) == 0) break   # timeout -> outer retry
    r <- fromJSON(unframe(open_raw), simplifyVector = FALSE)
    if (identical(r$type, "modules")) next                                  # catalog push
    if (!identical(r$type, "result") || !identical(r$work_id, work_id)) next
    if (identical(r$status, "running")) next                 # park marker: lane is booting
    msg <- if (is.null(r$payload$error_message)) "" else r$payload$error_message
    if (identical(r$status, "complete")) {
      dataset_id <- r$payload$dataset_id
      cat(sprintf("[frontend] dataset ready: id=%s rows=%s\n", dataset_id, r$payload$rows))
      done <- TRUE
      break
    }
    if (grepl("No data lane", msg)) {
      cat(sprintf("[frontend] data lane not ready yet (attempt %d), retrying...\n", attempt))
      break
    }
    stop(sprintf("[frontend] dataset_open failed: %s", msg))
  }
  if (done) break
  Sys.sleep(0.2)
}
if (is.null(dataset_id)) stop("[frontend] dataset_open never completed (is the data runner up?)")

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
  dataset_ids = list(dataset_id),
  kind = "analysis",
  payload = list(
    module = "jaspTTests", module_version = "0.95.5",
    analysis = "TTestIndependentSamples",
    options = options,
    settings = list(ppi = 96L, numDecimals = 3L)
  )
)
work_raw <- pack(toJSON(work, auto_unbox = TRUE, null = "null"))

# ── 4. Submit, then wait for the terminal result ─────────────────────────────
# The jaspbase runner is provisioned on demand: the orchestrator PARKS the work and spawns
# the runner, sending `running` park markers until the runner registers and the parked work
# is dispatched. So submit once, then keep receiving, skipping `running` markers (parked)
# and `modules` frames, until a terminal result arrives. A "No runner is available"
# fatalError (only when provisioning is disabled) triggers a re-submit.
result <- NULL
parked <- FALSE
attempt <- 0
while (is.null(result) && attempt < 60) {
  attempt <- attempt + 1
  if (!parked) {
    send(ch, work_raw, mode = "raw", block = 3000L)
  }
  res <- recv(ch, mode = "raw", block = 5000L)
  if (is_err(res) || length(res) == 0) {
    cat(sprintf("[frontend] attempt %d: no frame yet, retrying...\n", attempt))
    next
  }
  r <- fromJSON(unframe(res), simplifyVector = FALSE)
  if (identical(r$type, "modules")) next                       # catalog push
  if (!identical(r$type, "result")) {
    cat(sprintf("[frontend] skipping type=%s frame (not a result)\n", r$type))
    next
  }
  if (identical(r$status, "running")) {
    if (!parked) cat("[frontend] work parked — orchestrator is provisioning the runner...\n")
    parked <- TRUE
    next
  }
  msg <- if (is.null(r$payload$results$errorMessage)) "" else r$payload$results$errorMessage
  if (identical(r$status, "fatalError") && grepl("No runner is available", msg)) {
    cat(sprintf("[frontend] attempt %d: %s retrying...\n", attempt, msg))
    parked <- FALSE
    Sys.sleep(1)
    next
  }
  result <- r
}
if (!is.null(result) && identical(result$status, "running")) result <- NULL

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
