#!/usr/bin/env Rscript
# Self-contained orchestrator + runner integration test.
# No JASP needed — connects to both sockets, sends a mock work,
# receives the runner's computed result. Also validates the smoke
# test (runner socket receives work).
#
# Usage:
#   1. Start the orchestrator: cargo run --manifest-path orchestrator/Cargo.toml
#   2. Run this test:           Rscript refactor_design/integration_test.R

suppressMessages({
  library(nanonext)
  library(jsonlite)
})

# ── framing (Section 18.1) ───────────────────────────────────────────────────

pack <- function(json_text) {
  jb <- charToRaw(json_text)
  c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb)
}

parse_frame <- function(msg) {
  len <- readBin(msg[1:4], "integer", n = 1L, size = 4L, endian = "big")
  if (length(msg) < 4 + len) stop("frame too short")
  list(json = rawToChar(msg[5:(4 + len)]),
       bin  = if (length(msg) > 4 + len) msg[(5 + len):length(msg)] else raw(0))
}

is_err <- function(x) {
  if (exists("is_error_value", asNamespace("nanonext"))) nanonext::is_error_value(x)
  else inherits(x, "errorValue")
}

pass <- 0L; fail <- 0L
check <- function(desc, cond) {
  if (isTRUE(cond)) { cat(sprintf("  PASS: %s\n", desc)); pass <<- pass + 1L }
  else              { cat(sprintf("  FAIL: %s\n", desc)); fail <<- fail + 1L }
}

# ── connect ──────────────────────────────────────────────────────────────────

fe_url <- Sys.getenv("JASP_ORCH_URL", "tcp://127.0.0.1:9555")
rn_url <- Sys.getenv("JASP_RUNNER_URL", "tcp://127.0.0.1:9556")

cat(sprintf("=== integration test ===\n"))
cat(sprintf("frontend: %s\n", fe_url))
cat(sprintf("runner:   %s\n", rn_url))

fe <- tryCatch(socket("poly", dial = fe_url), error = function(e) {
  cat(sprintf("FATAL: cannot dial frontend socket: %s\n", conditionMessage(e)))
  cat("Is the orchestrator running?\n")
  quit(status = 1)
})
cat("[ok] dialled frontend socket\n")

rn <- tryCatch(socket("poly", dial = rn_url), error = function(e) {
  cat(sprintf("FATAL: cannot dial runner socket: %s\n", conditionMessage(e)))
  quit(status = 1)
})
cat("[ok] dialled runner socket\n")

on.exit({ close(fe); close(rn) }, add = TRUE)

# -- T0: runner registers with the orchestrator --

cat("\n-- T0: runner registration --\n")

reg_json <- toJSON(list(
  v = 1, id = "rn-reg-1", type = "register",
  runner_id = "test-runner",
  capabilities = list(
    list(kind = "analysis", name = "test", version = "0.1")
  ),
  priority = 0,
  environment = setNames(list(), character(0))
), auto_unbox = TRUE, null = "null")

send(rn, pack(reg_json), mode = "raw", block = 3000L)
cat("[ok] sent register on runner socket\n")

ack_raw <- recv(rn, mode = "raw", block = 5000L)
check("runner received register_ack", !is_err(ack_raw) && length(ack_raw) > 0)
ack <- fromJSON(parse_frame(ack_raw)$json, simplifyVector = FALSE)
check("register_ack type", identical(ack$type, "register_ack"))
check("register_ack ok", isTRUE(ack$ok))
check("register_ack carries assigned runner_id", is.character(ack$runner_id) && nzchar(ack$runner_id))
if (is.character(ack$runner_id)) cat(sprintf("  assigned runner_id: %s\n", ack$runner_id))

# -- T1: frontend -> orchestrator -> runner (work forwarding) --

work_json <- toJSON(list(
  v = 1, id = "int-test-1", type = "work",
  work_id = "w-int-test", revision = 0,
  dataset_ids = list("ds-001"),
  kind = "analysis",
  payload = list(
    module = "test", module_version = "0.1", analysis = "test",
    options = setNames(list(), character(0)),
    settings = list(ppi = 96, numDecimals = 3)
  )
), auto_unbox = TRUE, null = "null")

send(fe, pack(work_json), mode = "raw", block = 3000L)
cat("[ok] sent work on frontend socket\n")

# Runner receives the forwarded work.
rn_msg <- recv(rn, mode = "raw", block = 5000L)
check("runner received a message", !is_err(rn_msg) && length(rn_msg) > 0)

parsed <- parse_frame(rn_msg)
work <- fromJSON(parsed$json, simplifyVector = FALSE)
check("message type is work", identical(work$type, "work"))
check("work_id echoed", identical(work$work_id, "w-int-test"))
check("revision echoed", identical(work$revision, 0L))
check("dataset_paths injected", !is.null(work$dataset_paths))
check("output_dir injected", !is.null(work$output_dir))

cat(sprintf("  dataset_paths: %s\n", toJSON(work$dataset_paths, auto_unbox = TRUE)))
cat(sprintf("  output_dir:    %s\n", work$output_dir))

# ── T2: runner -> orchestrator -> frontend (result forwarding) ───────────────

cat("\n-- T2: result forwarding (runner -> orchestrator -> frontend) --\n")

result_json <- toJSON(list(
  v = 1, id = "rn-int-1", reply_to = work$id,
  type = "result",
  work_id = work$work_id, revision = work$revision,
  status = "complete",
  payload = list(results = list(title = "Integration test", data = list()))
), auto_unbox = TRUE, null = "null")

send(rn, pack(result_json), mode = "raw", block = 3000L)
cat("[ok] sent result on runner socket\n")

# Frontend receives the forwarded result.
fe_msg <- recv(fe, mode = "raw", block = 5000L)
check("frontend received a message", !is_err(fe_msg) && length(fe_msg) > 0)

parsed2 <- parse_frame(fe_msg)
result <- fromJSON(parsed2$json, simplifyVector = FALSE)
check("result type is result", identical(result$type, "result"))
check("result work_id matches", identical(result$work_id, "w-int-test"))
check("result revision matches", identical(result$revision, 0L))
check("result status is complete", identical(result$status, "complete"))
check("reply_to references the work id", identical(result$reply_to, work$id))

# ── summary ──────────────────────────────────────────────────────────────────

cat(sprintf("\n=== %d passed, %d failed ===\n", pass, fail))
if (fail > 0) quit(status = 1)
