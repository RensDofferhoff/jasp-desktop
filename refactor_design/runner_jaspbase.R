#!/usr/bin/env Rscript
# NEO JASP runner — jaspBase-backed (real analyses).
#
# Replaces the hello-world compute core of runner_alpha.R with the real engine path:
# per `work` it calls jaspBase::runJaspResults() on the module's *Internal analysis
# function, with a bridge (defined in globalenv) standing in for the C++ engine's data /
# temp / value natives. jaspBase serializes the results natively (getResults()); the
# runner hand-builds nothing. See refactor_design/jaspbase-plugin.md for the contract.
#
# This replicates the ENGINE path only — NOT runWrappedAnalysis (that's the syntax/RStudio
# path and needs jaspSyntax), and NOT jaspTools (testing only, never used here).
#
# Protocol: §18.1 framing ([u32 BE len][JSON bytes]). REQ/REP handshake on the control endpoint
# (JASP_ORCH_URL), then a dedicated PAIR v1 data channel handed back in the register_ack; all
# work/result traffic flows on the channel, not the control endpoint.
# Liveness (§19.4/§25.4): each pass through the loop sends an `activity` message — reaching the
# loop IS the activity (a work cycle completed, runner ready), so the signal is truthful rather
# than a synthetic timer. This keeps the orchestrator's hang detector from mistaking a live runner
# for a wedged one.
# Event loop: recv_aio + cv + call_aio(ra)$data (blocking wait, zero CPU idle).
#
# Usage:
#   Rscript refactor_design/runner_jaspbase.R [control_url]
# Env:
#   JASP_RUNNER_LIBDIR  compiled package library (default: the jaspModuleTools workdir)
#   JASP_RUNNER_MODULE  module to load + advertise (default: jaspTTests)
#   JASP_RUNNER_VERBOSE if set, dump the processed options JSON per work (default: off)

# Control endpoint (REQ/REP handshake). The orchestrator hands each runner a dedicated PAIR data
# channel in the register_ack; all work/result traffic flows there, not on the control endpoint.
ORCH_URL    <- Sys.getenv("JASP_ORCH_URL", "tcp://127.0.0.1:9555")
# The runner is bound to ONE libpath (LIBDIR) for its lifetime. It advertises every JASP module
# found there (scan_modules) and lazy-loads each on first work. MODULE/MODULE_VER are now only a
# fallback (a work payload with no module name, or a lib that fails to scan); they no longer drive
# advertisement.
LIBDIR      <- Sys.getenv("JASP_RUNNER_LIBDIR",
                          "/home/sp42/jaspModuleTools/workdir/jaspTTests")
MODULE      <- Sys.getenv("JASP_RUNNER_MODULE", "jaspTTests")
MODULE_VER  <- Sys.getenv("JASP_RUNNER_MODULE_VER", "0.95.5")

# Verbose logging gate (mirrors JASP_CLIENT_LOG): the processed-options dump is noisy, so it's
# off unless JASP_RUNNER_VERBOSE is set.
VERBOSE <- nzchar(Sys.getenv("JASP_RUNNER_VERBOSE", ""))

suppressMessages({
  library(nanonext)
  library(jsonlite)
})

# The unified Arrow/Feather read path (§8.3) — provides read_jasp_data(). Root-relative: the runner
# is launched from the project root (launch_alpha.sh cd's there), as is the data.R smoke test.
source("jaspRunner/R/data.R")

`%||%` <- function(x, y) if (is.null(x)) y else x

# ── step timing ───────────────────────────────────────────────────────────────
# Wall-clock step timing so we can see where each work's time goes. proc.time()'s
# `elapsed` is monotonic (immune to wall-clock jumps), fine-grained enough here.
now_s <- function() proc.time()[["elapsed"]]
log_step <- function(label, t0, note = "") {
  dt <- now_s() - t0
  cat(sprintf("[runner]   %-28s %9.3f s%s\n", label, dt,
              if (nzchar(note)) paste0("   ", note) else ""))
  invisible(dt)
}

# ── diagnostics: state round-trip (plot-state bloat investigation) ───────────
# Provenance line so every log self-describes its exact library builds — A/B
# runs are only comparable if these match except for the one package under test.
log_provenance <- function() {
  pkg_info <- function(pkg) {
    v   <- tryCatch(as.character(packageVersion(pkg)), error = function(e) return("?"))
    lib <- tryCatch(dirname(system.file(package = pkg)), error = function(e) return("?"))
    sha <- tryCatch({
      d <- read.dcf(system.file("DESCRIPTION", package = pkg))
      if ("RemoteSha" %in% colnames(d)) substr(d[1, "RemoteSha"], 1, 7) else ""
    }, error = function(e) "")
    sprintf("%s %s%s [%s]", pkg, v, if (nzchar(sha)) paste0("/", sha) else "", lib)
  }
  cat(sprintf("[runner] provenance: R %s | %s | %s | %s | %s\n",
              as.character(getRversion()), pkg_info("jaspBase"),
              pkg_info("ggplot2"), pkg_info("jaspGraphs"), pkg_info("qs2")))
}

# Split the runJaspResults blind block into: state load / state save / finish
# tail (collect objects + complete(): json + rds + seal + send). Wraps the
# jaspBase internals in place; pure diagnostics, behavior unchanged.
# SUPERSEDED: this logging now lives in jaspBase itself (JASP_RESULTS_TIMING,
# see jaspBase/R/common.R). REMOVE this shim once the libpath jaspBase is
# rebuilt, and set JASP_RESULTS_TIMING=1 instead (do NOT run both: double logs).
install_state_timers <- function() {
  ns <- asNamespace("jaspBase")
  state_file_mb <- function() {
    loc <- tryCatch(jaspBase:::.fromRCPP(".requestStateFileNameNative"), error = function(e) NULL)
    if (is.null(loc)) return("")
    sz <- tryCatch(file.info(loc$relativePath)$size, error = function(e) NA)
    if (is.na(sz)) "" else sprintf("%.1f MB file", sz / 1e6)
  }
  wrap <- function(name, label, with_size = FALSE) {
    orig <- get(name, envir = ns)
    wrapper <- function(...) {
      t0 <- now_s()
      out <- orig(...)
      log_step(label, t0, if (with_size) state_file_mb() else "")
      out
    }
    tryCatch(assignInNamespace(name, wrapper, ns = "jaspBase"),
             error = function(e) cat(sprintf("[runner] WARN: could not instrument %s: %s\n",
                                             name, conditionMessage(e))))
  }
  wrap(".retrieveState",    "state load",  with_size = TRUE)
  wrap(".saveState",        "state save",  with_size = TRUE)
  wrap("finishJaspResults", "finish tail")
}

# ── decision: do NOT persist live plot objects in the state ──────────────────
# The behavior lives in jaspBase (finishJaspResults, jaspBase/R/common.R): when
# JASP_STATE_NO_FIGURES is set, the `figures` part of jaspState.RData (the LIVE
# ggplot objects — under ggplot2 4.x/S7 ~100-300 MB serialized each) is dropped.
# They are a cache, not data: PNGs, plotly JSON files, and the results JSON
# (option-dependencies, editOptions, resizedByUser dims, png paths) persist
# without them; a re-run rebuilds exactly the invalidated plots and reuses the
# rest from the results JSON. The runner only sets the gate (and logs it).
# JASP_STATE_KEEP_FIGURES=1 restores the old behavior for A/B runs.
setup_figures_drop <- function() {
  if (nzchar(Sys.getenv("JASP_STATE_KEEP_FIGURES", ""))) {
    Sys.setenv(JASP_STATE_NO_FIGURES = "")
    cat("[runner] figures drop: DISABLED (JASP_STATE_KEEP_FIGURES set)\n")
  } else {
    Sys.setenv(JASP_STATE_NO_FIGURES = "1")
    cat("[runner] figures drop: live plot objects will NOT be persisted in jaspState.RData\n")
  }
}

# ── decision: do NOT forge the write seal ────────────────────────────────────
# The seal (jaspResultsFinishedWriting.txt) guards a SHARED, in-place state file
# against mid-write crashes; NEO's immutable per-revision dirs have no such
# hazard, and a missing seal made jaspBase SILENTLY skip state+results reuse
# (the HANDOVER-next4 failure mode). jaspBase's constructor now bypasses the
# gate when JASP_RESULTS_NO_SEAL is set. No-op until the libpath jaspBase is
# rebuilt with that change; the copy-on-seal below stays harmless either way.
setup_seal_bypass <- function() {
  Sys.setenv(JASP_RESULTS_NO_SEAL = "1")
  cat("[runner] seal bypass: state/results trusted without the write-seal file\n")
}

# ── framing / nng helpers (§18.1) ─────────────────────────────────────────────

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

# Is an aio's recv still pending? (nanonext's `$data` is an *unresolvedValue*
# placeholder — not NULL — until completion; probe-verified.)
still_pending <- function(ra) {
  tryCatch(isTRUE(unresolved(ra$data)), error = function(e) TRUE)
}

# Liveness ping (§19.4/§25.4). Sent once per loop pass — reaching the loop is real activity (a work
# cycle finished / runner ready), so no work_id is attached and there is nothing to rate-limit: the
# cadence is bounded by the work rate itself. The orchestrator bumps the runner's last_activity on
# any recv, so this resets its hang-detector clock.
send_activity <- function(sock) {
  act <- list(v = 1, id = sprintf("rn-act-%s", format(Sys.time(), "%s")), type = "activity")
  send(sock, pack_envelope(toJSON(act, auto_unbox = TRUE, null = "null")),
       mode = "raw", block = 100L)
}

# ── the abort plane (orchestrator-v2 §7): two planes, one flag ─────────────
#
# `abort {work_id}` is the ONE unsolicited message a busy runner may see (works pull,
# aborts push). Two planes, one flag:
#
#   * Wrapper plane (this section): while run_analysis blocks the R thread, the PAIR
#     socket is free (the loop's recv consumed the work), so a mid-run abort parks in
#     a pending recv_aio. Arming/disarming never touches R state mid-run.
#   * R plane (checkpoint-gated): jaspBase's check code calls `jasp_checkpoint()` at
#     its cooperative unwind points (that integration lands in the jaspBase bridge —
#     this script defines the hook). A checkpoint DRAINS the pending recv
#     non-blocking; an abort for the running work raises the `jaspAbort` condition,
#     which unwinds run_analysis (nothing in jaspBase catches it — it is not an
#     `error`), and the loop sends `result(status="aborted")`.
#
# No protocol can beat checkpoint density (§7): an analysis grinding in a C call sees
# nothing, hears nothing, aborts never — the router has no "aborting soon" state and
# simply keeps seeing RUNNING until the terminal (or its hang detector recycles us).
# A raced completion (the run finished before the abort was processed) pops the abort
# at loop-top: a documented NO-OP.

# The abort state: `.ab$current` = the running (work_id, revision); `.ab$aio` = the
# pending mid-run recv; `.ab$hit` = an abort for the current work was seen.
.ab <- new.env(parent = emptyenv())

arm_abort_plane <- function(sock, work) {
  .ab$current <- list(work_id = work$work_id, revision = work$revision,
                      session_id = work$session_id, id = work$id)
  .ab$hit <- FALSE
  # One pending recv for the whole run — the socket is otherwise idle (pull
  # discipline: nothing but an abort may arrive). Wrapped: a pipe error mid-run
  # surfaces at the next loop recv either way.
  .ab$aio <- tryCatch(recv_aio(sock, mode = "raw"), error = function(e) NULL)
  invisible(TRUE)
}

disarm_abort_plane <- function(sock) {
  ra <- .ab$aio
  .ab$aio <- NULL
  if (is.null(ra)) return(invisible(FALSE))
  if (still_pending(ra)) {
    # Nothing arrived mid-run: cancel the pending recv (a cancelled aio's data is
    # an ERROR value — never parse it; pipe trouble surfaces at the loop-top recv).
    tryCatch(stop_aio(ra), error = function(e) NULL)
    return(invisible(FALSE))
  }
  if (is_err(ra$data)) return(invisible(FALSE))  # pipe error: the loop handles it
  note_abort_frame(ra$data)
  invisible(.ab$hit)
}

# Parse a drained frame; flag (and log) an abort for the running work. Anything else
# arriving mid-run would violate pull discipline — logged loudly, never silent.
note_abort_frame <- function(raw) {
  if (is_err(raw) || length(raw) == 0) return(invisible(FALSE))
  parsed <- tryCatch(parse_envelope(raw), error = function(e) NULL)
  msg <- if (is.null(parsed)) NULL else
    tryCatch(fromJSON(parsed$json, simplifyVector = FALSE), error = function(e) NULL)
  if (is.null(msg)) {
    cat("[runner] WARN: undecodable frame drained mid-run — dropped\n")
    return(invisible(FALSE))
  }
  if ((msg$type %||% "") == "abort") {
    if (!is.null(msg$work_id) && !is.null(.ab$current) &&
        msg$work_id == .ab$current$work_id) {
      .ab$hit <- TRUE
      cat(sprintf("[runner] abort received for work_id=%s (drained)\n", msg$work_id))
    } else {
      cat(sprintf("[runner] abort for work_id=%s ignored (raced/stale; running %s)\n",
                  msg$work_id %||% "?", .ab$current$work_id %||% "?"))
    }
    return(invisible(TRUE))
  }
  cat(sprintf("[runner] WARN: non-abort type=%s arrived mid-run (pull violation?) — dropped\n",
              msg$type %||% "?"))
  invisible(FALSE)
}

# The checkpoint hook (R plane). Returns TRUE if an abort for the running work has
# arrived; also raises `jaspAbort` so the caller unwinds cooperatively. jaspBase's
# check code calls this at its unwind points — until that lands, the only callers
# are the run boundaries (arm/disarm), which is correct but coarse.
jasp_checkpoint <- function() {
  ra <- .ab$aio
  if (!is.null(ra) && !still_pending(ra) && !is_err(ra$data)) {
    note_abort_frame(ra$data)
    .ab$aio <- NULL  # the frame is consumed — disarm must not re-note it
  }
  if (isTRUE(.ab$hit)) {
    stop(structure(list(message = sprintf("abort requested for work_id=%s",
                                          .ab$current$work_id %||% "?")),
                   class = c("jaspAbort", "condition", "error")), call. = FALSE)
  }
  invisible(FALSE)
}

# The aborted terminal payload (minimal — the router discards it by revision or
# drops it on work_close; content is diagnostics only).
aborted_result <- function(work) {
  list(results = list(title = "aborted",
                      errorMessage = "the analysis was aborted at a checkpoint"),
       status = "aborted")
}

# ── the bridge: engine stand-in natives (globalenv) ───────────────────────────
#
# jaspBase's .fromRCPP (R/common.R:622) resolves these names via exists()/getAnywhere(),
# which reaches globalenv (verified empirically — jaspbase-plugin.md §2). The namespace is
# LOCKED, so we must NOT assign into it; globalenv is correct.
#
# Per-work mutable state lives in `.state`; the per-work value natives (.ppi etc.) are plain
# globals reassigned before each run (the work unit is invariant — it carries its own ppi).

.state <- new.env(parent = emptyenv())
.state$dataset     <- NULL          # current work's preloaded aliased frame (preloadData=true)
.state$outputDir   <- tempdir()     # current work's scratchpad (== work$output_dir)
.state$tempCounter <- 0L
# Pruning/encoding state (HANDOVER-runner-data-pruning.md §3.2), reset at every work start.
# D11: the cache's FIELD names are the storage tokens — schemaNames holds TOKENS (the raw
# footer, zero extra reads); display names resolve through them statelessly (the token
# is hex-of-display, so membership either way is one token_of away) and are extracted
# in full ONLY on the O(k) paths that already pay O(k) (rewrite_syntax, all.columns).
.state$dataPath    <- NULL                     # absolute Feather path (orchestrator-injected)
.state$schema      <- NULL                     # arrow Schema object (lazy type source)
.state$schemaNames <- character(0)             # eager (one vectorized call) — the FIELD names (tokens)
.state$schemaIdx   <- new.env(parent = emptyenv())  # token -> 0-based field index (O(1) type lookup)
.state$typeCache   <- new.env(parent = emptyenv())  # name (either form) -> type, on first use
.state$aliasCache  <- new.env(parent = emptyenv())  # name (either form) -> schema-type alias
.state$displayNames <- NULL                    # lazy full display vector (schema_displays, on demand)
.state$cols        <- new.env(parent = emptyenv())  # per-work source-vector cache, display name -> vector

# Schema-only read — FOOTER ONLY, zero data materialized.
# NB: arrow::read_feather(path, as_data_frame = FALSE)$schema is a trap: it materializes the
# ENTIRE file in C++ memory first just to expose $schema (measured 2026-08-15: a 94 MB
# feather took RSS from 110 MB to 1.74 GB; on terror_tall that is ~2 GB of pure waste per
# work — the "2.2 GB base" mystery). RecordBatchFileReader opens the IPC/Feather container
# and exposes the schema without touching batches (+9 MB measured).
read_feather_schema <- function(path) {
  rf <- arrow::ReadableFile$create(path)
  on.exit(try(rf$close(), silent = TRUE))
  arrow::RecordBatchFileReader$create(rf)$schema
}

# ── lazy schema access (wide-file fix, 2026-08-15; D11 dual vocabulary) ──────
# Types are resolved O(used), not O(all): work start extracts only the NAMES (one
# vectorized call) plus a token->index map (list2env, C speed). A type is extracted on
# first use via Schema$field(i) — O(1) direct index — and cached. GetFieldByName is
# NEVER used: it linear-scans the field list per call (O(k) each, O(k^2) over all
# columns; measured 5.7 s at 10k columns). all.columns/header paths legitimately touch
# all types but pay O(k) once, not O(k^2).
#
# D11: every accessor takes a name in EITHER vocabulary — the storage token (the
# field's own name) or the display name — because token_of is injective: a display's
# token is in the idx iff the display is a column, and a token is in the idx directly.
# The DISPLAY form is the runner's working vocabulary (options, pairs, results decode);
# tokens are the storage identity.
schema_display_name <- function(nm) {
  if (!is.character(nm) || length(nm) != 1L || !nzchar(nm)) return(NULL)
  if (!is.null(.state$schemaIdx[[token_of(nm)]])) return(nm)   # nm is a DISPLAY
  if (!is.null(.state$schemaIdx[[nm]])) {                     # nm is a field name
    d <- token_decode(nm)
    if (!is.null(d)) return(d)                               # a token -> its display
    return(nm)                                                # old-vocabulary field name
  }
  NULL
}

schema_type_of <- function(nm) {
  if (!is.character(nm) || length(nm) != 1L || !nzchar(nm)) return(NULL)
  ty <- .state$typeCache[[nm]]
  if (!is.null(ty)) return(ty)
  # Display-priority (same order as schema_display_name — the torture case must
  # resolve identically everywhere): a display's token first, then the field itself.
  i <- .state$schemaIdx[[token_of(nm)]]
  if (is.null(i)) i <- .state$schemaIdx[[nm]]
  if (is.null(i)) return(NULL)
  typ <- .state$schema$field(i)$type          # 0-based index (verified)
  ty <- if (inherits(typ, "DictionaryType")) {
          if (isTRUE(typ$ordered)) "ordinal" else "nominal"
        } else "scale"
  assign(nm, ty, envir = .state$typeCache)
  ty
}

# The full display-name vector — extracted ON DEMAND and cached (the O(k) paths only:
# rewrite_syntax scans it, all.columns aliases it; work starts stay O(1) in schema reads).
schema_displays <- function() {
  d <- .state$displayNames
  if (!is.null(d)) return(d)
  toks <- .state$schemaNames
  d <- vapply(seq_along(toks), function(i) {
    m <- .state$schema$field(i - 1L)$metadata[["jasp:display_name"]]
    if (is.null(m) || !nzchar(m)) toks[i] else m
  }, character(1L), USE.NAMES = FALSE)
  .state$displayNames <- d
  d
}

# All types, DISPLAY-named (the all.columns/header paths — O(k) by nature). Seeds the
# type cache under the display names (the working vocabulary).
schema_all_types <- function() {
  nms <- schema_displays()
  tys <- vapply(seq_along(nms), function(i) {
    typ <- .state$schema$field(i - 1L)$type
    if (inherits(typ, "DictionaryType")) {
      if (isTRUE(typ$ordered)) "ordinal" else "nominal"
    } else "scale"
  }, character(1L), USE.NAMES = FALSE)
  names(tys) <- nms
  for (i in seq_along(nms)) if (is.null(.state$typeCache[[nms[i]]]))
    assign(nms[i], tys[i], envir = .state$typeCache)
  tys
}

# Lazy schema-type alias for a name in EITHER vocabulary (alias_map of old, but only
# for names ever asked). The alias is the display's token + "_" + type — identical
# string whichever form was asked.
schema_alias_of <- function(nm) {
  if (!is.character(nm) || length(nm) != 1L || !nzchar(nm)) return(NULL)
  a <- .state$aliasCache[[nm]]
  if (!is.null(a)) return(a)
  d <- schema_display_name(nm)
  if (is.null(d)) return(NULL)
  ty <- schema_type_of(d)
  if (is.null(ty)) return(NULL)
  a <- alias_encode(d, ty)
  assign(nm, a, envir = .state$aliasCache)
  a
}

# ── stateless column-name aliases (HANDOVER-runner-data-pruning.md §2.2) ─────
#
# Everything inside R sees aliases; raw UTF-8 names live only outside (lane, wire,
# frontend, logs). Pure stateless functions — encode/decode ARE the scheme: no map,
# no counter, no per-dataset state, decodable in isolation from any artifact.
# decode NEVER throws (legacy's strict-throw killed real analyses, jasp-issues #3495).
#
#   alias = "jasp_enc_hex_" + lowercase-hex(utf8(name)) + "_" + type
#
# The prefix is distinctive, greppable, and SELF-DESCRIBING: the codec segment ("hex") says
# how the payload decodes — a future codec swap mints "jasp_enc_b32_" aliases that this
# decoder simply treats as non-aliases (starts_with fails -> pass-through, never throws).
# Hex keeps aliases ASCII, identifier-safe by
# construction, and decodable with base R in any future runtime. The type suffix is
# ALWAYS present — single- and dual-role columns have identical shape, nothing
# special-cases.

ALIAS_PREFIX <- "jasp_enc_hex_"
ALIAS_TYPES  <- c("scale", "ordinal", "nominal")

# Scalar. name: single UTF-8 string; type: one of ALIAS_TYPES.
alias_encode <- function(name, type) {
  stopifnot(is.character(name), length(name) == 1L, type %in% ALIAS_TYPES)
  bytes <- charToRaw(enc2utf8(name))
  paste0(ALIAS_PREFIX, paste0(as.character(bytes), collapse = ""), "_", type)
}

# ── storage tokens (D11 — the base cache's field-name vocabulary) ────────────
#
# The cache's FIELD names are the encoded canonical identity jasp_enc_hex_<hex(display)>
# (no type suffix — types stay schema metadata). The token is the front segment of the
# alias: alias = token + "_" + cast type, so every alias is its column's token plus the
# type, and the worker's blob fields (token_type) ARE the aliases — the slice-B rename
# is an identity. display_name (wire ColumnInfo + jasp:display_name metadata) is the
# decode; these helpers are the stateless codec mirror of the ingest encoder.

token_of <- function(display) {
  bytes <- charToRaw(enc2utf8(display))
  paste0(ALIAS_PREFIX, paste(sprintf("%02x", as.integer(bytes)), collapse = ""))
}

# Scalar -> the display name, or NULL (not token-shaped / malformed). Never throws.
token_decode <- function(token) {
  if (!is.character(token) || length(token) != 1L || is.na(token)) return(NULL)
  if (!startsWith(token, ALIAS_PREFIX)) return(NULL)
  hex <- substring(token, nchar(ALIAS_PREFIX) + 1L)
  if (!grepl("^([0-9a-f]{2})+$", hex)) return(NULL)
  bytes <- as.raw(strtoi(substring(hex, seq.int(1L, nchar(hex), 2L),
                                        seq.int(2L, nchar(hex), 2L)), 16L))
  name <- rawToChar(bytes)
  Encoding(name) <- "UTF-8"
  if (!validUTF8(name)) NULL else name
}

# Scalar -> list(name, type) or NULL. Never throws; malformed -> NULL.
alias_decode <- function(alias) {
  if (!is.character(alias) || length(alias) != 1L || is.na(alias)) return(NULL)
  if (!startsWith(alias, ALIAS_PREFIX)) return(NULL)
  parts <- strsplit(substring(alias, nchar(ALIAS_PREFIX) + 1L), "_", fixed = TRUE)[[1L]]
  if (length(parts) != 2L || !(parts[2L] %in% ALIAS_TYPES)) return(NULL)
  hex <- parts[1L]
  if (!grepl("^([0-9a-f]{2})+$", hex)) return(NULL)
  bytes <- as.raw(strtoi(substring(hex, seq.int(1L, nchar(hex), 2L),
                                        seq.int(2L, nchar(hex), 2L)), 16L))
  name <- rawToChar(bytes)
  Encoding(name) <- "UTF-8"
  if (!validUTF8(name)) return(NULL)
  list(name = name, type = parts[2L])
}

# Vectorised strict helpers. Non-aliases / unknowns pass through UNCHANGED (a module may
# legitimately pass raw names; the natives serve those too).
alias_decode_names <- function(x) {
  if (is.null(x)) return(NULL)
  vapply(unname(as.character(x)), function(a) {
    d <- alias_decode(a); if (is.null(d)) a else d$name
  }, character(1L), USE.NAMES = FALSE)
}

# Strict display decode (§2.2): schema-gated, never throws. `schema` may be a named
# type vector (fixtures), a plain names vector, or a MEMBERSHIP FUNCTION (the runner's
# D11 gate — display membership via the token index, no eager display extraction).
alias_decode_strict <- function(x, schema) {
  gate <- if (is.function(schema)) schema
          else { schema_names <- if (!is.null(names(schema))) names(schema) else schema
                function(nm) nm %in% schema_names }
  if (!is.character(x) || length(x) != 1L || is.na(x)) return(x)
  d <- alias_decode(x)
  if (is.null(d) || !isTRUE(gate(d$name))) return(x)
  d$name
}

# Lax display decode (§2.2): single pass over free text; at each "jasp_enc_hex_" with an
# identifier boundary before it (no [A-Za-z0-9._] immediately preceding — the prefix
# contains "_", so a naive substring scan would match inside longer identifiers):
# greedy-match identifier chars, trim one char at a time until (decode succeeds AND name
# is a schema column); substitute; NEVER rescan substituted output (no chaining).
# `schema` may be a names vector (fixtures) or a MEMBERSHIP FUNCTION (the runner's gate).
alias_decode_lax <- function(text, schema) {
  gate <- if (is.function(schema)) schema
          else { schema_names <- if (!is.null(names(schema))) names(schema) else schema
                function(nm) nm %in% schema_names }
  if (!is.character(text) || length(text) != 1L || is.na(text)) return(text)
  if (!grepl(ALIAS_PREFIX, text, fixed = TRUE)) return(text)
  is_name_char <- function(ch) grepl("[A-Za-z0-9._]", ch)
  n <- nchar(text); plen <- nchar(ALIAS_PREFIX)
  chunks <- character(0); i <- 1L
  while (i <= n) {
    if (i + plen - 1L <= n && substr(text, i, i + plen - 1L) == ALIAS_PREFIX &&
        (i == 1L || !is_name_char(substr(text, i - 1L, i - 1L)))) {
      j <- i + plen
      while (j <= n && is_name_char(substr(text, j, j))) j <- j + 1L
      end <- j - 1L
      replaced <- FALSE
      while (end >= i + plen) {
        d <- alias_decode(substr(text, i, end))
        if (!is.null(d) && isTRUE(gate(d$name))) {
          chunks <- c(chunks, d$name)
          i <- end + 1L
          replaced <- TRUE
          break
        }
        end <- end - 1L
      }
      if (replaced) next
    }
    chunks <- c(chunks, substr(text, i, i))
    i <- i + 1L
  }
  paste0(chunks, collapse = "")
}

# Successor of the engine's decodeJsonSafeHtml: lax-decode every string (and object KEY,
# legacy replaceAll renamed keys too) in a parsed JSON tree. .state$schemaGate is the
# membership predicate (D11: display membership via the token index — no eager display
# extraction on the per-work results path).
# NOTE: JSON nulls arrive as R NULLs; `node[[i]] <- NULL` would DELETE the element (shrinking
# the list mid-iteration -> subscript out of bounds), so NULL members are skipped untouched
# (jaspBase tables always carry them: footnotes' cols/rows).
lax_decode_tree <- function(node) {
  if (is.character(node))
    return(vapply(node, function(s) alias_decode_lax(s, .state$schemaGate),
                  character(1L), USE.NAMES = FALSE))
  if (is.list(node)) {
    if (!is.null(names(node)))
      names(node) <- vapply(names(node),
                            function(s) alias_decode_lax(s, .state$schemaGate),
                            character(1L), USE.NAMES = FALSE)
    for (i in seq_along(node))
      if (!is.null(node[[i]])) node[[i]] <- lax_decode_tree(node[[i]])
  }
  node
}

# ── rewrite_syntax: raw schema names -> aliases inside R-code text (§3.2 step 6) ──
#
# Engine modeled 1:1 on legacy ColumnEncoder::encodeRScript (columnencoder.cpp:440-507):
#   * name chars [A-Za-z0-9._]; a match needs a non-name char (or text edge) BEFORE and
#     AFTER — so a column "E" never shreds the identifier TRUE;
#   * occurrences inside string literals ('…' / "…") are skipped (escapes not considered,
#     same as legacy);
#   * a name followed by optional whitespace + "(" is a FUNCTION call -> left alone
#     (legacy's guard for columns named like `rep` or `if`);
#   * longest names first, so partial names don't shred longer ones;
#   * substituted aliases cannot rematch: every alias char is a name char, so any raw name
#     landing inside an inserted alias fails the boundary test.

.substitute_free_occurrences <- function(text, nm, alias_of) {
  n <- nchar(text); m <- nchar(nm)
  if (m == 0L || n < m) return(text)
  is_name_char <- function(ch) grepl("[A-Za-z0-9._]", ch)
  starts <- gregexpr(nm, text, fixed = TRUE)[[1L]]
  if (starts[1L] == -1L) return(text)
  # string-literal mask (legacy ignores escape chars too)
  chars <- strsplit(text, "")[[1L]]
  in_str <- logical(n); inside <- FALSE; delim <- ""
  for (k in seq_len(n)) {
    ch <- chars[k]
    if (!inside && (ch == "\"" || ch == "'")) { inside <- TRUE; delim <- ch; in_str[k] <- TRUE }
    else if (inside) { in_str[k] <- TRUE; if (ch == delim) inside <- FALSE }
  }
  hits <- integer(0)
  for (s in as.integer(starts)) {
    e <- s + m - 1L
    if (in_str[s]) next
    start_free <- s == 1L || !is_name_char(substr(text, s - 1L, s - 1L))
    end_free <- TRUE
    if (e < n) {
      nxt <- substr(text, e + 1L, e + 1L)
      if (is_name_char(nxt)) end_free <- FALSE
      else {
        k <- e + 1L   # function-call guard: whitespace* + "(" after the name
        while (k <= n && substr(text, k, k) %in% c(" ", "\t", "\n")) k <- k + 1L
        if (k <= n && substr(text, k, k) == "(") end_free <- FALSE
      }
    }
    if (start_free && end_free) hits <- c(hits, s)
  }
  if (!length(hits)) return(text)
  al <- alias_of(nm)   # lazy: only names with a FREE occurrence pay for aliasing
  if (is.null(al)) return(text)
  out <- character(0); pos <- 1L
  for (s in hits) {
    e <- s + m - 1L
    out <- c(out, substr(text, pos, s - 1L), al)
    pos <- e + 1L
  }
  out <- c(out, substr(text, pos, n))
  paste0(out, collapse = "")
}

# Dual mode: rewrite_syntax(text, alias_map named vector) [fixtures] or
# rewrite_syntax(text, schema_names, alias_of resolver) [runner]. Names are scanned
# longest-first; alias_of is called ONLY for names that actually occur free in the text,
# so wide schemas cost a C-speed name scan, not k alias constructions.
rewrite_syntax <- function(text, schema_names, alias_of = NULL) {
  if (!is.character(text) || length(text) != 1L || is.na(text) || !nzchar(text)) return(text)
  if (is.null(alias_of)) {
    am <- schema_names
    if (length(am) == 0L) return(text)
    schema_names <- names(am)
    alias_of <- function(nm) {
      i <- match(nm, names(am))
      if (is.na(i)) NULL else unname(am[i])
    }
  }
  if (length(schema_names) == 0L) return(text)
  for (nm in schema_names[order(nchar(schema_names), decreasing = TRUE)])
    text <- .substitute_free_occurrences(text, nm, alias_of)
  text
}

# ── the options walk (§3.2 steps 1-2) ─────────────────────────────────────────
#
# One recursion over the catalog, mirrored 1:1 from legacy columnencoder.cpp
# (_addTypeToColumnNamesInOptionsRecursively :784-826, _convertPreloadingDataOption
# :673-782, meta-driven pass :843-897). Input: the wire options (jsonlite list,
# simplifyVector=FALSE) incl. `.meta`; the schema name->type map. Output:
#   options  — rewritten: variable slots -> alias of the pair THAT SLOT asked for;
#              `<key>.types` siblings ALWAYS emitted (readDataSetByVariableTypes
#              hard-errors without them, common.R:320-329); model text rewritten;
#              `.meta` dropped (modules never saw it).
#   pairs    — data.frame(name, type), unique, first-appearance order.

walk_and_rewrite_options <- function(options, schema_types, preloading = TRUE) {
  # schema_types: EAGER named vector display->type (fixtures) OR LAZY accessor
  # list(idx, type_of, resolve, displays) (runner). The walk only needs membership,
  # per-name types, and schema-type aliases — the lazy shape keeps wide-file work
  # starts O(used), not O(all).
  #
  # `preloading` mirrors the classic engine contract (columnencoder.cpp
  # encodeColumnNamesinOptions(options, preloadingData)): with preloading, slot
  # values are aliased PER THE SLOT'S types entry (the preload frame carries every
  # collected pair, so dual-role columns exist under both casts). WITHOUT
  # preloading (on-demand), the classic meta pass encoded everything by the
  # SCHEMA type — ONE alias per column — because formula-building modules (ANOVA)
  # reference a column through one slot (modelTerms) but read it through another
  # (fixedFactors): divergent per-slot aliases would break the formula↔dataset
  # name match. The runner passes its payload's preloadData here.
  #
  # D11 dual vocabulary: an option value may name a column by its DISPLAY name
  # (classic-shaped options — the migration window) or by its STORAGE TOKEN (the
  # post-flip frontend binds columnName = the wire name). `resolve` maps either form
  # to the DISPLAY (the working vocabulary); the alias is the display's token + "_" +
  # type — the identical string whichever form arrived, so token-shaped options pass
  # through UNTOUCHED (token-native) and classic-shaped options encode exactly as
  # before. Display priority: a value that is a known display is ALWAYS the display
  # (the torture case — a display that literally spells another column's token).
  is_obj <- function(x) is.list(x) && !is.null(names(x))
  if (is.character(schema_types)) {
    nms <- names(schema_types)
    idx <- list2env(as.list(setNames(seq_along(nms), nms)), parent = emptyenv())
    raw_type_of <- function(nm) {
      t <- schema_types[nm]              # [ ] not [[ ]]: missing name -> NA, not an error
      if (length(t) == 1L && !is.na(t)) unname(t) else NULL
    }
    resolve <- function(nm) {
      if (!is.character(nm) || length(nm) != 1L || !nzchar(nm)) return(NULL)
      if (!is.null(idx[[nm]])) return(nm)                 # a display name
      d <- token_decode(nm)                               # maybe a token
      if (!is.null(d) && !is.null(idx[[d]])) d else NULL
    }
    displays <- function() nms
  } else {
    idx <- schema_types$idx
    raw_type_of <- schema_types$type_of
    resolve <- schema_types$resolve
    displays <- schema_types$displays
  }
  has_col <- function(nm)
    is.character(nm) && length(nm) == 1L && nzchar(nm) && !is.null(resolve(nm))
  type_of <- function(nm) {              # form-agnostic: resolve, then the raw lookup
    d <- resolve(nm)
    if (is.null(d)) NULL else raw_type_of(d)
  }
  alias_env <- new.env(parent = emptyenv())
  alias_of <- function(nm) {             # alias under the SCHEMA type, resolved on demand
    a <- alias_env[[nm]]
    if (!is.null(a)) return(a)
    d <- resolve(nm)
    if (is.null(d)) return(NULL)
    ty <- raw_type_of(d)
    if (is.null(ty)) return(NULL)
    a <- alias_encode(d, ty)
    assign(nm, a, envir = alias_env)
    a
  }

  pair_aliases <- character(0)          # aliases are injective -> dedup keys, order kept
  pair_seen <- new.env(parent = emptyenv())
  add_pair <- function(name, type) {
    d <- resolve(name)                   # pairs record DISPLAY names (the working vocabulary)
    if (is.null(d)) return()
    if (!(type %in% ALIAS_TYPES)) return()
    a <- alias_encode(d, type)
    if (is.null(pair_seen[[a]])) { pair_seen[[a]] <- TRUE; pair_aliases <<- c(pair_aliases, a) }
  }

  # types entry -> usable type string ("" = none). Scalar broadcasts; arrays index (legacy
  # :716/:750). "unknown"/invalid -> schema fallback (:720-724); still none -> "".
  # On-demand (preloading=FALSE): the slot's types are IGNORED — "" forces the
  # schema-type fallback in rewrite_name, the classic contract (see above).
  type_for <- function(t_entry, j) {
    if (!preloading) return("")
    t <- ""
    if (is.character(t_entry)) {
      t <- if (length(t_entry) == 1L) t_entry
           else if (length(t_entry) >= j) t_entry[j] else ""
    } else if (is.list(t_entry)) {
      t <- if (length(t_entry) >= j && is.character(t_entry[[j]]) && length(t_entry[[j]]) == 1L)
             t_entry[[j]] else ""
    }
    if (length(t) == 1L && t %in% ALIAS_TYPES) return(t)
    ""
  }
  rewrite_name <- function(name, t_entry, j) {
    if (!is.character(name) || length(name) != 1L || !nzchar(name)) return(name)
    d <- resolve(name)
    if (is.null(d)) return(name)        # not a schema column: passes through (legacy)
    ty <- type_for(t_entry, j)
    if (!nzchar(ty)) {
      sty <- raw_type_of(d)
      ty <- if (is.null(sty)) "" else sty
    }
    if (!nzchar(ty)) return(name)       # typeless: passes through (legacy)
    add_pair(d, ty)
    alias_encode(d, ty)                 # == token + "_" + ty when `name` was a token
  }

  # One {value, types, ...} node -> rewritten node (legacy _convertPreloadingDataOption).
  convert_variable_node <- function(node) {
    option_key <- node[["optionKey"]]
    option_key <- if (is.character(option_key) && length(option_key) == 1L) option_key else ""
    keep_original <- nzchar(option_key) && length(node) > 3L   # SEM model-node shape

    value_list <- node[["value"]]
    type_list  <- node[["types"]]
    single_value <- is.character(value_list)
    if (single_value) value_list <- as.list(value_list)
    if (is.character(type_list)) type_list <- as.list(type_list)
    if (!is.list(value_list)) return(node)      # unrecognised shape: leave alone
    if (!is.list(type_list)) type_list <- list()

    # one element: string (variable) or string array (interaction); for optionKey rows the
    # name lives under the row's option_key member (itself string or interaction array).
    # t_entry is the element's OWN types entry (type_list[[i]] — legacy :710); a scalar
    # type broadcasts over interaction components, an array indexes per component (:750).
    type_at <- function(i) if (length(type_list) >= i) type_list[[i]] else NULL
    rewrite_elem <- function(v, t_entry) {
      if (is.character(v) && length(v) == 1L)
        return(rewrite_name(v, t_entry, 1L))
      if (is.list(v) && length(v) > 0L &&
          all(vapply(v, function(x) is.character(x) && length(x) == 1L, logical(1L))))
        return(lapply(seq_along(v), function(j) rewrite_name(v[[j]], t_entry, j)))
      v
    }

    if (keep_original) {
      new_node <- node
      new_vals <- lapply(seq_along(value_list), function(i)
        rewrite_elem(value_list[[i]], type_at(i)))
      new_node[[option_key]] <- new_vals
      # Model-node dispositions (§3.7): model rewritten in place; columns aliased per the
      # parallel types; prefixedColumns aliased by SCHEMA type; modelOriginal untouched.
      # rewrite_syntax scans DISPLAY names (user-typed R code references them) — the
      # lazy accessor extracts them only when an R-code option actually exists.
      if (is.character(node[["modelOriginal"]])) {
        if (is.character(new_node[["model"]]) && length(new_node[["model"]]) == 1L)
          new_node[["model"]] <- rewrite_syntax(new_node[["model"]], displays(), alias_of)
        cols <- new_node[["columns"]]
        if (is.list(cols)) {
          for (k in seq_along(cols)) {
            cn <- cols[[k]]
            if (is.character(cn) && length(cn) == 1L && has_col(cn)) {
              cd <- resolve(cn)
              ty <- type_for(if (length(type_list) >= k) type_list[[k]] else NULL, 1L)
              if (!nzchar(ty)) ty <- raw_type_of(cd)
              add_pair(cd, ty)
              cols[[k]] <- alias_encode(cd, ty)
            }
          }
          new_node[["columns"]] <- cols
        }
        pc <- new_node[["prefixedColumns"]]
        if (is_obj(pc)) {
          for (pfx in names(pc)) {
            entries <- pc[[pfx]]
            if (is.list(entries)) {
              for (k in seq_along(entries)) {
                cn <- entries[[k]]
                if (is.character(cn) && length(cn) == 1L && has_col(cn)) {
                  add_pair(cn, type_of(cn))
                  entries[[k]] <- alias_of(cn)
                }
              }
              pc[[pfx]] <- entries
            }
          }
          new_node[["prefixedColumns"]] <- pc
        }
      }
      return(new_node)
    }

    if (nzchar(option_key)) {
      # rowComponent objects: rewrite the name under option_key, keep sibling members
      return(lapply(seq_along(value_list), function(i) {
        elem <- value_list[[i]]
        if (is_obj(elem) && !is.null(elem[[option_key]])) {
          elem[[option_key]] <- rewrite_elem(elem[[option_key]], type_at(i))
          elem
        } else rewrite_elem(elem, type_at(i))
      }))
    }

    new_vals <- lapply(seq_along(value_list), function(i)
      rewrite_elem(value_list[[i]], type_at(i)))
    if (single_value && length(new_vals) == 1L && !is.list(new_vals[[1L]])) new_vals[[1L]]
    else new_vals
  }

  # Structural walk: convert variable nodes, emit .types siblings, collect bare strings
  # (== schema column -> pair at schema type; rewrite is the meta pass's job — legacy).
  walk_object <- function(obj) {
    extras <- list()
    for (i in seq_along(obj)) {
      member <- obj[[i]]
      if (is_obj(member) && !is.null(member[["value"]]) && !is.null(member[["types"]])) {
        extras[[paste0(names(obj)[i], ".types")]] <- member[["types"]]   # ALWAYS (legacy :780/:796)
        obj[[i]] <- convert_variable_node(member)
      } else {
        obj[[i]] <- walk_any(member)
      }
    }
    for (nm in names(extras)) obj[[nm]] <- extras[[nm]]
    obj
  }
  walk_any <- function(x) {
    if (is_obj(x)) return(walk_object(x))
    if (is.list(x)) return(lapply(x, walk_any))
    if (is.character(x) && length(x) == 1L && has_col(x))
      add_pair(x, type_of(x))
    x
  }

  # Meta-driven rewrite (legacy _encodeColumnNamesinOptions :843-897): walk options and
  # .meta in parallel (object members by name; array+array per-index; object meta over an
  # array broadcasts). shouldEncode -> strict replacement of schema-column strings by
  # alias(name, SCHEMA type); isRCode -> rewrite_syntax. `.types` siblings are skipped.
  strict_rewrite <- function(x) {
    if (is.character(x)) {
      out <- vapply(x, function(s)
        if (has_col(s)) { add_pair(s, type_of(s)); alias_of(s) } else s,
        character(1L), USE.NAMES = FALSE)
      if (length(out) == 1L) out else as.list(out)
    } else if (is_obj(x)) {
      for (nm in names(x)) x[[nm]] <- strict_rewrite(x[[nm]])
      x
    } else if (is.list(x)) lapply(x, strict_rewrite)
    else x
  }
  rcode_rewrite <- function(x) {
    if (is.character(x) && length(x) == 1L) return(rewrite_syntax(x, displays(), alias_of))
    if (is_obj(x)) { for (nm in names(x)) x[[nm]] <- rcode_rewrite(x[[nm]]); return(x) }
    if (is.list(x)) return(lapply(x, rcode_rewrite))
    x
  }
  rewrite_by_meta <- function(opt, meta) {
    if (!is.list(meta)) return(opt)
    if (isTRUE(meta[["shouldEncode"]])) return(strict_rewrite(opt))
    if (isTRUE(meta[["isRCode"]]))      return(rcode_rewrite(opt))
    if (is_obj(meta)) {
      if (is_obj(opt)) {
        for (nm in names(opt)) {
          if (nm == ".meta" || nm == "types" || grepl("\\.types$", nm)) next
          if (!is.null(meta[[nm]])) opt[[nm]] <- rewrite_by_meta(opt[[nm]], meta[[nm]])
        }
        return(opt)
      }
      if (is.list(opt)) return(lapply(opt, function(el) rewrite_by_meta(el, meta)))
      return(opt)
    }
    # unnamed-list meta = JSON array: per-index
    if (is.list(opt) && !is_obj(opt)) {
      for (i in seq_len(min(length(opt), length(meta))))
        opt[[i]] <- rewrite_by_meta(opt[[i]], meta[[i]])
    }
    opt
  }

  # .meta encodeThis (FactorLevelListBase factors+levels, factorlevellistbase.cpp:108-117)
  # — collect recursively anywhere in .meta (legacy collectExtraEncodingsFromMetaJson).
  collect_encode_this <- function(meta) {
    if (!is.list(meta)) return()
    et <- meta[["encodeThis"]]
    if (!is.null(et)) {
      if (is.character(et)) et <- as.list(et)
      if (is.list(et)) for (e in et)
        if (is.character(e) && length(e) == 1L && has_col(e))
          add_pair(e, type_of(e))
    }
    for (i in seq_along(meta)) collect_encode_this(meta[[i]])
  }

  meta <- if (is_obj(options)) options[[".meta"]] else NULL
  if (is_obj(options)) options <- walk_object(options)
  collect_encode_this(meta)
  options <- rewrite_by_meta(options, meta)
  if (is_obj(options)) options[[".meta"]] <- NULL

  pairs <- if (length(pair_aliases)) {
    decoded <- lapply(pair_aliases, alias_decode)
    data.frame(name = vapply(decoded, function(d) d$name, character(1L)),
               type = vapply(decoded, function(d) d$type, character(1L)),
               stringsAsFactors = FALSE)
  } else data.frame(name = character(0), type = character(0), stringsAsFactors = FALSE)

  list(options = options, pairs = pairs)
}

# ── frame assembly + cache (§3.2 steps 3-4) ───────────────────────────────────

# Numeric → categorical cast, system semantics (D9 — identical to the worker's
# CatF64 plan and data.R's f64→nominal branch): non-finite is MISSING, never a
# category; levels = distinct values sorted numerically, rendered with the system
# jasp_level_string (%.15g grouping; values agreeing at 15 significant digits are
# ONE category — dedupe on the string). The migration sibling-coerce rung and the
# lazy fallback both flow through here, so every path a module can reach agrees
# with the views blob (the parity gate's whole claim).
factor_from_numeric <- function(vals, ordinal) {
  vals[!is.finite(vals)] <- NA
  u <- sort(unique(vals[!is.na(vals)]))
  labs <- vapply(u, jasp_level_string, character(1))
  labs <- labs[!duplicated(labs)]
  row_lab <- vapply(vals, function(x)
    if (is.na(x)) NA_character_ else jasp_level_string(x), character(1))
  factor(match(row_lab, labs), levels = seq_along(labs), labels = labs,
         ordered = isTRUE(ordinal))
}

# Per-column coercion semantics IDENTICAL to jaspRunner/R/data.R:74-99 (labels are already
# applied at read time; source vectors are schema-typed: factor for Arrow dictionaries,
# numeric otherwise). Dual-role views (§3.5) are just this applied twice to one source.
coerce_col <- function(vals, as_type) {
  if (is.factor(vals)) {
    if (as_type == "scale")
      return(as.numeric(levels(vals))[as.integer(vals)])   # VALUES, not codes
    if (as_type == "nominal" && is.ordered(vals))
      class(vals) <- "factor"                             # nominal is UNORDERED, always (D9)
    if (as_type == "ordinal" && !is.ordered(vals))
      class(vals) <- c("ordered", "factor")
    return(vals)
  }
  switch(as_type,
    scale   = vals,
    nominal = factor_from_numeric(vals, ordinal = FALSE),
    ordinal = factor_from_numeric(vals, ordinal = TRUE),
    vals)
}

.frame_from_cols <- function(cols) {
  if (!length(cols)) return(data.frame())
  structure(cols, class = "data.frame", row.names = c(NA_integer_, length(cols[[1L]])))
}

# Cache-backed schema-typed source read: one batched C++ col_select per miss-set
# (read_jasp_data prunes at the C++ level, jaspRunner/R/data.R). Unknown names throw
# (legacy rbridge parity — loud beats silent). Names arrive as DISPLAYS (the working
# vocabulary); read_jasp_data resolves them to storage fields (D11).
load_cols <- function(nms) {
  nms <- unique(nms[nzchar(nms)])
  if (!length(nms)) return(invisible(NULL))
  unknown <- nms[vapply(nms, function(n) is.null(schema_display_name(n)), logical(1L))]
  if (length(unknown))
    stop(sprintf("unknown column(s) requested: %s", paste(unknown, collapse = ", ")))
  missing <- nms[vapply(nms, function(n) is.null(.state$cols[[n]]), logical(1L))]
  if (length(missing)) {
    spec <- lapply(missing, function(n) list(name = n, as = schema_type_of(n) %||% "scale"))
    df <- read_jasp_data(.state$dataPath, spec)
    for (n in missing) assign(n, df[[n]], envir = .state$cols)
  }
  invisible(NULL)
}

# ── the view-read seam (slice B; runner-views-read-design.md §3) ─────────────
#
# view_refs PRESENT on the work envelope: the frame IS the analysis's — pruned,
# aliased, coerced by the worker. Read it (read_feather + rename) and serve it
# frame-first. ABSENT: today's lazy path, completely unchanged (the migration
# bridge — classic and unstapled v2 works never carry view_refs). Pass-through
# refs (view_id null, path = the base cache) are NEVER eagerly read — that would
# regress the terror_tall memory win exactly where pruning won it (D3); the lazy
# base path serves them.

# Read one materialized view blob into the MODULE frame (D11): fields are
# <token>_<type> — the storage token plus the cast type, which IS the alias the
# stateless codec would derive (alias_encode(display, type) == the field name), so
# the slice-B rename is an IDENTITY and the frame arrives pre-aliased. __base_row
# dropped (D8). The blob's token_map ({token: display_name} — display_name IS the
# decode) is cross-checked against the codec-derived display: a mismatch means the
# worker's base metadata disagrees with the stateless derivation — a non-deterministic
# or corrupt build, which AV8 forbids: LOUD, never papered over (D2). Ordered-ness
# rides the Arrow conversion faithfully (probe-pinned: dict_is_ordered survives the
# feather round-trip), so ordinal fields arrive as ordered factors — nothing re-derived.
view_frame_from_ref <- function(ref) {
  t0 <- now_s()
  tbl <- arrow::read_feather(ref$path, as_data_frame = FALSE)
  meta <- jsonlite::fromJSON(tbl$schema$metadata[["jasp:view"]], simplifyVector = FALSE)
  df <- as.data.frame(tbl)
  brc <- meta$base_row_column %||% "__base_row"
  fields <- setdiff(names(df), brc)
  stopifnot(length(fields) > 0L)
  # Split each field at the LAST "_": the token (hex never contains "_") + the type.
  parts <- strsplit(fields, "_", fixed = TRUE)
  token <- vapply(parts, function(p) paste(p[-length(p)], collapse = "_"), "")
  ty    <- vapply(parts, function(p) p[length(p)], "")
  real <- vapply(token, function(t) {
    d <- token_decode(t)
    if (is.null(d)) stop(sprintf("view blob '%s': field '%s' is not a token", basename(ref$path), t))
    d
  }, "")
  for (i in seq_along(fields)) {
    if (!ty[i] %in% ALIAS_TYPES)
      stop(sprintf("view blob '%s': bad type suffix '%s'", fields[i], ty[i]))
    want <- meta$token_map[[token[i]]]
    # NOTE: compare by VALUE — identical() is encoding-sensitive and JSON-borne
    # strings arrive native-marked while the codec's are UTF-8-marked.
    if (length(want) != 1L || !isTRUE(unname(want) == real[i]))
      stop(sprintf(
        "view blob token_map mismatch for '%s': blob=%s derived=%s — non-deterministic build (AV8)?",
        token[i], if (length(want)) want else "<absent>", real[i]))
    # The identity claim itself: the field IS the alias the codec derives.
    if (!isTRUE(alias_encode(real[i], ty[i]) == fields[i]))
      stop(sprintf("view blob field '%s' is not the alias of '%s'/%s — non-deterministic build (AV8)?",
        fields[i], real[i], ty[i]))
  }
  out <- list()
  for (i in seq_along(fields))
    out[[fields[i]]] <- df[[fields[i]]]      # identity: blob fields ARE the aliases
  log_step("view read", t0, sprintf("%s (%d rows x %d col(s), %d bytes)",
            basename(ref$path), nrow(df), length(fields), file.size(ref$path)))
  list(frame  = .frame_from_cols(out),
       fields = data.frame(name = unname(real), type = unname(ty), stringsAsFactors = FALSE))
}

# The primary view for THIS work: the first materialized ref (one per dataset
# input; the runner is single-dataset today — dataset_ids order decides).
primary_view_frame <- function() {
  if (is.null(.state$viewFrames) || !length(.state$viewFrames)) return(NULL)
  .state$viewFrames[[1L]]
}

# ── data natives (§3.3; args positional from .fromRCPP, common.R:387) ─────────

# Preload path (runJaspResults, common.R:109-110): the aliased pair frame (preload=true).
.readDataSetRequestedNative <- function() {
  if (is.null(.state$dataset)) return(data.frame())
  .state$dataset
}

# On-demand path (common.R:376-391): alias args decoded -> raw names -> cache-backed
# col_select -> coerce per the as.* args. Colnames ECHO what the module asked for (alias
# or raw), so module indexing by its own option strings always works. exclude.na.listwise
# is wrapper-side (common.R:388) — untouched.
#
# VIEW-FIRST (slice B): with a materialized view, the frame IS the analysis's — serve
# the exact (name, type) alias directly; on a miss, the sibling field of the same name
# coerced (the dual-role rung); the migration ladder's last rung is today's lazy base
# read (post-slice-D: a miss is a LOUD error — it means the spec derivation missed a
# column; silent coercion would hide a frontend bug).
.readDatasetToEndNative <- function(columns = NULL, columns.as.numeric = NULL,
                                    columns.as.ordinal = NULL, columns.as.factor = NULL,
                                    all.columns = FALSE) {
  if (isTRUE(all.columns)) {                   # the 5 free-syntax sites: full frame,
    nms <- schema_displays()                   # aliased by SCHEMA types (§3.3)
    tys <- schema_all_types()
    load_cols(nms)
    out <- list()
    for (nm in nms) {
      ty <- tys[[nm]]
      out[[alias_encode(nm, ty)]] <- coerce_col(.state$cols[[nm]], ty)
    }
    return(.frame_from_cols(out))
  }
  vf <- primary_view_frame()
  out <- list()
  serve <- function(requested, as) {
    if (is.null(requested)) return()
    for (s in unname(as.character(requested))) {
      if (is.na(s) || !nzchar(s)) next
      d <- alias_decode(s)
      raw <- if (is.null(d)) s else d$name
      ty <- as
      if (is.null(ty)) ty <- schema_type_of(raw) %||% "scale"
      out[[s]] <<- serve_column(s, raw, ty, vf)
    }
  }
  serve(columns,              NULL)
  serve(columns.as.numeric,   "scale")
  serve(columns.as.ordinal,   "ordinal")
  serve(columns.as.factor,    "nominal")
  .frame_from_cols(out)
}

# One requested symbol through the view-first ladder. `vf` = the primary view frame
# (NULL when absent — pure lazy path, byte-identical with today).
serve_column <- function(symbol, raw, ty, vf) {
  if (!is.null(vf)) {
    hit <- vf[[alias_encode(raw, ty)]]
    if (!is.null(hit)) return(hit)             # 1. the frame: already coerced by the worker
    flds <- .state$viewFields[[1L]]
    sib <- which(flds$name == raw)             # 2. sibling field (dual-role): same name,
    if (length(sib)) {                         #    other type — coerce (D9 semantics)
      src <- vf[[alias_encode(raw, flds$type[sib[1L]])]]
      if (!is.null(src)) return(coerce_col(src, ty))
    }
  }
  if (is.null(schema_display_name(raw)))      # 3. migration: the lazy base read
    stop(sprintf(".readDatasetToEndNative: unknown column '%s'", raw))
  load_cols(raw)
  coerce_col(.state$cols[[raw]], ty)
}

# Header path (common.R:405-418): names + types only — ZERO rows, aliased (the
# module lives in alias space). Zero callers today (audited); contract completeness.
# With a materialized view: names/types split from the frame's own fields (D6) —
# dual-role columns appear once per cast, exactly what the spec materialized.
.readDataSetHeaderNative <- function(columns = NULL, columns.as.numeric = NULL,
                                     columns.as.ordinal = NULL, columns.as.factor = NULL,
                                     all.columns = FALSE) {
  empty_col <- function(ty) switch(ty,
    scale = numeric(0), nominal = factor(character()),
    ordinal = ordered(factor(character())), character(0))
  flds <- if (length(.state$viewFields %||% list())) .state$viewFields[[1L]] else NULL
  if (isTRUE(all.columns)) {
    if (!is.null(flds)) {
      out <- lapply(seq_len(nrow(flds)), function(i) empty_col(flds$type[i]))
      names(out) <- vapply(seq_len(nrow(flds)),
                           function(i) alias_encode(flds$name[i], flds$type[i]), "")
      return(.frame_from_cols(out))
    }
    nms <- schema_displays()
    tys <- schema_all_types()
    out <- lapply(nms, function(nm) empty_col(tys[[nm]]))
    names(out) <- vapply(nms, function(nm) alias_encode(nm, tys[[nm]]),
                         character(1L), USE.NAMES = FALSE)
    return(.frame_from_cols(out))
  }
  requested <- unname(unlist(c(columns, columns.as.numeric,
                               columns.as.ordinal, columns.as.factor)))
  out <- list()
  for (s in requested) {
    if (is.null(s) || is.na(s) || !nzchar(s)) next
    d <- alias_decode(s)
    raw <- if (is.null(d)) s else d$name
    ty <- if (!is.null(d)) d$type else (schema_type_of(raw) %||% "scale")
    if (!is.null(flds)) {
      if (!any(flds$name == raw && flds$type == ty)) next
    } else if (is.null(schema_display_name(raw))) next
    out[[s]] <- empty_col(ty)
  }
  .frame_from_cols(out)
}

# Contract completeness ONLY: `.readFullDatasetToEnd` is NOT in jaspBase's .fromRCPP
# collection (Engine/jaspBase/R/common.R:628-637) — stop("Unknown RCPP object") fires
# before any globalenv lookup, so this is unreachable as jaspBase stands (zero module
# callers anyway). Defined so the native set is complete; HANDOVER §3.3.
.readFullDatasetToEnd <- function() {
  .readDatasetToEndNative(all.columns = TRUE)
}

# ── coder natives (§3.3) — served to jaspBase's encodeColNames/decodeColNames
# resolution via .findFun (writeImage.R:316-353). Until now these resolved to the
# identity dummy (writeImage.R:328-329); defining them in globalenv ends the silent
# identity mode. Schema-gated, NEVER throws (§2.2).

.encodeColNamesStrict <- function(x) {
  if (is.null(x)) return(x)
  vapply(unname(as.character(x)), schema_alias_of, character(1L), USE.NAMES = FALSE)
}
.encodeColNamesLax <- function(x) {
  if (is.null(x)) return(x)
  vapply(unname(as.character(x)), function(s)
    rewrite_syntax(s, schema_displays(), schema_alias_of),
         character(1L), USE.NAMES = FALSE)
}
.decodeColNamesStrict <- function(x) {
  if (is.null(x)) return(x)
  vapply(unname(as.character(x)), function(s) alias_decode_strict(s, .state$schemaGate),
         character(1L), USE.NAMES = FALSE)
}
.decodeColNamesLax <- function(x) {
  if (is.null(x)) return(x)
  vapply(unname(as.character(x)), function(s) alias_decode_lax(s, .state$schemaGate),
         character(1L), USE.NAMES = FALSE)
}

# Temp / state natives — resolve inside the per-work scratchpad (.state$outputDir ==
# work$output_dir, §19.3). The runner references artifacts by path relative to output_dir.
.requestTempRootNameNative <- function() {
  list(root = .state$outputDir)
}

.requestTempFileNameNative <- function(ext = "png") {
  .state$tempCounter <- .state$tempCounter + 1L
  list(root = .state$outputDir,
       relativePath = sprintf("jasp-%d.%s", .state$tempCounter, ext))
}

.requestStateFileNameNative <- function() {
  # .saveState/.retrieveState use $relativePath with save()/load(); an absolute path is cwd-proof.
  list(root = .state$outputDir,
       relativePath = file.path(.state$outputDir, "jaspState.RData"))
}

# Per-work VALUE natives (non-functions; .fromRCPP returns them directly). Reassigned per work.
.ppi            <- 96
.imageBackground <- "transparent"
.baseCitation   <- "JASP Team (2024). JASP (Version 0.19) [Computer software]."

# ── startup: load the framework (jaspBase); analysis modules load lazily (§1a) ─
#
# The runner is bound to one libpath but advertises every JASP module in it; each module is
# lazy-loaded the first time a work unit targets it (and then stays loaded — the runner never
# unloads, mirroring jaspBase's engine). Only the framework (jaspBase) is loaded at startup.

load_runner_modules <- function() {
  .libPaths(c(LIBDIR, .libPaths()))
  cat(sprintf("[runner] libpath: %s\n", LIBDIR))

  suppressMessages(library(jaspBase))           # + Rcpp::loadModule("jaspResults")
  cat(sprintf("[runner] jaspBase %s loaded\n", as.character(packageVersion("jaspBase"))))

  # Bridge natives are now defined (above). initEnvironment() calls .requestTempRootNameNative
  # to setwd, so it must come AFTER the bridge is in place.
  tryCatch(jaspBase::initEnvironment(),
           error = function(e) cat(sprintf("[runner] initEnvironment warning: %s\n", conditionMessage(e))))
}

# Detect the JASP analysis modules installed in a library — what the runner advertises. Two tests:
#   1. Name prefix "jasp" — a cheap string pre-filter that skips the ~100 dependency packages
#      (ggplot2, Rcpp, ...) with no filesystem access. JASP modules are always named jasp<X>.
#   2. Ships a Description.qml (the analysis description form) — the authoritative discriminator,
#      and what the desktop keys on (dynamicmodules.cpp checks for inst/Description.qml, which lands
#      at the package root once installed). This is what separates modules from the jasp-prefixed
#      framework packages (jaspBase, jaspGraphs), which ship no Description.qml.
# Returns a list of list(name, version); the version is read from each module's R DESCRIPTION.
scan_modules <- function(libdir) {
  pkgs <- list.dirs(libdir, full.names = FALSE, recursive = FALSE)
  pkgs <- pkgs[startsWith(pkgs, "jasp")]        # fast pre-filter: jasp<X> (deps have no jasp prefix)
  is_module <- function(pkg) file.exists(file.path(libdir, pkg, "Description.qml"))
  lapply(Filter(is_module, pkgs), function(pkg) {
    desc <- read.dcf(file.path(libdir, pkg, "DESCRIPTION"))
    list(name = pkg, version = as.character(desc[1, "Version"]))
  })
}

# ─────────────────────────────────────────────────────────────────────────────
# jaspBase accommodation — REQUIRED to run jaspBase UNALTERED.
#
# jaspBase's incremental recompute (reusing a previous run's cached plots) needs
# TWO things to be loadable when a new revision runs, and the unmodified C++
# jaspResults gates both behind locations we must set explicitly:
#
#   (a) the WRITE SEAL gate (lastWriteWorked()): a marker file
#       (jaspResultsFinishedWriting.txt) that complete()/finishWriting() writes.
#       setWriteSealLocation() tells jaspResults WHERE; copy-on-seed brings the
#       base's seal into the new dir so lastWriteWorked() returns true.
#   (b) the RESULTS JSON (jaspResults.json): complete()/saveResults() writes it,
#       loadResults() reads it back so getOldPlotInfo() can match old plots.
#       setSaveLocation() tells jaspResults WHERE; without it saveResults() no-ops
#       ("Did not store jaspResults") and recompute finds nothing to reuse.
#
# The write seal's ORIGINAL purpose — guarding a SHARED, in-place state file
# against a mid-write crash — is moot in our model: each revision is a self-
# contained results_<rev>/ dir, the base is immutable, nothing is shared or
# reused in place. But the unmodified C++ still uses the seal as the gate, so we
# set its location. Everything in this block exists solely to drive jaspBase's
# unmodified recompute path; none of it is needed for our model's correctness.
# (The seal fakery itself disappears once the libpath jaspBase is rebuilt with
# the JASP_RESULTS_NO_SEAL gate — see setup_seal_bypass; the rest stays.)
# ─────────────────────────────────────────────────────────────────────────────

# Point jaspBase's write-seal gate (lastWriteWorked) at this work's output dir.
# Must run BEFORE runJaspResults: the C++ jaspResults constructor reads the seal
# location when it decides whether to load the seeded state. complete() writes the
# seal here (via finishWriting()); no separate seal step is needed.
jaspbase_set_seal_location <- function(outputDir)
  jaspBase:::setWriteSealLocation(outputDir, jaspBase:::writeSealFilename())

# Point jaspBase's results-JSON save/load location at this work's output dir, so
# complete()/saveResults() writes jaspResults.json here and the next run's
# loadResults() loads it (after copy-on-seed), enabling plot reuse. Without this,
# saveResults() no-ops ("Did not store jaspResults") and recompute finds nothing.
jaspbase_set_save_location <- function(outputDir)
  jaspBase:::setSaveLocation(outputDir, "jaspResults.json")

# Copy-on-seed: seed this revision's dir from a finished base revision so jaspBase
# reuses the base's cached plots. Copies the base's state (jaspState.RData),
# write-seal, and images into outputDir. Copying the seal is what makes
# lastWriteWorked() return true -> jaspBase loads the state -> recompute. Returns
# TRUE if a base was actually seeded (so the caller can continue the temp counter).
jaspbase_seed_from_base <- function(baseDir, outputDir) {
  if (is.null(baseDir) || length(baseDir) != 1 || !nzchar(baseDir) || !dir.exists(baseDir))
    return(FALSE)
  files <- list.files(baseDir, all.files = TRUE, no.. = TRUE)
  if (length(files))
    file.copy(file.path(baseDir, files), outputDir, overwrite = TRUE)
  TRUE
}

# Highest jasp-<N> temp-file index already in outputDir (from a seeded base). New
# temp files must continue numbering, otherwise a recomputed plot would clobber a
# reused plot's image (both use jasp-<N>.<ext> naming).
jaspbase_max_temp_index <- function(outputDir) {
  nums <- as.integer(sub("^jasp-([0-9]+)\\..*$", "\\1",
                         list.files(outputDir, pattern = "^jasp-[0-9]+\\.")))
  if (length(nums) == 0) 0L else max(nums, na.rm = TRUE)
}



# ── per work: run the analysis via jaspBase, return list(results, status) ─────

run_analysis <- function(work) {
  payload  <- work$payload
  module   <- payload$module %||% MODULE
  analysis <- payload$analysis
  settings <- payload$settings %||% list()

  # Lazy-load the target module on first use (it then stays loaded — the runner never unloads,
  # mirroring jaspBase's engine). Only the framework was loaded at startup; analysis modules load
  # here, on demand, the first time a work unit targets them.
  if (!isNamespaceLoaded(module)) {
    t_mod <- now_s()
    suppressMessages(library(module, character.only = TRUE))
    cat(sprintf("[runner] lazy-loaded module %s %s in %.3f s\n",
                module, as.character(packageVersion(module)), now_s() - t_mod))
  }

  # 1. Dataset schema + options walk + pruning (HANDOVER-runner-data-pruning.md §3.2).
  # Only the Arrow FOOTER is read up front. The options walk (legacy case catalog,
  # columnencoder.cpp:673-826) derives the used (name, type) pairs and rewrites the option
  # values raw->alias — everything jaspBase/modules see from here on is aliases; raw UTF-8
  # names never enter R. preloadData=false reads NOTHING until a native asks.
  ds_paths <- work$dataset_paths %||% list()
  data_path <- NULL
  for (p in ds_paths) {
    if (is.character(p) && length(p) == 1 && nzchar(p) && file.exists(p)) { data_path <- p; break }
  }
  if (is.null(data_path)) stop("no readable dataset path in work$dataset_paths")

  preloadData <- payload$preloadData %||% TRUE

  t_schema <- now_s()
  schema <- tryCatch(read_feather_schema(data_path),
                     error = function(e) { cat(sprintf("[runner] schema read error: %s\n", conditionMessage(e))); NULL })
  if (is.null(schema)) stop("could not read dataset schema")
  .state$dataPath    <- data_path
  .state$schema      <- schema
  .state$schemaNames <- schema$names            # vectorized: full width — the FIELD names (D11 tokens)
  # token -> 0-based field index; Schema$field(i) is O(1) (GetFieldByName is an O(k) scan).
  # Types and displays are NOT extracted here — schema_type_of/schema_display_name
  # resolve them lazily per used column (the wide-file start stays O(1) in schema reads).
  .state$schemaIdx   <- list2env(as.list(setNames(seq_along(.state$schemaNames) - 1L,
                                                  .state$schemaNames)), parent = emptyenv())
  .state$typeCache   <- new.env(parent = emptyenv())
  .state$aliasCache  <- new.env(parent = emptyenv())
  .state$displayNames <- NULL                    # lazy (schema_displays, on demand)
  .state$schemaGate  <- function(nm) !is.null(schema_display_name(nm))  # decode gate (D11)
  .state$cols        <- new.env(parent = emptyenv())
  log_step("schema read", t_schema,
           sprintf("%s (%d cols)", data_path, length(.state$schemaNames)))

  # ── the view-read seam (slice B): view_refs present → the frame IS the analysis's.
  # Materialized refs are read ONCE here (small by construction — the spec IS the
  # prune); pass-through refs are never eager-read (D3). Absent → the lazy path
  # below, byte-identical with today (the migration bridge).
  .state$viewFrames  <- NULL
  .state$viewFields  <- NULL
  view_refs <- work$view_refs
  if (!is.null(view_refs)) {
    t_views <- now_s()
    for (ref in view_refs) {
      if (is.null(ref$view_id) || !nzchar(ref$view_id)) {
        cat(sprintf("[runner] view ref %s: pass-through (lazy base path)\n", ref$dataset_id))
        next
      }
      v <- view_frame_from_ref(ref)
      # Store under the dataset_id order (dataset_ids decides "primary").
      .state$viewFrames[[length(.state$viewFrames %||% list()) + 1L]] <- v$frame
      .state$viewFields[[length(.state$viewFields %||% list()) + 1L]] <- v$fields
    }
    # D6: the walk's type fallback consults the FRAME FIELDS first — the view's
    # requested type is what the module expects (a dual-role column's first cast
    # in spec order wins for untyped slots). Seeding the type cache does exactly
    # that; the schema footer remains the fallback (and serves pass-through).
    if (length(.state$viewFields %||% list()))
      for (i in seq_len(nrow(.state$viewFields[[1L]]))) {
        f <- .state$viewFields[[1L]][i, ]
        assign(f$name, f$type, envir = .state$typeCache)
      }
    log_step("view seam", t_views,
             sprintf("%d materialized, %d pass-through",
                     length(.state$viewFrames %||% list()),
                     length(view_refs) - length(.state$viewFrames %||% list())))
  }

  t_walk <- now_s()
  raw_options <- payload$options %||% list()
  if (VERBOSE) cat(sprintf("[runner] raw options (pre-walk): %s\n",
                           toJSON(raw_options, auto_unbox = TRUE, null = "null", digits = NA)))
  walked <- walk_and_rewrite_options(raw_options,
    list(idx = .state$schemaIdx, type_of = schema_type_of,
         resolve = schema_display_name, displays = schema_displays),
    preloading = preloadData)
  options <- walked$options
  pairs   <- walked$pairs
  log_step("options walk", t_walk, sprintf("%d pair(s)", nrow(pairs)))
  if (preloadData && nrow(pairs) == 0L)
    cat(paste0(
      "[runner] WARN: preloadData=true but the options walk derived NO (name, type) pairs.\n",
      "[runner]        The preload frame is empty; a module indexing the dataset will fail.\n",
      "[runner]        Fine for analyses with no variable bindings; if unexpected, re-run with\n",
      "[runner]        JASP_RUNNER_VISIBLE=1 and inspect the raw-options dump above the walk.\n"))

  t_data <- now_s()
  vf <- primary_view_frame()
  if (!is.null(vf) && preloadData) {
    # The view frame IS the preload frame (D5): already pruned, aliased, coerced.
    # Migration safety: any walked pair NOT in the frame (a hand-stapled spec that
    # missed a column) is appended via the lazy path — loud divergence beats a
    # silently empty column. Post-slice-C the superset guarantee makes this dead.
    cols <- as.list(vf)
    for (i in seq_len(nrow(pairs))) {
      a <- alias_encode(pairs$name[i], pairs$type[i])
      if (is.null(cols[[a]])) {
        load_cols(pairs$name[i])
        cols[[a]] <- coerce_col(.state$cols[[pairs$name[i]]], pairs$type[i])
        cat(sprintf("[runner] WARN: pair '%s'/%s not in the view spec — lazy-appended\n",
                    pairs$name[i], pairs$type[i]))
      }
    }
    .state$dataset <- .frame_from_cols(cols)
    log_step("preload frame (view)", t_data,
             sprintf("%d rows x %d aliased col(s) (%d pair(s) checked)",
                     nrow(.state$dataset), ncol(.state$dataset), nrow(pairs)))
  } else if (preloadData && nrow(pairs) > 0L) {
    load_cols(unique(pairs$name))
    cols <- list()
    for (i in seq_len(nrow(pairs))) {
      nm <- pairs$name[i]; ty <- pairs$type[i]
      cols[[alias_encode(nm, ty)]] <- coerce_col(.state$cols[[nm]], ty)
    }
    .state$dataset <- .frame_from_cols(cols)
    log_step("preload frame", t_data,
             sprintf("%d rows x %d aliased col(s) from %d pair(s)",
                     nrow(.state$dataset), ncol(.state$dataset), nrow(pairs)))
  } else {
    .state$dataset <- NULL
    log_step(if (preloadData) "preload frame (no pairs)" else "no preload (on-demand)", t_data,
             sprintf("%d pair(s)", nrow(pairs)))
  }

  # 2. scratchpad — per-work and absolute (§19.3). Reclaim the process cwd for this work:
  # jaspBase's initEnvironment() setwd's to a startup tempdir once, so without this the cwd would
  # be a stray tempdir forever. Keeping cwd == this work's scratchpad means any bare relative
  # write lands here, and removes any need for `.runnerRoot`-style cwd bookkeeping.
  .state$outputDir <- work$output_dir %||% file.path(tempdir(), work$work_id %||% "work")
  dir.create(.state$outputDir, recursive = TRUE, showWarnings = FALSE)
  setwd(.state$outputDir)

  # jaspBase recompute seeding (see the "jaspBase accommodation" block above): point the
  # write-seal + results-JSON locations at this dir, then seed from the base revision (if the
  # frontend named one) so jaspBase reuses its cached plots. The temp counter continues past the
  # base's images so recomputed plots don't clobber reused ones (both use jasp-<N>.<ext> naming).
  jaspbase_set_seal_location(.state$outputDir)
  jaspbase_set_save_location(.state$outputDir)
  t_seed <- now_s()
  seeded <- jaspbase_seed_from_base(work$base_results_dir, .state$outputDir)
  .state$tempCounter <- if (seeded) jaspbase_max_temp_index(.state$outputDir) else 0L
  # Seeding copies the base revision's state (jaspState.RData) + seal + images —
  # with a bloated state file this copy is a real cost, so it gets its own timing.
  # Report what was copied (right after the copy, outputDir holds exactly the seeded
  # files): the seeded jasp-* artifact count is the baseline that tells us later
  # whether plots were reused from state or re-rendered.
  seeded_artifacts <- 0L
  if (seeded) {
    fi <- file.info(file.path(.state$outputDir,
                              list.files(.state$outputDir, all.files = TRUE, no.. = TRUE)))
    seeded_artifacts <- sum(grepl("^jasp-[0-9]+\\.", basename(rownames(fi))))
    state_sz <- fi[basename(rownames(fi)) == "jaspState.RData", "size"]
    log_step("copy-on-seed", t_seed, sprintf(
      "from %s: %d files, %.1f MB (state %.1f MB, %d jasp-* artifacts)",
      work$base_results_dir, nrow(fi), sum(fi$size) / 1e6,
      if (length(state_sz)) state_sz / 1e6 else 0, seeded_artifacts))
  } else {
    log_step("copy-on-seed", t_seed, "no base (fresh run)")
  }

  # 3. per-work value natives (invariant work unit carries its own settings)
  .ppi             <<- settings$ppi %||% 96
  .imageBackground <<- settings$imageBackground %||% "transparent"
  # .baseCitation left at session default unless settings provides one
  if (!is.null(settings$baseCitation)) .baseCitation <<- settings$baseCitation

  # 4. run (the engine path: Internal fn + walked/aliased options)
  optionsJson    <- toJSON(options, auto_unbox = TRUE, null = "null", digits = NA)
  functionCall   <- paste0(module, "::", analysis, "Internal")
  cat(sprintf("[runner] running %s (work_id=%s revision=%s preloadData=%s)\n",
              functionCall, work$work_id %||% "?", work$revision %||% "?", preloadData))
  if (VERBOSE) cat(sprintf("[runner] optionsJson (walked): %s\n", optionsJson))

  t_run <- now_s()
  jr <- jaspBase::runJaspResults(
    name         = analysis,
    title        = analysis,
    dataKey      = "{}",
    options      = optionsJson,
    stateKey     = "{}",
    functionCall = functionCall,
    preloadData  = preloadData)
  log_step("runJaspResults", t_run)
  # What the run left behind. jaspState.RData size matters twice: it is the payload of
  # every later copy-on-seed, and it's what bloated to 200+ MB before figures drop
  # (with it, expect only the `other` state: sub-MB — the gate lives in jaspBase's
  # finishJaspResults, set by setup_figures_drop). And jasp-* artifacts appearing
  # ON TOP of the seeded ones mean plots were re-rendered instead of reused (reuse keys
  # off the results JSON + dependency pruning, not the live plot object).
  fi <- file.info(file.path(.state$outputDir,
                            list.files(.state$outputDir, all.files = TRUE, no.. = TRUE)))
  state_sz <- fi[basename(rownames(fi)) == "jaspState.RData", "size"]
  artifacts <- sum(grepl("^jasp-[0-9]+\\.", basename(rownames(fi))))
  cat(sprintf("[runner]   artifacts: state %.1f MB, dir %.1f MB / %d files, jasp-* %d -> %d\n",
              if (length(state_sz)) state_sz / 1e6 else 0, sum(fi$size) / 1e6, nrow(fi),
              seeded_artifacts, artifacts))

  # 5. native serialization (live web form: .meta + top-level nodes) + alias decode —
  # results leave R carrying REAL names (lax decode, schema-gated, single pass, §2.2;
  # successor of the engine's decodeJsonSafeHtml).
  t_ser <- now_s()
  jaspObject  <- jr$.__enclos_env__$private$jaspObject
  resultsJson <- jaspObject$getResults()
  parsed      <- fromJSON(resultsJson, simplifyVector = FALSE)
  parsed      <- lax_decode_tree(parsed)
  results     <- parsed$results
  log_step("serialize results", t_ser, sprintf("%d bytes JSON", nchar(resultsJson)))

  status <- tryCatch(jr$status, error = function(e) NULL)
  if (is.null(status) || !nzchar(status))
    status <- if (isTRUE(results$error)) "fatalError" else "complete"

  # Provenance (§19.4): the version of the module that actually produced this result.
  module_version <- tryCatch(as.character(packageVersion(module)), error = function(e) NULL)

  list(results = results, status = status, module_version = module_version)
}

# ── main: dial, register, loop ────────────────────────────────────────────────

main <- function(control_url = ORCH_URL) {
  # Provenance: the script is read ONCE, at spawn — a long-lived runner keeps executing the
  # code it booted with even after the file changes on disk. Log path + mtime so a stale
  # instance is spotted the moment it answers a work (kill orchestrator+runner to reload).
  fa <- commandArgs(trailingOnly = FALSE)
  f <- sub("^--file=", "", fa[grepl("^--file=", fa)])
  if (length(f))
    cat(sprintf("[runner] script: %s (mtime %s)\n", normalizePath(f[1L]),
                format(file.mtime(f[1L]), "%Y-%m-%d %H:%M:%S")))

  t_load <- now_s()
  load_runner_modules()
  log_step("startup: load jaspBase", t_load)
  log_provenance()
  install_state_timers()
  setup_figures_drop()
  setup_seal_bypass()

  cat(sprintf("[runner] dialling orchestrator control endpoint at %s\n", control_url))

  # ── Handshake on the control endpoint (REQ/REP, §17.2/§19.5) ────────────────
  # REQ-dial the well-known control endpoint, send `register` advertising EVERY JASP module in the
  # bound libpath (truthful capability, §1a — the runner can run any of them, lazy-loading on
  # demand), each with a `base_uri` — the file:// directory the frontend lazy-loads that module's
  # assets (Description.qml, qml/, icons/, help/) from, feeding the orchestrator's module catalog —
  # and read the `register_ack` carrying the orchestrator-assigned runner_id and a dedicated PAIR
  # data-channel URL.
  t_reg <- now_s()
  modules <- scan_modules(LIBDIR)
  if (length(modules) == 0) {
    cat(sprintf("[runner] WARNING: no JASP modules found in %s; advertising fallback %s@%s\n",
                LIBDIR, MODULE, MODULE_VER))
    modules <- list(list(name = MODULE, version = MODULE_VER))
  }
  cat(sprintf("[runner] advertising %d module(s): %s\n", length(modules),
              paste(vapply(modules, function(m) sprintf("%s@%s", m$name, m$version), ""),
                    collapse = ", ")))
  req <- socket("req", dial = control_url)
  register_msg <- list(
    v         = 1,
    id        = sprintf("rn-reg-%s", format(Sys.time(), "%s")),
    type      = "register",
    runner_id = sprintf("runner-%s", Sys.getpid()),
    capabilities = lapply(modules, function(m)
      list(kind = "analysis_r_classic_jaspbase", name = m$name, version = m$version,
           base_uri = paste0("file://", file.path(LIBDIR, m$name), "/"))),
    priority  = 0,
    environment = list(r_version = as.character(getRversion()))
  )
  send(req, pack_envelope(toJSON(register_msg, auto_unbox = TRUE, null = "null")),
       mode = "raw", block = 3000L)
  cat("[runner] sent register\n")

  cv <- cv()
  ra <- recv_aio(req, mode = "raw", cv = cv)
  if (!until(cv, 5000L)) stop("[runner] register_ack timeout")
  ack_raw <- call_aio(ra)$data
  if (is_err(ack_raw) || length(ack_raw) == 0) stop("[runner] register_ack not received")
  ack <- fromJSON(parse_envelope(ack_raw)$json, simplifyVector = FALSE)
  if (!isTRUE(ack$ok)) stop("[runner] registration rejected: ", ack$reason %||% "?")
  channel_url <- ack$channel_url
  if (is.null(channel_url) || !nzchar(channel_url)) stop("[runner] register_ack missing channel_url")
  cat(sprintf("[runner] registered as %s; dialling data channel %s\n", ack$runner_id, channel_url))
  close(req)  # the control connection is transient (one request→reply)
  log_step("startup: register handshake", t_reg, sprintf("%d module(s)", length(modules)))

  # ── Data channel (PAIR v1): all work/result traffic flows here ──────────────
  sock <- socket("poly", dial = channel_url)
  on.exit(close(sock), add = TRUE)
  cat("[runner] waiting for work...\n")

  repeat {
    send_activity(sock)   # liveness: we reached the loop (alive + ready) — §19.4/§25.4
    ra <- recv_aio(sock, mode = "raw", cv = cv)
    if (!until(cv, 3600000L)) { cat("[runner] idle timeout (1hr), exiting\n"); break }
    msg <- call_aio(ra)$data
    if (is_err(msg) || length(msg) == 0) { cat("[runner] pipe closed / recv error, exiting\n"); break }

    parsed <- tryCatch(parse_envelope(msg), error = function(e) NULL)
    if (is.null(parsed)) next
    work <- tryCatch(fromJSON(parsed$json, simplifyVector = FALSE), error = function(e) NULL)
    if (is.null(work)) next
    if ((work$type %||% "") != "work") {
      # Loop-top type-switch (§7 as-built gap 2): `abort` here is a RACED COMPLETION —
      # the router pushed it while we were finishing; the work it names already
      # terminaled. A documented NO-OP (the socket is drained; nothing is lost).
      if ((work$type %||% "") == "abort")
        cat(sprintf("[runner] abort popped at loop-top (raced completion; work_id=%s) — no-op\n",
                    work$work_id %||% "?"))
      else
        cat(sprintf("[runner] ignoring type=%s\n", work$type))
      next
    }

    cat(sprintf("[runner] <- work work_id=%s revision=%s base_revision=%s analysis=%s\n",
                work$work_id, work$revision, work$base_revision %||% "-",
                work$payload$analysis %||% "?"))
    t_work <- now_s()

    # The abort plane: armed for the whole run, drained at its boundaries (and at
    # jaspBase checkpoints, once the bridge calls jasp_checkpoint()).
    arm_abort_plane(sock, work)
    out <- tryCatch(
      run_analysis(work),
      jaspAbort = function(c) {
        cat(sprintf("[runner] cooperative unwind: %s\n", conditionMessage(c)))
        list(results = list(title = "aborted",
                            errorMessage = conditionMessage(c)),
             status = "aborted")
      },
      error = function(e) {
        cat(sprintf("[runner] analysis error: %s\n", conditionMessage(e)))
        list(results = list(error = TRUE, errorMessage = conditionMessage(e),
                            title = work$payload$analysis %||% "error"),
             status = "fatalError")
      })
    if (isTRUE(disarm_abort_plane(sock)) && (out$status %||% "") != "aborted") {
      # The abort landed after the last checkpoint but before the run finished:
      # the run's own result is stale at the router (revision check) — report the
      # abort, the truthful terminal for what the router asked.
      cat("[runner] abort drained at run boundary — reporting aborted\n")
      out <- aborted_result(work)
    }

    result_msg <- list(
      v          = 1,
      id         = sprintf("rn-%s", format(Sys.time(), "%s")),
      reply_to   = work$id,
      session_id = work$session_id,  # echo: lets the orchestrator correlate (session_id, work_id)
      type       = "result",
      work_id    = work$work_id,
      revision   = work$revision,
      status     = out$status,
      # Adjacently-tagged kind+payload (§19.2 Result payloads by kind): an analysis result
      # carries the opaque jaspResults tree in payload$results; the orchestrator fills
      # results_dir when it forwards. module_version = the producer's provenance (§19.4):
      # what actually ran, else what the work asked for.
      kind       = "analysis_r_classic_jaspbase",
      module_version = out$module_version %||% work$payload$module_version,
      payload    = list(results = out$results)
    )
    reply_raw <- pack_envelope(toJSON(result_msg, auto_unbox = TRUE, null = "null", na = "null"))
    send(sock, reply_raw, mode = "raw", block = 3000L)
    cat(sprintf("[runner] -> result work_id=%s status=%s (%d bytes)\n",
                work$work_id, out$status, length(reply_raw)))
    log_step(sprintf("work %s total", work$work_id), t_work,
             sprintf("status=%s", out$status))
  }
}

args <- commandArgs(trailingOnly = TRUE)
url <- if (length(args) >= 1 && !is.na(args[1])) args[1] else ORCH_URL
main(url)
