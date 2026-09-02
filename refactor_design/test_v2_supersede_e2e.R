#!/usr/bin/env Rscript
# v2 e2e smoke: supersession over the REAL runner (jaspBase-backed).
#
# frontend → jasp-orchestrator-v2 → runner_jaspbase.R (JASP_RUNNER_LIBDIR):
#   1. submit analysis w-0 rev 1        → dispatches
#   2. submit w-0 rev 2 quickly         → abort push; rev 1's result races it
#   3. the frontend must see ONLY rev 2 complete (stale rev-1 discarded by
#      revision; the abort popped at the runner's loop-top is a no-op)
#
# Run: Rscript refactor_design/test_v2_supersede_e2e.R
library(nanonext)
library(jsonlite)

orch_bin <- "orchestrator/target/debug/jasp-orchestrator-v2"
stopifnot(file.exists(orch_bin))

port <- 19700 + (Sys.getpid() %% 500)
url <- sprintf("tcp://127.0.0.1:%d", port)
root <- sprintf("/tmp/jasp-v2-e2e-%d", Sys.getpid())

orch <- system2(orch_bin, NULL, stdout = "/tmp/v2-e2e-orch.log", stderr = "/tmp/v2-e2e-orch.log",
                env = c(paste0("JASP_ORCH_URL=", url), paste0("JASP_ORCH_DIR_ROOT=", root)),
                wait = FALSE)
Sys.sleep(0.8)
on.exit({ tools::pskill(orch); unlink(root, recursive = TRUE) }, add = TRUE)

runner <- system2("Rscript", c("refactor_design/runner_jaspbase.R", url),
                  stdout = "/tmp/v2-e2e-runner.log", stderr = "/tmp/v2-e2e-runner.log",
                  env = "JASP_RUNNER_LIBDIR=/home/sp42/jaspModuleTools/workdir/jaspTTests",
                  wait = FALSE)
on.exit({ tools::pskill(runner) }, add = TRUE)

# frontend handshake (REQ hello → welcome → PAIR dial), then eat the catalog frame.
frame <- function(lst) {
  body <- charToRaw(toJSON(lst, auto_unbox = TRUE, null = "null"))
  n <- length(body)
  as.raw(c(bitwShiftR(n, 24) %% 256, bitwShiftR(n, 16) %% 256, bitwShiftR(n, 8) %% 256, n %% 256, body))
}
req <- socket("req", dial = url)
hello <- list(v = 1, id = "fe-hello", type = "hello", client_id = "e2e")
send(req, frame(hello), mode = "raw")
ack <- recv(req, mode = "raw", block = 5000)
welcome <- fromJSON(rawToChar(ack[5:length(ack)]), simplifyVector = FALSE)
stopifnot(isTRUE(welcome$ok))
session <- welcome$session_id
fe <- socket("poly", dial = welcome$channel_url)
Sys.sleep(0.3)
recv(fe, mode = "raw", block = 1000)  # catalog frame

# Open a real dataset through the data lane first (the analysis needs data).
csv <- normalizePath("test_data/debug.csv")
open_work <- list(
  v = 1, id = "w-open", type = "work", work_id = "w-open", revision = 0,
  dataset_ids = list(), kind = "data",
  payload = list(op = "data_open", source = csv, cache_path = "", format = "csv",
                 ingest = list()))
send(fe, frame(open_work), mode = "raw")
deadline <- Sys.time() + 30
dataset_id <- NULL
repeat {
  raw <- tryCatch(recv(fe, mode = "raw", block = 1000), error = function(e) NULL)
  if (!is.null(raw) && length(raw) > 4) {
    n <- sum(as.integer(raw[1:4]) * 256^(3:0))
    msg <- fromJSON(rawToChar(raw[5:(4 + n)]), simplifyVector = FALSE)
    if ((msg$type %||% "") == "result" && msg$work_id == "w-open" &&
        msg$status == "complete") {
      dataset_id <- msg$payload$dataset_id
      break
    }
  }
  if (Sys.time() > deadline) { cat("E2E FAIL: dataset open never completed\n"); quit(status = 1) }
}
cat(sprintf("dataset %s ready; submitting\n", dataset_id))

work <- function(rev) list(
  v = 1, id = sprintf("w-%d", rev), type = "work",
  work_id = "w-0", revision = rev, dataset_ids = list(dataset_id),
  kind = "analysis_r_classic_jaspbase",
  payload = list(module = "jaspTTests", module_version = "0.95.5",
                 analysis = "TTestIndependentSamples",
                 options = list(
                   `.meta` = list(dependent = list(shouldEncode = TRUE),
                                  group = list(shouldEncode = TRUE)),
                   alternative = "twoSided",
                   barPlot = FALSE, barPlotCiLevel = 0.95, barPlotErrorType = "ci",
                   barPlotYAxisFixedToZero = TRUE,
                   dependent = list(types = list(), value = list("x")),
                   descriptives = FALSE, descriptivesPlot = FALSE, descriptivesPlotCiLevel = 0.95,
                   effectSize = FALSE, effectSizeCi = FALSE, effectSizeCiLevel = 0.95,
                   effectSizeType = "cohen",
                   equalityOfVariancesTest = FALSE, equalityOfVariancesTestType = "brownForsythe",
                   group = list(types = list(), value = "group"),
                   mannWhitneyU = FALSE, meanDifference = FALSE, meanDifferenceCi = FALSE,
                   meanDifferenceCiLevel = 0.95,
                   naAction = "perDependent", normalityTest = FALSE,
                   plotHeight = 300L, plotWidth = 350L,
                   qqPlot = FALSE, qqPlotCi = FALSE, qqPlotCiLevel = 0.95,
                   raincloudPlot = FALSE, raincloudPlotHorizontal = FALSE,
                   student = TRUE, vovkSellke = FALSE, welch = FALSE),
                 preloadData = TRUE,
                 settings = list(ppi = 96L, numDecimals = 3L)))

# Wait for the runner to finish booting (jaspBase load can take a minute+):
# poll its log for the post-registration line.
deadline <- Sys.time() + 180
repeat {
  log <- tryCatch(readLines("/tmp/v2-e2e-runner.log", warn = FALSE), error = function(e) "")
  if (any(grepl("waiting for work", log))) break
  if (Sys.time() > deadline) { cat("E2E FAIL: runner never registered\n"); quit(status = 1) }
  Sys.sleep(1)
}
cat("runner is up; submitting\n")

send(fe, frame(work(1)), mode = "raw")
send(fe, frame(work(2)), mode = "raw")

# Collect results for ~30s; we want exactly: [running?] ... complete rev 2 (rev 1 never
# terminal-shaped for the frontend).
deadline <- Sys.time() + 30
seen <- list()
repeat {
  raw <- tryCatch(recv(fe, mode = "raw", block = 1000), error = function(e) NULL)
  if (!is.null(raw) && length(raw) > 4) {
    n <- sum(as.integer(raw[1:4]) * 256^(3:0))
    msg <- fromJSON(rawToChar(raw[5:(4 + n)]), simplifyVector = FALSE)
    if ((msg$type %||% "") == "result") seen[[length(seen) + 1]] <- msg
  }
  done <- any(vapply(seen, function(r) r$work_id == "w-0" && r$revision == 2 &&
                                  r$status %in% c("complete", "fatalError"), FALSE))
  if (done || Sys.time() > deadline) break
}
`%||%` <- function(x, y) if (is.null(x)) y else x
statuses <- vapply(seen, function(r) sprintf("rev=%s %s", r$revision, r$status), "")
cat("frontend saw:\n"); print(statuses)
for (r in seen) {
  if (r$status %in% c("fatalError", "validationError"))
    cat(sprintf("  rev=%s error: %s\n", r$revision,
                r$payload$results$errorMessage %||% r$message %||% "?"))
}
bad <- any(vapply(seen, function(r) r$revision == 1 && r$status %in% c("complete", "fatalError", "validationError", "aborted"), FALSE))
ok2 <- any(vapply(seen, function(r) r$revision == 2 && r$status == "complete", FALSE))
cat(if (ok2 && !bad) "E2E OK: only rev 2 completed\n" else "E2E FAIL\n")
quit(status = if (ok2 && !bad) 0 else 1)
