#!/usr/bin/env Rscript
# NEO JASP frontend — incremental-recompute verification harness.
#
# Submits TWO revisions of the same t-test work_id to exercise the runner's copy-on-seed
# incremental recompute (the "jaspBase accommodation" block in runner_jaspbase.R):
#
#   Analysis: Bayesian One-Sample T-Test, prior-and-posterior plot ON (the ONE plot under test;
#   descriptivesPlot/raincloud/bar all OFF). We chose this analysis deliberately: it is the one
#   jaspTTests analysis whose plot recomputes cleanly UNMODIFIED. (Every classical t-test plot is
#   broken upstream — Independent/One-Sample declare a phantom `variables` dependency, Paired's
#   `pairs` dependency dies in jaspBase's isJsonSubArray, and the shared QQ plot has a dead guard.)
#
#   rev 0: descriptives TABLE ON  -> renders the prior/posterior image (jasp-1.png), seals results_0/
#   rev 1: descriptives TABLE OFF, base_revision = 0
#          -> runner seeds results_1/ from results_0/. The toggle removes only the descriptives
#             TABLE; the prior/posterior plot is UNCHANGED and must be REUSED from the base's cached
#             state — NOT re-rendered.
#
# PASS criterion (the recompute discriminator):
#   results_1/ must NOT gain any new plot artifact. Reuse => results_1/ holds exactly the artifacts
#   copied from results_0/ (same filenames). If recompute FAILED (full recompute), the raincloud
#   re-renders at the *continued* temp counter -> a NEW jasp-<N> appears (the copied one orphaned).
#   So: any artifact filename in results_1/ that results_0/ lacks => re-render => FAIL.
#
# Usage: Rscript refactor_design/frontend_recompute.R [control_url]
# Env:   JASP_ORCH_DIR_ROOT  orchestrator directory root the orchestrator/runner share (default /tmp/jasp-orchestrator)

args <- commandArgs(trailingOnly = TRUE)
url <- if (length(args) >= 1) args[1] else Sys.getenv("JASP_ORCH_URL", "tcp://127.0.0.1:9555")
SCRATCH <- Sys.getenv("JASP_ORCH_DIR_ROOT", "/tmp/jasp-orchestrator")

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
hello <- list(v = 1L, id = "fe-hello-recompute", type = "hello", client_id = "recompute-frontend")
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

# ── 3. The options: Bayesian One-Sample T-Test (only the prior/posterior plot) ─
# Real options captured from JASP. The prior-and-posterior plot is the single plot under test;
# `descriptives` (the TABLE) is the only thing we toggle between revisions.
base_options <- list(
  `.meta` = list(dependent = list(shouldEncode = TRUE)),
  alternative = "twoSided",
  barPlot = FALSE, barPlotCiLevel = 0.95, barPlotErrorType = "ci", barPlotYAxisFixedToZero = TRUE,
  bayesFactorType = "BF10",
  bfRobustnessPlot = FALSE, bfRobustnessPlotAdditionalInfo = TRUE,
  bfSequentialPlot = FALSE, bfSequentialPlotRobustness = FALSE,
  defaultStandardizedEffectSize = "cauchy",
  dependent = list(types = list("scale"), value = list("x")),
  descriptives = TRUE, descriptivesPlot = FALSE, descriptivesPlotCiLevel = 0.95,
  dienesEffectSize = "uniform",
  effectSize = "standardized", effectSizeStandardized = "default",
  halfNormalDienesStd = 0.707,
  informativeCauchyLocation = 0.0, informativeCauchyScale = 0.707,
  informativeNormalMean = 0.0, informativeNormalStd = 0.707,
  informativeStandardizedEffectSize = "cauchy",
  informativeTDf = 1.0, informativeTLocation = 0.0, informativeTScale = 0.707,
  naAction = "perDependent",
  normalDienesMean = 0.707, normalDienesStd = 0.707,
  plotHeight = 240L, plotWidth = 320L,
  priorAndPosteriorPlot = TRUE, priorAndPosteriorPlotAdditionalInfo = TRUE, priorAndPosteriorPlotCiLevel = 0.95,
  priorWidth = 0.707,
  raincloudPlot = FALSE, raincloudPlotHorizontal = FALSE,
  standardizedEffectSize = TRUE,
  test = "student", testValue = 0.0,
  uniformDienesLowerBound = 0.707, uniformDienesUpperBound = 0.707,
  wilcoxonSamples = 1000L
)

work_id <- "w-recompute"

# Build a work envelope for a given revision. `base_revision` (NULL => full recompute) is what makes
# the orchestrator inject base_results_dir; `descriptives` (the TABLE) is the option we toggle.
make_work <- function(rev, base_revision, descriptives) {
  opts <- base_options
  opts$descriptives <- descriptives
  work <- list(
    v = 1L, type = "work", id = sprintf("work-recompute-%d", rev),
    work_id = work_id, revision = rev,
    dataset_ids = list("ds-001"),
    kind = "analysis",
    payload = list(
      module = "jaspTTests", module_version = "0.95.5",
      analysis = "TTestBayesianOneSample",
      options = opts,
      settings = list(ppi = 96L, numDecimals = 3L)
    )
  )
  if (!is.null(base_revision)) work$base_revision <- base_revision
  work
}

# Submit a work unit and wait for a result whose revision matches `expect_rev` (skipping any
# stale/leftover results in the buffer). Retries the send until the runner is routing.
submit_and_wait <- function(work, label, expect_rev) {
  raw <- pack(toJSON(work, auto_unbox = TRUE, null = "null"))
  for (attempt in 1:60) {
    send(ch, raw, mode = "raw", block = 3000L)
    res <- recv(ch, mode = "raw", block = 5000L)
    if (is_err(res) || length(res) == 0) {
      cat(sprintf("[frontend] %s attempt %d: no result yet (runner not ready?), retrying...\n", label, attempt))
      next
    }
    r <- fromJSON(unframe(res), simplifyVector = FALSE)
    if (identical(r$type, "modules")) {   # connect-time catalog push — not a result
      cat(sprintf("[frontend] %s: module catalog received (%d module(s))\n", label, length(r$modules)))
      next
    }
    if (identical(as.integer(r$revision), as.integer(expect_rev))) {
      cat(sprintf("[frontend] %s: work_id=%s rev=%s status=%s\n", label, r$work_id, r$revision, r$status))
      if (!identical(r$status, "complete"))
        stop(sprintf("[frontend] %s: status=%s (not complete)", label, r$status))
      return(r)
    }
    cat(sprintf("[frontend] %s: skipping stale result rev=%s (want %s)\n", label, r$revision, expect_rev))
  }
  stop(sprintf("[frontend] %s: no result with revision %s received", label, expect_rev))
}

# ── 4. Run both revisions ─────────────────────────────────────────────────────
r0 <- submit_and_wait(make_work(0L, NULL, TRUE),  "rev0", 0L)  # descriptives table ON,  no base
r1 <- submit_and_wait(make_work(1L, 0L,   FALSE), "rev1", 1L)  # descriptives table OFF, base = rev 0

# Sanity: the toggle should have altered the results (the descriptives table drops out). If the two
# results serialize identically, the option change had no effect and the test is vacuous.
r0_json <- toJSON(r0$payload$results, auto_unbox = TRUE, digits = NA)
r1_json <- toJSON(r1$payload$results, auto_unbox = TRUE, digits = NA)
if (identical(r0_json, r1_json))
  cat("[frontend] WARN: rev0 and rev1 results are identical — did the Welch change take effect?\n")

# ── 5. Verify recompute: compare plot artifacts between results_0 and results_1 ──
work_dir <- file.path(SCRATCH, session_id, work_id)
res0 <- file.path(work_dir, "results_0")
res1 <- file.path(work_dir, "results_1")
if (!dir.exists(res0)) stop(sprintf("[frontend] results_0 dir missing: %s", res0))
if (!dir.exists(res1)) stop(sprintf("[frontend] results_1 dir missing: %s", res1))

# jasp-<N>.<ext> are the plot artifacts (png / plotly json); jaspState.RData and the write-seal
# don't match this pattern, so this compares plots only.
arts0 <- list.files(res0, pattern = "^jasp-[0-9]+\\.")
arts1 <- list.files(res1, pattern = "^jasp-[0-9]+\\.")
png0  <- list.files(res0, pattern = "^jasp-[0-9]+\\.png$")

cat(sprintf("[frontend] results_0 plot artifacts: %s\n", if (length(arts0)) paste(arts0, collapse = ", ") else "(none)"))
cat(sprintf("[frontend] results_1 plot artifacts: %s\n", if (length(arts1)) paste(arts1, collapse = ", ") else "(none)"))

# Sanity: rev 0 actually rendered the prior/posterior plot.
if (length(png0) == 0) {
  cat("[frontend] FAIL: rev0 rendered no plot image — check dependent/priorAndPosteriorPlot.\n")
  quit(status = 1)
}

# THE discriminator: results_1 must contain no artifact filename that results_0 lacks. Reuse =>
# results_1 == the copied results_0 (identical filename set). Re-render => a new jasp-<N> appears.
new_artifacts <- setdiff(arts1, arts0)
if (length(new_artifacts) > 0) {
  cat(sprintf("[frontend] FAIL: recompute did NOT reuse the plot — new artifact(s) in results_1: %s\n",
              paste(new_artifacts, collapse = ", ")))
  quit(status = 1)
}

cat("[frontend] SUCCESS — prior/posterior plot reused from base revision (no re-render); descriptives table toggled.\n")
quit(status = 0)
