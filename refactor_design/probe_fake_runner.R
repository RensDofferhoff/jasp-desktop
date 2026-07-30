#!/usr/bin/env Rscript
# Fake runner for the module-discovery e2e test — a stand-in for runner_jaspbase.R that skips
# the analysis engine entirely (no jaspBase, no library): register, advertise ONE module WITH a
# `base_uri`, dial the data channel, and sit there until killed. That is all discovery needs —
# the orchestrator merges the advertisement into its catalog on `register` and drops it when the
# socket closes. Modeled on probe_register_disconnect.R.
#
# Usage: Rscript refactor_design/probe_fake_runner.R <control_url> <module> <version> <base_uri>

args <- commandArgs(trailingOnly = TRUE)
if (length(args) < 4) stop("usage: probe_fake_runner.R <control_url> <module> <version> <base_uri>")
url      <- args[1]
module   <- args[2]
version  <- args[3]
base_uri <- args[4]

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

# Handshake on the control endpoint (REQ/REP): register advertising the fake module, base_uri
# included — that field on the wire is part of what the discovery e2e verifies.
req <- socket("req", dial = url)
reg <- list(
  v = 1L, id = sprintf("fake-reg-%s", Sys.getpid()), type = "register",
  runner_id = sprintf("fake-runner-%s", Sys.getpid()),
  capabilities = list(list(kind = "analysis", name = module, version = version,
                           base_uri = base_uri)),
  priority = 0L, environment = list()
)
send(req, pack(toJSON(reg, auto_unbox = TRUE, null = "null")), mode = "raw", block = 3000L)
ack <- fromJSON(unframe(recv(req, mode = "raw", block = 5000L)), simplifyVector = FALSE)
if (!isTRUE(ack$ok)) stop("registration rejected: ", if (is.null(ack$reason)) "?" else ack$reason)
cat(sprintf("[fake-runner] registered as %s advertising %s@%s base_uri=%s\n",
            ack$runner_id, module, version, base_uri))
close(req)

# Dial the data channel and stay alive until killed (SIGTERM from the test driver). The closing
# socket makes the orchestrator evict us — the disconnect the test observes as a catalog push.
ch <- socket("poly", dial = ack$channel_url)
cat("[fake-runner] holding connection; waiting to be killed\n")
while (TRUE) Sys.sleep(3600)
