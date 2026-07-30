#!/usr/bin/env Rscript
# Validate nng pipe_notify under PAIR v1 ("poly") over TCP — the disconnect-detection
# mechanism the orchestrator's runner registry will rely on (HANDOVER Phase 2, neo-jasp §25.5).
#
# The PoC validated this under PAIR v0 (where it also produced spurious signals); everything now
# runs PAIR v1 and this was never re-validated. This script answers:
#   T1  does pipe ADD fire?            T4  any spurious signals when idle?
#   T2  does recv still work alongside pipe_notify?   T5  sequential add/remove with N peers?
#   T3  does pipe REMOVE fire (the disconnect case)?  T6  message-vs-disconnect distinguisher
#
# Run: Rscript refactor_design/validate_pipe_notify.R
suppressMessages(library(nanonext))

urlA <- "tcp://127.0.0.1:19711"
urlB <- "tcp://127.0.0.1:19712"
pass <- 0L; fail <- 0L
check <- function(desc, ok) {
  cat(sprintf("  [%s] %s\n", if (isTRUE(ok)) "PASS" else "FAIL", desc))
  if (isTRUE(ok)) pass <<- pass + 1L else fail <<- fail + 1L
}

cat("=== pipe_notify under PAIR v1 (poly), TCP ===\n")

# ---- Part A: add / remove / spurious / multi-peer on one socket ----
srv <- socket("poly", listen = urlA)
cvp <- cv()
pipe_notify(srv, cvp, add = TRUE, remove = TRUE, flag = TRUE)

# T1: ADD
r1 <- socket("poly", dial = urlA)
a1 <- until(cvp, 3000); v_add <- cv_value(cvp)
check(sprintf("T1 pipe ADD fires (cv_value=%s)", v_add), a1)

# T2: recv still works on a pipe_notify'd socket
send(r1, charToRaw("ping"), mode = "raw", block = 2000)
m <- tryCatch(recv(srv, mode = "raw", block = 2000), error = function(e) NULL)
check("T2 recv works alongside pipe_notify", !is.null(m) && length(m) > 0 && rawToChar(m) == "ping")

# T3: REMOVE (the disconnect case)
close(r1)
rem1 <- until(cvp, 3000); v_rem <- cv_value(cvp)
check(sprintf("T3 pipe REMOVE fires on disconnect (cv_value=%s)", v_rem), rem1)
check("T3 add vs remove are distinguishable by cv_value", v_add != v_rem)

# T4: spurious — no events for 1.5s should NOT signal
spur <- until(cvp, 1500)
check("T4 no spurious signal when idle", !spur)

# T5: multiple peers, sequential add/remove
r2 <- socket("poly", dial = urlA); a2 <- until(cvp, 3000)
r3 <- socket("poly", dial = urlA); a3 <- until(cvp, 3000)
check("T5 second ADD fires", a2)
check("T5 third ADD fires", a3)
close(r2); rm2 <- until(cvp, 3000)
close(r3); rm3 <- until(cvp, 3000)
check("T5 sequential REMOVE #1 fires", rm2)
check("T5 sequential REMOVE #2 fires", rm3)
check("T4b no spurious after multi-peer churn", !until(cvp, 1000))
close(srv)

# ---- Part B: message-vs-disconnect distinguisher (shared cv: recv_aio + pipe_notify) ----
# Fresh socket pair so Part A's pipe_notify registration doesn't interfere.
cat("\n--- distinguisher: one cv shared by recv_aio + pipe_notify ---\n")
srv2 <- socket("poly", listen = urlB)
cvm <- cv()
pipe_notify(srv2, cvm, remove = TRUE, flag = TRUE)
r4 <- socket("poly", dial = urlB)
Sys.sleep(0.3)  # let the add settle (we only watch removes on cvm)

# (i) arm recv + send a message -> cv should fire for the MESSAGE
ra <- recv_aio(srv2, mode = "raw", cv = cvm)
send(r4, charToRaw("data"), mode = "raw", block = 2000)
wm <- until(cvm, 3000); v_msg <- cv_value(cvm)
msg <- tryCatch(call_aio(ra)$data, error = function(e) NULL)
got_msg <- !is.null(msg) && length(msg) > 0
check(sprintf("T6 message wakes shared cv (cv_value=%s, got_msg=%s)", v_msg, got_msg), wm && got_msg)

# (ii) arm recv again, then DISCONNECT -> cv should fire for the PIPE REMOVAL, recv errors
ra2 <- recv_aio(srv2, mode = "raw", cv = cvm)
close(r4)
wd <- until(cvm, 3000); v_disc <- cv_value(cvm)
msg2 <- tryCatch(call_aio(ra2)$data, error = function(e) "ERR")
disc_detected <- wd && (inherits(msg2, "character") && msg2 == "ERR" || length(msg2) == 0 ||
                        nanonext::is_error_value(msg2))
check(sprintf("T6 disconnect wakes shared cv (cv_value=%s)", v_disc), wd)
check("T6 message vs disconnect distinguishable by cv_value", v_msg != v_disc)
check("T6 recv on dead pipe yields error/empty (runner's exit signal)", disc_detected)
close(srv2)

cat(sprintf("\n=== %d passed, %d failed ===\n", pass, fail))
if (fail > 0) quit(status = 1)
