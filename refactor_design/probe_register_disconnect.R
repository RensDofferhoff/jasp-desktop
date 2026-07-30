#!/usr/bin/env Rscript
# Cross-language probe (Rust↔R): REQ-dial the orchestrator's control endpoint, register, read the
# register_ack (with the assigned runner_id + channel_url), PAIR-dial the dedicated data channel,
# stay connected briefly, then disconnect. The Rust test `r_runner_disconnect_is_detected` verifies
# the orchestrator registers the R runner and then evicts it via pipe_notify (RemovePost) when this
# process closes its channel. See refactor_design/orchestrator-design.md (§3, §9).
#
# Usage: Rscript refactor_design/probe_register_disconnect.R [control_url]

args <- commandArgs(trailingOnly = TRUE)
url <- if (length(args) >= 1) args[1] else "tcp://127.0.0.1:9555"

suppressMessages({
  library(nanonext)
  library(jsonlite)
})

# §18.1 framing.
pack <- function(json_text) {
  jb <- charToRaw(json_text)
  c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb)
}
unframe <- function(msg) {
  len <- readBin(msg[1:4], "integer", n = 1L, size = 4L, endian = "big")
  rawToChar(msg[5:(4 + len)])
}

# 1. Handshake on the control endpoint (REQ/REP).
req <- socket("req", dial = url)
on.exit(try(close(req), silent = TRUE), add = TRUE)
reg <- list(
  v = 1L, id = "probe-reg", type = "register", runner_id = "r-probe",
  capabilities = list(list(kind = "analysis", name = "jaspTTests", version = "0.95.5")),
  priority = 0L, environment = list()
)
send(req, pack(toJSON(reg, auto_unbox = TRUE, null = "null")), mode = "raw", block = 3000L)
ack_raw <- recv(req, mode = "raw", block = 5000L)
ack <- fromJSON(unframe(ack_raw), simplifyVector = FALSE)
if (!isTRUE(ack$ok)) stop("registration rejected: ", if (is.null(ack$reason)) "?" else ack$reason)
channel_url <- ack$channel_url
if (is.null(channel_url) || !nzchar(channel_url)) stop("no channel_url in register_ack")
cat(sprintf("[probe] registered runner_id=%s channel=%s\n", ack$runner_id, channel_url))
close(req)

# 2. PAIR-dial the dedicated data channel ("poly" = pair1, compatible with the orchestrator's
#    mono Pair1 listener), stay connected briefly, then disconnect -> orchestrator evicts us.
ch <- socket("poly", dial = channel_url)
Sys.sleep(0.5)
close(ch)
cat("[probe] disconnected\n")
