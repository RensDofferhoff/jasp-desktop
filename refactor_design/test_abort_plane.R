#!/usr/bin/env Rscript
# Abort-plane unit test (standalone): exercises the runner's abort machinery from
# runner_jaspbase.R against a real PAIR socket pair — no jaspBase needed.
#
#   arm → pending → (nothing) → disarm cancels            : quiet run
#   arm → abort lands → checkpoint raises jaspAbort       : cooperative unwind
#   arm → abort lands → disarm flags it                   : run-boundary drain
#   arm → abort for ANOTHER work → ignored                : raced/stale guard
#
# Run: Rscript refactor_design/test_abort_plane.R

library(nanonext)
library(jsonlite)

`%||%` <- function(x, y) if (is.null(x)) y else x

# ── pull the abort-plane section out of the runner script (between markers) ──
lines <- readLines("refactor_design/runner_jaspbase.R")
start <- grep("^# ── the abort plane", lines)
end <- grep("^# ── the bridge", lines)
stopifnot(length(start) == 1, length(end) == 1, start < end)
# The section uses parse_envelope/pack_envelope/toJSON — stub pack (unused) and
# alias parse to the real framing ([u32 BE len][JSON]).
pack_envelope <- function(json) raw(0)
is_err <- function(x) {
  if (exists("is_error_value", asNamespace("nanonext"))) nanonext::is_error_value(x)
  else inherits(x, "errorValue")
}
still_pending <- function(ra) {
  tryCatch(isTRUE(unresolved(ra$data)), error = function(e) TRUE)
}
parse_envelope <- function(raw) {
  n <- as.integer(raw[1]) * 16777216L + as.integer(raw[2]) * 65536L +
       as.integer(raw[3]) * 256L + as.integer(raw[4])
  list(json = rawToChar(raw[5:(4 + n)]))
}
env <- new.env()
eval(parse(text = paste(lines[(start + 1):(end - 1)], collapse = "\n")), envir = env)

frame <- function(lst) {
  json <- toJSON(lst, auto_unbox = TRUE, null = "null")
  body <- charToRaw(json)
  as.raw(c(as.integer(body) %/% 256^3 %% 256, as.integer(body) %/% 256^2 %% 256,
           as.integer(body) %/% 256 %% 256, as.integer(body) %% 256, body)) |> (\(x) x)()
}
# (big-endian length prefix, byte-correct)
frame <- function(lst) {
  json <- toJSON(lst, auto_unbox = TRUE, null = "null")
  body <- charToRaw(json)
  n <- length(body)
  prefix <- as.raw(c(bitwShiftR(n, 24) %% 256, bitwShiftR(n, 16) %% 256,
                     bitwShiftR(n, 8) %% 256, n %% 256))
  c(prefix, body)
}

WORK <- list(work_id = "w-1", revision = 3, session_id = "s-9", id = "t-1")
abort_msg <- function(wid) list(v = 1, id = "a", type = "abort", work_id = wid)

pass <- 0; fail <- 0
check <- function(label, cond) {
  if (isTRUE(cond)) { pass <<- pass + 1; cat(sprintf("ok    %s\n", label)) }
  else { fail <<- fail + 1; cat(sprintf("FAIL  %s\n", label)) }
}

s <- socket("pair", listen = "inproc://abort-plane-test")
c <- socket("pair", dial = "inproc://abort-plane-test")
Sys.sleep(0.1)

# 1. Quiet run: arm, nothing arrives, disarm cancels cleanly.
env$arm_abort_plane(s, WORK)
Sys.sleep(0.1)
check("quiet run: no abort flagged", !isTRUE(env$disarm_abort_plane(s)))

# 2. Cooperative unwind: abort lands mid-run, checkpoint raises jaspAbort.
env$arm_abort_plane(s, WORK)
send(c, frame(abort_msg("w-1")), mode = "raw")
Sys.sleep(0.2)
raised <- tryCatch({ env$jasp_checkpoint(); FALSE },
                   jaspAbort = function(c) TRUE,
                   error = function(e) FALSE)
check("checkpoint raises jaspAbort for the running work", raised)
check("flag consumed by disarm after checkpoint", !isTRUE(env$disarm_abort_plane(s)))

# 3. Run-boundary drain: abort lands, no checkpoint call, disarm flags it.
env$arm_abort_plane(s, WORK)
send(c, frame(abort_msg("w-1")), mode = "raw")
Sys.sleep(0.2)
check("boundary drain flags the abort", isTRUE(env$disarm_abort_plane(s)))

# 4. Raced/stale guard: an abort for a DIFFERENT work id is ignored.
env$arm_abort_plane(s, WORK)
send(c, frame(abort_msg("other-work")), mode = "raw")
Sys.sleep(0.2)
check("abort for another work ignored", !isTRUE(env$disarm_abort_plane(s)))
raised2 <- tryCatch({ env$jasp_checkpoint(); FALSE },
                    jaspAbort = function(c) TRUE, error = function(e) FALSE)
check("checkpoint stays quiet for foreign aborts", !raised2)

cat(sprintf("\n%d passed, %d failed\n", pass, fail))
quit(status = if (fail > 0) 1 else 0)
