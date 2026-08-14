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

# Liveness ping (§19.4/§25.4). Sent once per loop pass — reaching the loop is real activity (a work
# cycle finished / runner ready), so no work_id is attached and there is nothing to rate-limit: the
# cadence is bounded by the work rate itself. The orchestrator bumps the runner's last_activity on
# any recv, so this resets its hang-detector clock.
send_activity <- function(sock) {
  act <- list(v = 1, id = sprintf("rn-act-%s", format(Sys.time(), "%s")), type = "activity")
  send(sock, pack_envelope(toJSON(act, auto_unbox = TRUE, null = "null")),
       mode = "raw", block = 100L)
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
.state$dataset     <- NULL          # current work's data.frame
.state$outputDir   <- tempdir()     # current work's scratchpad (== work$output_dir)
.state$tempCounter <- 0L

# Derive a read_jasp_data() columns_spec from an Arrow schema: dictionary-encoded columns are
# categorical (ordinal iff the Arrow `ordered` flag is set), everything else is scale. Mirrors the
# §8.3 type map. No type GUESSING — Arrow carries the real types, so the old character->factor
# `.typeDataset` coercion is gone. (The full design gets the schema from the orchestrator's
# `dataset_ready`, §19.2; the alpha reads it off the Feather file.)
.spec_from_schema <- function(sch) {
  lapply(sch$names, function(nm) {
    typ <- sch$GetFieldByName(nm)$type
    list(name = nm,
         as   = if (inherits(typ, "DictionaryType")) {
                  if (isTRUE(typ$ordered)) "ordinal" else "nominal"
                } else "scale")
  })
}

# Unwrap the frontend's option form into what an analysis expects. boundValues() sends variable
# options as {value, types} objects plus a top-level `.meta` (shouldEncode flags); the old C++
# ColumnEncoder unwrapped these and encoded column names before R ever saw them. The runner has
# no C++ encoder, so we unwrap here. Column-name ENCODING is the identity for simple names
# (the alpha's x/y/group); real datasets with special-char names need encodeColNames later (TODO).
.processOptions <- function(options) {
  for (nm in setdiff(names(options), ".meta")) {
    opt <- options[[nm]]
    if (is.list(opt) && !is.null(opt[["value"]]))
      options[[nm]] <- opt[["value"]]
  }
  options[[".meta"]] <- NULL
  options
}

# Full dataset, typed. Called by runJaspResults when preloadData=TRUE (the engine path for
# jaspTTests). Also the fallback for any analysis that reads the whole frame. The frame is already
# correctly typed by read_jasp_data() (Arrow dictionaries -> factors, float64 -> numeric), so it is
# returned as-is.
.readDataSetRequestedNative <- function() {
  if (is.null(.state$dataset)) return(data.frame())
  .state$dataset
}

# Column-wise read (for analyses that read on demand). Args are positional from .fromRCPP:
# (columns, columns.as.numeric, columns.as.ordinal, columns.as.factor, all.columns).
.readDatasetToEndNative <- function(columns = NULL, columns.as.numeric = NULL,
                                    columns.as.ordinal = NULL, columns.as.factor = NULL,
                                    all.columns = FALSE) {
  df <- .state$dataset
  if (is.null(df)) return(data.frame())
  if (isTRUE(all.columns)) {
    wanted <- names(df)
  } else {
    wanted <- unique(unlist(c(columns, columns.as.numeric, columns.as.ordinal, columns.as.factor)))
    wanted <- wanted[!vapply(wanted, is.null, logical(1))]
  }
  wanted <- intersect(wanted, names(df))
  if (length(wanted) == 0) return(data.frame())
  out <- df[, wanted, drop = FALSE]
  for (col in intersect(columns.as.numeric, names(out))) out[[col]] <- as.numeric(as.character(out[[col]]))
  for (col in intersect(columns.as.ordinal, names(out))) out[[col]] <- factor(out[[col]], ordered = TRUE)
  for (col in intersect(columns.as.factor,  names(out))) out[[col]] <- factor(out[[col]])
  out
}

# Header read — analyses use it to learn column names/types without the full data. Returning
# the (typed) requested columns is a superset that satisfies those callers.
.readDataSetHeaderNative <- function(columns = NULL, columns.as.numeric = NULL,
                                     columns.as.ordinal = NULL, columns.as.factor = NULL,
                                     all.columns = FALSE) {
  .readDatasetToEndNative(columns, columns.as.numeric, columns.as.ordinal, columns.as.factor, all.columns)
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

  # 1. dataset — Arrow/Feather cache file (§8), ABSOLUTE path injected by the orchestrator (§19.3).
  # Read through jaspRunner's unified path with a schema-derived spec: Arrow carries the real
  # types, so jaspBase gets a properly typed frame (factor grouping vars, numeric measurements).
  ds_paths <- work$dataset_paths %||% list()
  data_path <- NULL
  for (p in ds_paths) {
    if (is.character(p) && length(p) == 1 && nzchar(p) && file.exists(p)) { data_path <- p; break }
  }
  if (is.null(data_path)) stop("no readable dataset path in work$dataset_paths")
  t_data <- now_s()
  .state$dataset <- tryCatch({
    sch <- arrow::read_feather(data_path, as_data_frame = FALSE)$schema
    read_jasp_data(data_path, .spec_from_schema(sch))
  }, error = function(e) { cat(sprintf("[runner] Arrow read error: %s\n", conditionMessage(e))); NULL })
  if (is.null(.state$dataset)) stop("could not read dataset")
  log_step("dataset read", t_data,
           sprintf("%s (%d rows, %d cols)", data_path, nrow(.state$dataset), ncol(.state$dataset)))

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

  # 4. run (the engine path: Internal fn + preloadData from the work, default TRUE)
  preloadData    <- payload$preloadData %||% TRUE
  optionsJson    <- toJSON(.processOptions(payload$options %||% list()),
                           auto_unbox = TRUE, null = "null", digits = NA)
  functionCall   <- paste0(module, "::", analysis, "Internal")
  cat(sprintf("[runner] running %s (work_id=%s revision=%s preloadData=%s)\n",
              functionCall, work$work_id %||% "?", work$revision %||% "?", preloadData))
  if (VERBOSE) cat(sprintf("[runner] optionsJson (processed): %s\n", optionsJson))

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

  # 5. native serialization (live web form: .meta + top-level nodes)
  t_ser <- now_s()
  jaspObject  <- jr$.__enclos_env__$private$jaspObject
  resultsJson <- jaspObject$getResults()
  parsed      <- fromJSON(resultsJson, simplifyVector = FALSE)
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
      list(kind = "analysis", name = m$name, version = m$version,
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
    if ((work$type %||% "") != "work") { cat(sprintf("[runner] ignoring type=%s\n", work$type)); next }

    cat(sprintf("[runner] <- work work_id=%s revision=%s base_revision=%s analysis=%s\n",
                work$work_id, work$revision, work$base_revision %||% "-",
                work$payload$analysis %||% "?"))
    t_work <- now_s()

    out <- tryCatch(run_analysis(work), error = function(e) {
      cat(sprintf("[runner] analysis error: %s\n", conditionMessage(e)))
      list(results = list(error = TRUE, errorMessage = conditionMessage(e),
                          title = work$payload$analysis %||% "error"),
           status = "fatalError")
    })

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
      kind       = "analysis",
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
