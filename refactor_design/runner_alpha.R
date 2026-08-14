#!/usr/bin/env Rscript
# NEO JASP runner — alpha.
#
# Handshakes with the orchestrator's control endpoint (REQ/REP), receives a dedicated PAIR data
# channel in the register_ack, then receives `work` messages on that channel, reads the referenced
# dataset, computes n/mean/sd for numeric columns, and streams back a jaspResults-shaped `result`.
# Pure base R + nanonext + jsonlite — no JASP packages needed.
#
# Protocol: §18.1 framing ([u32 BE len][JSON bytes]). REQ/REP handshake on the control endpoint
# (JASP_ORCH_URL), then a dedicated PAIR v1 data channel handed back in the register_ack; all
# work/result traffic flows on the channel, not the control endpoint.
# Event loop: recv_aio + cv (blocking wait on condition variable, no spin).
#
# Usage:
#   Rscript refactor_design/runner_alpha.R [control_url]
#
# Default control endpoint: $JASP_ORCH_URL or tcp://127.0.0.1:9555

# Control endpoint (REQ/REP handshake). The orchestrator hands each runner a dedicated PAIR data
# channel in the register_ack; all work/result traffic flows there, not on the control endpoint.
ORCH_URL <- Sys.getenv("JASP_ORCH_URL", "tcp://127.0.0.1:9555")

suppressMessages({
  library(nanonext)
  library(jsonlite)
})

`%||%` <- function(x, y) if (is.null(x)) y else x

# ── helpers ──────────────────────────────────────────────────────────────────

# §18.1 framing: [uint32 big-endian json_len][JSON bytes]
pack_envelope <- function(json_text) {
  jb <- charToRaw(json_text)
  c(writeBin(length(jb), raw(4), size = 4L, endian = "big"), jb)
}

parse_envelope <- function(msg) {
  len <- readBin(msg[1:4], "integer", n = 1L, size = 4L, endian = "big")
  if (length(msg) < 4 + len) stop("frame too short")
  list(
    json   = rawToChar(msg[5:(4 + len)]),
    binary = if (length(msg) > 4 + len) msg[(5 + len):length(msg)] else raw(0)
  )
}

is_err <- function(x) {
  if (exists("is_error_value", asNamespace("nanonext"))) nanonext::is_error_value(x)
  else inherits(x, "errorValue")
}

# Read the injected dataset: Feather/Arrow via the arrow package, CSV via base R. Keeps the runner
# free of JASP packages while handling the Arrow fixture the orchestrator now injects by default.
read_dataset <- function(path) {
  if (grepl("\\.(arrow|feather)$", path, ignore.case = TRUE)) {
    if (!requireNamespace("arrow", quietly = TRUE))
      stop("dataset is Arrow but the 'arrow' package is not installed")
    as.data.frame(arrow::read_feather(path))
  } else {
    read.csv(path, stringsAsFactors = FALSE)
  }
}

# ── jaspResults builder (LIVE / web form) ─────────────────────────────────────
#
# The results web does NOT read the saved .jasp form (data / data_order / column-major
# colNames+colTypes). It renders from a top-level ".meta" array: each entry {name, type,
# title} points at a same-named TOP-LEVEL node — analysis.js createResultsViewFromMeta ->
# object.js objectConstructor -> `new JASPWidgets[type](results[name])`. A node of type
# "htmlNode" renders its `text` field (htmlNode.js). This mirrors Engine/jaspBase's
# jaspResults::dataEntry (root: name + .meta + per-child data) and jaspHtml::dataEntry
# (rawtext/text/class/maxWidth/elementType). Deliberately a throwaway hello-world: the real
# runner will build results with jaspBase, which emits this shape natively.
build_hello_result <- function(var_names, ns, means, sds) {
  lines <- sprintf("%s: n=%d mean=%.3f sd=%.3f", var_names, ns, round(means, 3), round(sds, 3))
  msg   <- paste(c("Hello World from the NEO runner!", "", lines), collapse = "\n")

  node <- list(
    title       = "Hello World",
    name        = "hello",
    rawtext     = msg,
    text        = msg,
    class       = "",
    maxWidth    = "15cm",
    elementType = "p",
    status      = "complete"
  )

  list(
    name    = "",
    ".meta" = list(
      list(name = "hello", type = "htmlNode", title = "Hello World")
    ),
    hello = node
  )
}

# ── main ─────────────────────────────────────────────────────────────────────

main <- function(control_url = ORCH_URL) {
  cat(sprintf("[runner] dialling orchestrator control endpoint at %s\n", control_url))

  # ── Handshake on the control endpoint (REQ/REP, §17.2/§19.5) ────────────────
  # REQ-dial the well-known control endpoint, send `register`, and read the `register_ack`, which
  # carries the orchestrator-assigned runner_id and a dedicated PAIR data-channel URL.
  req <- socket("req", dial = control_url)
  register_msg <- list(
    v         = 1,
    id        = sprintf("rn-reg-%s", format(Sys.time(), "%s")),
    type      = "register",
    runner_id = sprintf("runner-%s", Sys.getpid()),
    capabilities = list(
      list(kind = "analysis", name = "base-r", version = "0.1")
    ),
    priority  = 0,
    environment = list(r_version = as.character(getRversion()))
  )
  send(req, pack_envelope(toJSON(register_msg, auto_unbox = TRUE, null = "null")),
       mode = "raw", block = 3000L)
  cat("[runner] sent register\n")

  # Wait for register_ack via the async cv pattern (non-blocking and interruptible). The cv
  # signals when the ack arrives; call_aio()$data collects the raw frame. NOTE: the accessor
  # is `$data`, not `$raw` — nanonext's call_aio() exposes value/data/aio, and `$raw` is NULL
  # (which is what caused the earlier readBin "invalid connection" crash).
  cv <- cv()
  ra <- recv_aio(req, mode = "raw", cv = cv)
  if (!until(cv, 5000L)) stop("[runner] register_ack timeout")
  ack_raw <- call_aio(ra)$data
  if (is_err(ack_raw) || length(ack_raw) == 0) stop("[runner] register_ack not received (disconnect)")
  ack <- fromJSON(parse_envelope(ack_raw)$json, simplifyVector = FALSE)
  if (!isTRUE(ack$ok)) stop("[runner] registration rejected: ", ack$reason %||% "unknown")
  channel_url <- ack$channel_url
  if (is.null(channel_url) || !nzchar(channel_url)) stop("[runner] register_ack missing channel_url")
  cat(sprintf("[runner] registered as %s; dialling data channel %s\n", ack$runner_id, channel_url))
  close(req)  # the control connection is transient (one request→reply)

  # ── Data channel (PAIR v1): all work/result traffic flows here ──────────────
  sock <- socket("poly", dial = channel_url)
  on.exit(close(sock), add = TRUE)
  cat("[runner] waiting for work...\n")

  repeat {
    # Async cv recv: zero-CPU while idle, wakes when a message arrives; a 1hr idle timeout
    # leaves the cv unsignaled, handled as a disconnect below. Reuses `cv` from registration.
    ra <- recv_aio(sock, mode = "raw", cv = cv)
    if (!until(cv, 3600000L)) {
      cat("[runner] idle timeout (1hr), exiting\n")
      break
    }
    msg <- call_aio(ra)$data
    if (is_err(msg) || length(msg) == 0) {
      cat("[runner] pipe closed or recv error, exiting\n")
      break
    }

    # ── parse the work message ──
    parsed <- tryCatch(parse_envelope(msg), error = function(e) {
      cat(sprintf("[runner] bad frame: %s\n", conditionMessage(e)))
      NULL
    })
    if (is.null(parsed)) next

    work <- tryCatch(fromJSON(parsed$json, simplifyVector = FALSE), error = function(e) {
      cat(sprintf("[runner] bad JSON: %s\n", conditionMessage(e)))
      NULL
    })
    if (is.null(work)) next

    work_type <- work$type %||% ""
    if (work_type != "work") {
      cat(sprintf("[runner] ignoring message type=%s\n", work_type))
      next
    }

    cat(sprintf("[runner] <- work work_id=%s revision=%s\n",
                work$work_id, work$revision))

    # ── read the dataset (Arrow fixture by default; CSV also handled) ──
    ds_paths <- work$dataset_paths %||% list()
    data_path <- ds_paths[["ds-001"]] %||% "test_data/debug.arrow"
    cat(sprintf("[runner] reading dataset: %s\n", data_path))

    df <- tryCatch(read_dataset(data_path), error = function(e) {
      cat(sprintf("[runner] dataset read error: %s\n", conditionMessage(e)))
      NULL
    })
    if (is.null(df)) next

    # ── compute descriptives ──
    num_cols <- names(df)[sapply(df, is.numeric)]
    if (length(num_cols) == 0) {
      cat("[runner] no numeric columns found\n")
      next
    }

    ns    <- sapply(df[num_cols], function(x) sum(!is.na(x)))
    means <- sapply(df[num_cols], function(x) mean(x, na.rm = TRUE))
    sds   <- sapply(df[num_cols], function(x) sd(x, na.rm = TRUE))

    cat(sprintf("[runner] computed descriptives for %d columns\n", length(num_cols)))
    for (i in seq_along(num_cols)) {
      cat(sprintf("  %s: n=%d mean=%.3f sd=%.3f\n",
                  num_cols[i], ns[i], means[i], sds[i]))
    }

    # ── build jaspResults and send ──
    results <- build_hello_result(num_cols, ns, means, sds)

    result_msg <- list(
      v          = 1,
      id         = sprintf("rn-%s", format(Sys.time(), "%s")),
      reply_to   = work$id,
      session_id = work$session_id,  # echo: lets the orchestrator correlate (session_id, work_id)
      type       = "result",
      work_id    = work$work_id,
      revision   = work$revision,
      status     = "complete",
      # Adjacently-tagged kind+payload (§19.2 Result payloads by kind); the orchestrator
      # fills results_dir when it forwards.
      kind       = "analysis",
      payload    = list(results = results)
    )

    reply_json <- toJSON(result_msg, auto_unbox = TRUE, null = "null", na = "null")
    reply_raw  <- pack_envelope(reply_json)

    send(sock, reply_raw, mode = "raw", block = 3000L)
    cat(sprintf("[runner] -> result work_id=%s (%d bytes)\n",
                work$work_id, length(reply_raw)))
  }
}

# Resolve URL: cli arg > env var > default
args <- commandArgs(trailingOnly = TRUE)
url <- if (length(args) >= 1 && !is.na(args[1])) args[1] else ORCH_URL
main(url)
