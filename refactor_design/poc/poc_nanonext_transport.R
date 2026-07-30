#!/usr/bin/env Rscript
# NNG / nanonext transport PoC.
# Validates the behaviors the NEO JASP design relies on (neo-jasp.md §15.5, §17.2, §25.4, §25.5),
# over BOTH ipc:// (desktop) and tcp:// (web/remote).
#
# Confirmed nanonext 1.10.1 / nng 1.12.0 semantics (from probing, identical on ipc & tcp):
#   - until(cv, msec): timeout-safe; TRUE if the cv was signaled within msec (consumes the signal).
#   - cv_value(cv): non-consuming peek; != 0 while a signal is pending; reset to 0 by wait()/until().
#   - wait(cv): blocks until signaled; with pipe_notify(flag = TRUE) returns FALSE for a pipe event,
#     TRUE for a normal signal (e.g. a recv_aio completion) -- the runner's message-vs-disconnect
#     distinguisher.
#   - send(mode =) accepts only "serial" / "raw"; use charToRaw(...) + mode = "raw" for the wire framing.

suppressMessages(library(nanonext))
cat("=== NNG / nanonext transport PoC ===\n")
cat("nanonext", as.character(packageVersion("nanonext")), "| nng", nng_version(), "\n")

pass <- 0L; fail <- 0L
check <- function(desc, cond) {
  if (isTRUE(cond)) { cat("  PASS:", desc, "\n"); pass <<- pass + 1L }
  else              { cat("  FAIL:", desc, "\n"); fail <<- fail + 1L }
}
is_err <- function(x) {
  if (exists("is_error_value", asNamespace("nanonext"))) nanonext::is_error_value(x)
  else inherits(x, "errorValue")
}

port <- 45300
make_url <- function(transport, name) {
  if (transport == "ipc") paste0("ipc://", file.path(tempdir(), sprintf("jp_%s_%d.sock", name, Sys.getpid())))
  else { port <<- port + 1L; sprintf("tcp://127.0.0.1:%d", port) }
}
cleanup_ipc <- function(url) if (startsWith(url, "ipc://")) unlink(sub("^ipc://", "", url))

# non-consuming poll: is a signal pending on the cv?
poll_cv <- function(cv, timeout_ms = 3000, poll_ms = 5) {
  t0 <- Sys.time()
  repeat {
    if (cv_value(cv) != 0) return(TRUE)
    if (as.numeric(Sys.time() - t0, "secs") * 1000 > timeout_ms) return(FALSE)
    Sys.sleep(poll_ms / 1000)
  }
}
# consume all pending signals on a cv (so the next until() corresponds to a fresh event)
drain_cv <- function(cv, max_iters = 30) for (i in seq_len(max_iters)) if (!until(cv, 150L)) break

# §18.1 framing: [uint32 big-endian json_len][JSON bytes][binary payload]
pack_envelope <- function(json_text, binary = raw(0)) {
  jb <- charToRaw(json_text)
  c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb, binary)
}
parse_envelope <- function(msg) {
  len <- readBin(msg[1:4], "integer", n = 1L, size = 4L, endian = "big")   # envelope len is << 2^31, so signed int32 BE is exact
  list(json   = rawToChar(msg[5:(4 + len)]),
       binary = if (length(msg) > 4 + len) msg[(5 + len):length(msg)] else raw(0))
}

run_suite <- function(transport) {
  cat(sprintf("\n################## TRANSPORT: %s ##################\n", transport))

  ## T1: basic PAIR send/recv (raw both directions + serial R object)
  cat("\n-- T1: basic PAIR send/recv --\n")
  u <- make_url(transport, "t1")
  L <- socket("pair", listen = u); D <- socket("pair", dial = u)
  send(L, charToRaw("ping"), mode = "raw", block = 3000L)
  r <- recv(D, mode = "raw", block = 3000L)
  check("L->D raw message", !is_err(r) && identical(rawToChar(r), "ping"))
  send(D, charToRaw("pong"), mode = "raw", block = 3000L)
  r2 <- recv(L, mode = "raw", block = 3000L)
  check("D->L raw message (bidirectional)", !is_err(r2) && identical(rawToChar(r2), "pong"))
  obj <- list(a = 1:3, b = "x")
  send(L, obj, mode = "serial", block = 3000L)
  r3 <- recv(D, mode = "serial", block = 3000L)
  check("serial R-object round-trip", !is_err(r3) && identical(r3, obj))
  close(D); close(L); cleanup_ipc(u)

  ## T2: §18.1 framing round-trip (JSON + binary)
  cat("\n-- T2: framing [uint32 BE len][JSON][binary] --\n")
  u <- make_url(transport, "t2")
  L <- socket("pair", listen = u); D <- socket("pair", dial = u)
  json <- '{"v":1,"type":"data_edit","id":"9a41","op":"replace_column","format":"raw/float64"}'
  bin  <- writeBin(c(1.1, 2.2, 3.3, 4.4, 5.5), raw())   # stand-in binary payload
  send(L, pack_envelope(json, bin), mode = "raw", block = 3000L)
  got <- recv(D, mode = "raw", block = 3000L)
  if (!is_err(got)) {
    p <- parse_envelope(got)
    check("framing: JSON envelope round-trips", identical(p$json, json))
    check("framing: binary payload round-trips byte-for-byte", identical(p$binary, bin))
  } else check("framing: message received", FALSE)
  close(D); close(L); cleanup_ipc(u)

  ## T2b: framing carrying a real Arrow IPC stream (production data-plane realism)
  cat("\n-- T2b: framing carrying an Arrow IPC stream --\n")
  if (requireNamespace("arrow", quietly = TRUE) &&
      exists("write_ipc_stream", asNamespace("arrow")) && exists("read_ipc_stream", asNamespace("arrow"))) {
    res <- tryCatch({
      suppressMessages(library(arrow))
      tbl <- arrow_table(x = c(1.0, 2.0, 3.0), g = c("a", "b", "c"))
      tf <- tempfile(fileext = ".arrows"); write_ipc_stream(tbl, tf)        # write_ipc_stream closes its sink, so go via a file
      ipc_bytes <- readBin(tf, "raw", n = file.info(tf)$size); unlink(tf)
      u <- make_url(transport, "t2b"); L <- socket("pair", listen = u); D <- socket("pair", dial = u)
      send(L, pack_envelope('{"v":1,"type":"data_update","format":"arrow_ipc/stream"}', ipc_bytes),
           mode = "raw", block = 3000L)
      got <- recv(D, mode = "raw", block = 3000L)
      p <- parse_envelope(got)
      df2 <- read_ipc_stream(p$binary)                  # returns a data.frame by default
      ok <- identical(nrow(df2), 3L) && identical(names(df2), c("x", "g"))
      close(D); close(L); cleanup_ipc(u); ok
    }, error = function(e) { cat("    (Arrow-over-wire error:", conditionMessage(e), ")\n"); FALSE })
    check("Arrow IPC stream survives framing over the wire", res)
  } else cat("    (arrow IPC stream fns unavailable - skipped)\n")

  ## T3: per-peer PAIR channels are isolated (3 channels)
  cat("\n-- T3: per-peer PAIR channel isolation --\n")
  us <- vapply(1:3, function(i) make_url(transport, paste0("t3_", i)), character(1))
  orch  <- lapply(us, function(u) socket("pair", listen = u))
  peers <- lapply(seq_along(us), function(i) socket("pair", dial = us[i]))
  for (i in 1:3) send(orch[[i]], charToRaw(paste0("m", i)), mode = "raw", block = 3000L)
  gs <- lapply(peers, function(p) recv(p, mode = "raw", block = 3000L))
  for (i in 1:3) check(sprintf("channel %d received only its own message", i),
                       !is_err(gs[[i]]) && identical(rawToChar(gs[[i]]), paste0("m", i)))
  for (i in 1:3) { close(peers[[i]]); close(orch[[i]]); cleanup_ipc(us[i]) }

  ## T4: large messages / recv-size-max (default socket settings)
  cat("\n-- T4: large messages with DEFAULT socket settings --\n")
  u <- make_url(transport, "t4")
  L <- socket("pair", listen = u); D <- socket("pair", dial = u)
  for (mb in c(1, 8, 64, 256)) {
    n <- mb * 1024L * 1024L
    ok <- tryCatch({
      payload <- raw(n)                              # n zero bytes
      payload[1] <- as.raw(0xAB); payload[n] <- as.raw(0xCD)
      send(L, payload, mode = "raw", block = max(5000L, mb * 300L))
      got <- recv(D, mode = "raw", block = max(5000L, mb * 300L))
      !is_err(got) && length(got) == n &&
        identical(got[1], as.raw(0xAB)) && identical(got[n], as.raw(0xCD))
    }, error = function(e) { cat("    (", mb, "MB error:", conditionMessage(e), ")\n"); FALSE })
    check(sprintf("%3d MB message delivered intact (default recv max)", mb), ok)
    gc(FALSE)
    if (!ok && mb >= 64) { cat("    stopping size sweep (large messages dropped)\n"); break }
  }
  close(D); close(L); cleanup_ipc(u)

  ## T5: pipe_notify disconnect detection (single PAIR pipe)
  cat("\n-- T5: pipe_notify disconnect detection (single PAIR) --\n")
  u <- make_url(transport, "t5")
  L <- socket("pair", listen = u); v <- cv()
  pipe_notify(L, v, add = TRUE, remove = TRUE, flag = TRUE)
  D <- socket("pair", dial = u)
  check("pipe ADD detected on connect (until)", until(v, 3000L))
  close(D)
  check("pipe REMOVE detected on disconnect (until)", until(v, 3000L))
  close(L); cleanup_ipc(u)

  ## T6: message-vs-disconnect distinction on a shared cv (the runner event loop)
  cat("\n-- T6: message vs disconnect on shared cv (wait flag) --\n")
  u <- make_url(transport, "t6")
  L <- socket("pair", listen = u); D <- socket("pair", dial = u)
  v <- cv(); pipe_notify(L, v, remove = TRUE, flag = TRUE)
  ra <- recv_aio(L, mode = "raw", cv = v)            # message completion signals v (-> wait TRUE)
  send(D, charToRaw("data"), mode = "raw", block = 3000L)
  w_msg <- if (poll_cv(v, 3000L)) wait(v) else NA
  check("message arrival -> wait() TRUE", isTRUE(w_msg))
  close(D)
  w_disc <- if (poll_cv(v, 3000L)) wait(v) else NA
  check("disconnect -> wait() FALSE (flag distinguishes)", identical(w_disc, FALSE))
  close(L); cleanup_ipc(u)

  ## T7: multi-runner disconnect, per-channel PAIR topology (the design's shape)
  cat("\n-- T7: per-channel PAIR, 3 runners disconnect independently --\n")
  us <- vapply(1:3, function(i) make_url(transport, paste0("t7_", i)), character(1))
  ch  <- lapply(us, function(u) socket("pair", listen = u))
  cvs <- lapply(ch, function(s) { vv <- cv(); pipe_notify(s, vv, add = TRUE, remove = TRUE, flag = TRUE); vv })
  runners <- lapply(seq_along(us), function(i) socket("pair", dial = us[i]))
  adds <- vapply(cvs, function(vv) until(vv, 3000L), logical(1))
  check("all 3 channels connected (add detected)", all(adds))
  for (i in 1:3) {
    close(runners[[i]])                              # runner i dies
    det <- until(cvs[[i]], 3000L)                    # its channel detects the removal
    check(sprintf("channel %d detected its runner's disconnect", i), det)
    close(ch[[i]])                                   # orchestrator closes that channel only
  }
  for (u in us) cleanup_ipc(u)

  ## T8: multi-pipe single socket (#1665 shape) -- sequential removals all fire
  cat("\n-- T8: multi-pipe single socket, sequential removals (#1665 shape) --\n")
  u <- make_url(transport, "t8")
  L <- socket("pull", listen = u); v <- cv()
  pipe_notify(L, v, add = TRUE, remove = TRUE, flag = TRUE)
  ds <- lapply(1:3, function(i) socket("push", dial = u))
  Sys.sleep(0.3); cat("    cv_value after 3 connects:", cv_value(v), "\n")
  drain_cv(v)                                        # clear the add signals
  rems <- logical(3)
  for (i in 1:3) {
    close(ds[[i]])                                   # remove pipe i
    rems[i] <- until(v, 3000L)
    cat(sprintf("    removal %d detected: %s | cv_value: %d\n", i, rems[i], cv_value(v)))
  }
  check("all 3 sequential pipe removals detected on one socket", all(rems))
  close(L); cleanup_ipc(u)

  ## T9: send to a disconnected peer does not raise an R error
  cat("\n-- T9: send to disconnected peer (silent discard, no error) --\n")
  u <- make_url(transport, "t9")
  L <- socket("pair", listen = u); D <- socket("pair", dial = u)
  send(L, charToRaw("x"), mode = "raw", block = 3000L); recv(D, mode = "raw", block = 3000L)  # confirm up
  close(D); Sys.sleep(0.3)
  res <- tryCatch({ send(L, charToRaw("after-death"), mode = "raw", block = FALSE); "no-error" },
                  error = function(e) paste("error:", conditionMessage(e)))
  cat("    send-after-disconnect:", res, "\n")
  check("send to dead peer raises no R error (detect death via pipe_notify)", identical(res, "no-error"))
  close(L); cleanup_ipc(u)
}

run_suite("ipc")
run_suite("tcp")
cat(sprintf("\n=== TOTAL: %d passed, %d failed ===\n", pass, fail))
if (fail > 0) quit(status = 1) else cat("ALL TRANSPORT BEHAVIORS VALIDATED\n")
