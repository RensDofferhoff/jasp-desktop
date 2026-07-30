#!/usr/bin/env Rscript
# NEO JASP frontend — module discovery end-to-end test.
#
# Verifies the discovery flow against an orchestrator started WITH a libset:
#   1. On connect, the catalog arrives UNSOLICITED as the first frame on the data channel
#      (pushed at channel setup): jaspTTests@0.95.5, carrying a file:// base_uri whose
#      Description.qml actually exists — i.e. the frontend can lazy-load assets from it.
#   2. A `list_modules` query is answered with the same catalog, correlated by reply_to.
#   3. A fake runner (probe_fake_runner.R — registration only, no analysis engine) attaches
#      advertising a NEW module the libset does not have → the orchestrator PUSHES an updated
#      catalog containing it.
#   4. The fake runner exits → the orchestrator PUSHES a catalog with that module gone
#      (the libset module unaffected throughout).
#
# The driver manages the fake runner itself (spawn via pidfile+exec so we know its PID; kill via
# tools::pskill) so the whole lifecycle is one deterministic process — no shell-side polling.
#
# Usage: Rscript refactor_design/frontend_modules.R [control_url]
# Env:   JASP_ORCH_URL        control endpoint (default tcp://127.0.0.1:9555)
#        MODULES_FAKE_RUNNER  fake-runner probe script (default refactor_design/probe_fake_runner.R)

args <- commandArgs(trailingOnly = TRUE)
url <- if (length(args) >= 1) args[1] else Sys.getenv("JASP_ORCH_URL", "tcp://127.0.0.1:9555")
fake_runner <- Sys.getenv("MODULES_FAKE_RUNNER", "refactor_design/probe_fake_runner.R")
# What the fake runner advertises; nothing loads assets from the URI, it only has to ride the wire.
FAKE_MODULE <- "jaspFake"
FAKE_VERSION <- "9.9.9"
FAKE_BASE_URI <- "file:///tmp/jaspFake-e2e/"

suppressMessages({ library(nanonext); library(jsonlite) })

# §18.1 framing ([u32 BE len][JSON bytes]).
pack <- function(json_text) {
  jb <- charToRaw(json_text)
  c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb)
}
unframe <- function(msg) {
  len <- readBin(msg[1:4], "integer", n = 1L, size = 4L, endian = "big")
  rawToChar(msg[5:(4 + len)])
}
is_err <- function(x) if (exists("is_error_value", asNamespace("nanonext"))) nanonext::is_error_value(x) else inherits(x, "errorValue")

# "name@version" for every module in a parsed `modules` message — for order-insensitive asserts.
module_set <- function(m) sort(vapply(m$modules, function(x) sprintf("%s@%s", x$name, x$version), ""))

# Drain the channel until a `modules` message arrives (or we time out). If `reply_to_id` is
# given, only a reply correlated to it counts; otherwise only an unsolicited push (reply_to
# absent) counts — this is how we tell the query answer apart from change pushes.
wait_for_modules <- function(ch, reply_to_id = NULL, attempts = 24L) {
  for (i in seq_len(attempts)) {
    raw <- recv(ch, mode = "raw", block = 5000L)
    if (is_err(raw) || length(raw) == 0) {
      cat(sprintf("[frontend] (waiting for modules, attempt %d/%d)\n", i, attempts))
      next
    }
    env <- fromJSON(unframe(raw), simplifyVector = FALSE)
    if (!identical(env$type, "modules")) {
      cat(sprintf("[frontend] skipping non-modules message type=%s\n", env$type))
      next
    }
    want_reply <- identical(env$reply_to, reply_to_id)
    if (!is.null(reply_to_id) && !want_reply) next
    if (is.null(reply_to_id) && !is.null(env$reply_to)) next
    return(env)
  }
  stop("[frontend] FAIL: no `modules` message received")
}

# ── 1. Frontend handshake on the control endpoint ────────────────────────────
req <- socket("req", dial = url)
hello <- list(v = 1L, id = "fe-hello-modules", type = "hello", client_id = "modules-frontend")
send(req, pack(toJSON(hello, auto_unbox = TRUE, null = "null")), mode = "raw", block = 3000L)
welcome_raw <- recv(req, mode = "raw", block = 5000L)
if (is_err(welcome_raw) || length(welcome_raw) == 0) stop("[frontend] no welcome (is the orchestrator up?)")
welcome <- fromJSON(unframe(welcome_raw), simplifyVector = FALSE)
if (!isTRUE(welcome$ok)) stop("[frontend] hello rejected: ", if (is.null(welcome$error)) "?" else welcome$error)
session_id  <- welcome$session_id
channel_url <- welcome$channel_url
cat(sprintf("[frontend] session=%s channel=%s\n", session_id, channel_url))
close(req)

ch <- socket("poly", dial = channel_url)
on.exit(close(ch), add = TRUE)

# ── 2. The catalog arrives UNSOLICITED on connect (pushed at channel setup) ──
initial <- wait_for_modules(ch)   # reply_to absent → the connect-time push
mods <- module_set(initial)
cat(sprintf("[frontend] connect-time catalog: %s\n", paste(mods, collapse = ", ")))
if (!("jaspTTests@0.95.5" %in% mods))
  stop("[frontend] FAIL: libset module jaspTTests@0.95.5 not in the connect-time catalog: ",
       paste(mods, collapse = ", "))
if ("jaspFake@9.9.9" %in% mods)
  stop("[frontend] FAIL: the fake module must not be cataloged before the fake runner attaches")

# The base URI must point at a real module directory the frontend can lazy-load assets from.
tt <- Filter(function(x) identical(x$name, "jaspTTests"), initial$modules)[[1]]
if (is.null(tt$base_uri) || !startsWith(tt$base_uri, "file://"))
  stop("[frontend] FAIL: jaspTTests has no file:// base_uri: ", deparse(tt$base_uri))
tt_dir <- sub("^file://", "", tt$base_uri)
if (!file.exists(file.path(tt_dir, "Description.qml")))
  stop("[frontend] FAIL: Description.qml not loadable from base_uri ", tt$base_uri)
cat(sprintf("[frontend] base_uri OK: %s (Description.qml present)\n", tt$base_uri))

# ── 2b. The `list_modules` query path still answers, with the same content ───
query <- list(v = 1L, id = "fe-list-modules", type = "list_modules")
send(ch, pack(toJSON(query, auto_unbox = TRUE, null = "null")), mode = "raw", block = 3000L)
reply <- wait_for_modules(ch, reply_to_id = "fe-list-modules")
if (!identical(module_set(reply), mods))
  stop("[frontend] FAIL: list_modules reply differs from the connect-time catalog")
cat("[frontend] list_modules reply matches the connect-time catalog\n")

# ── 3. Attach a fake runner with a NEW module → expect a push ────────────────
# Spawn via `sh -c 'echo $$ > pidfile; exec Rscript ...'` so the pidfile holds the runner's own
# PID (exec replaces the shell in place) — system2(wait = FALSE) alone does not return it.
pidfile <- tempfile("runner_pid_")
runner_log <- "/tmp/modules_test_fake_runner.log"
cmd <- sprintf("echo $$ > '%s'; exec Rscript '%s' '%s' '%s' '%s' '%s'",
               pidfile, fake_runner, url, FAKE_MODULE, FAKE_VERSION, FAKE_BASE_URI)
system2("sh", c("-c", shQuote(cmd)), stdout = runner_log, stderr = "", wait = FALSE)
for (i in 1:50) {  # wait up to ~5s for the pidfile
  if (file.exists(pidfile) && length(readLines(pidfile, warn = FALSE)) > 0) break
  Sys.sleep(0.1)
}
runner_pid <- suppressWarnings(as.integer(readLines(pidfile, warn = FALSE)[1]))
if (is.na(runner_pid)) stop("[frontend] FAIL: fake runner did not write its pidfile")
on.exit(try(tools::pskill(runner_pid), silent = TRUE), add = TRUE)  # belt & braces on failure
cat(sprintf("[frontend] fake runner pid=%d; waiting for catalog push...\n", runner_pid))

push1 <- wait_for_modules(ch)
mods1 <- module_set(push1)
cat(sprintf("[frontend] push after attach: %s\n", paste(mods1, collapse = ", ")))
if (!all(c("jaspFake@9.9.9", "jaspTTests@0.95.5") %in% mods1))
  stop("[frontend] FAIL: push after fake-runner attach must carry both modules: ",
       paste(mods1, collapse = ", "))

# ── 4. Kill the fake runner → expect a push with the fake module gone ────────
tools::pskill(runner_pid)  # SIGTERM; the closing socket makes the orchestrator evict it
push2 <- wait_for_modules(ch)
mods2 <- module_set(push2)
cat(sprintf("[frontend] push after disconnect: %s\n", paste(mods2, collapse = ", ")))
if ("jaspFake@9.9.9" %in% mods2)
  stop("[frontend] FAIL: the fake module must be gone after the fake runner disconnects")
if (!("jaspTTests@0.95.5" %in% mods2))
  stop("[frontend] FAIL: the libset module must survive fake-runner disconnect")

cat("[frontend] SUCCESS: list_modules + push-on-actual-change verified end-to-end\n")
quit(status = 0)
