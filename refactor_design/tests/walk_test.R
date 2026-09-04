#!/usr/bin/env Rscript
# Dev-time fixture checks for the options walk + alias codec
# (HANDOVER-runner-data-pruning.md §8 step 2). NOT a formal suite — the formal gates are
# the e2e (orchestrator/tests/dataset_e2e.rs) and GUI validation on the release lane.
#
# Usage: Rscript refactor_design/tests/walk_test.R   (from the repo root, or anywhere —
# the runner file is located relative to this script).
#
# The runner file itself cannot be source()d (it dials the orchestrator), so the needed
# top-level definitions are evaluated straight from its parse tree.

`%||%` <- function(x, y) if (is.null(x)) y else x

args <- commandArgs(trailingOnly = FALSE)
file_arg <- sub("^--file=", "", args[grep("^--file=", args)])
here <- if (length(file_arg)) dirname(normalizePath(file_arg[1L])) else "refactor_design/tests"
runner_file <- normalizePath(file.path(here, "..", "runner_jaspbase.R"))

want <- c("ALIAS_PREFIX", "ALIAS_TYPES", "alias_encode", "alias_decode",
          "alias_decode_names", "alias_decode_strict", "alias_decode_lax", "lax_decode_tree",
          ".substitute_free_occurrences", "rewrite_syntax", "walk_and_rewrite_options",
          "factor_from_numeric", "coerce_col", ".frame_from_cols")
exprs <- parse(runner_file)
for (e in exprs) {
  if (is.call(e) && length(e) >= 3L && as.character(e[[1L]]) %in% c("<-", "=")) {
    nm <- tryCatch(as.character(e[[2L]]), error = function(err) character(0))
    if (length(nm) == 1L && nm %in% want) eval(e, envir = globalenv())
  }
}
stopifnot(exists("walk_and_rewrite_options"), exists("alias_encode"))
# coerce_col's numeric->categorical branch flows through jasp_level_string (the
# system %.15g level-string format, runner-views-read-design.md D9) — sourced from
# the engine file, as the runner itself does.
source(normalizePath(file.path(here, "..", "..", "jaspRunner", "R", "data.R")))
stopifnot(exists("jasp_level_string"))

nfail <- 0L
check <- function(label, cond) {
  ok <- isTRUE(cond)
  cat(sprintf("%s %s\n", if (ok) "PASS" else "FAIL", label))
  if (!ok) nfail <<- nfail + 1L
}

# Fixture schema: plain, spaced, unicode, reserved-ish names.
schema <- c(contNormal = "scale", contBinom = "nominal",
            "reaction time" = "scale", "국어 점수" = "ordinal",
            "if" = "nominal", "T" = "scale")

# ── 1. alias codec: round-trip property + R-identifier validity ──────────────
for (nm in names(schema)) for (ty in ALIAS_TYPES) {
  a <- alias_encode(nm, ty)
  d <- alias_decode(a)
  check(sprintf("roundtrip '%s'/%s", nm, ty),
        !is.null(d) && identical(d$name, nm) && identical(d$type, ty) &&
        grepl("^jasp_enc_hex_[0-9a-f]+_(scale|ordinal|nominal)$", a) &&
        identical(make.names(a), a))
}
check("decode never throws / malformed -> NULL",
      is.null(alias_decode("jasp_enc_hex_zz_scale")) &&   # non-hex
      is.null(alias_decode("jasp_enc_hex_6")) &&          # odd hex
      is.null(alias_decode("jasp_enc_hex_61")) &&         # no type suffix
      is.null(alias_decode("enc_61_scale")) &&            # wrong prefix
      is.null(alias_decode("jasp_enc_61_scale")) &&       # old-format prefix: NOT this scheme
      is.null(alias_decode("jasp_enc_hex_61_bogus")) &&   # unknown type
      is.null(alias_decode(NA) ) && is.null(alias_decode(character(0))))

# ── 2. dual-role binding (the silent-failure case) ───────────────────────────
opts <- list(
  deps  = list(value = "contNormal", types = "scale"),
  group = list(value = "contNormal", types = "nominal"),
  .meta = list())
w <- walk_and_rewrite_options(opts, schema)
a_sc <- alias_encode("contNormal", "scale")
a_no <- alias_encode("contNormal", "nominal")
check("dual-role pairs", nrow(w$pairs) == 2L &&
      all(w$pairs$name == c("contNormal", "contNormal")) &&
      all(w$pairs$type == c("scale", "nominal")))
check("dual-role deps -> scale alias (scalar kept)", identical(w$options$deps, a_sc))
check("dual-role group -> nominal alias (scalar kept)", identical(w$options$group, a_no))
check("dual-role .types siblings", identical(w$options$deps.types, "scale") &&
      identical(w$options$group.types, "nominal"))
check(".meta dropped", is.null(w$options$.meta))

# ── 3. interactions (arrays of arrays, per-index types) ──────────────────────
opts <- list(
  modelTerms = list(value = list("contNormal", list("contNormal", "contBinom")),
                    types = list("scale", list("scale", "nominal"))),
  .meta = list())
w <- walk_and_rewrite_options(opts, schema)
mt <- w$options$modelTerms
a_b_no <- alias_encode("contBinom", "nominal")
check("interaction structure preserved",
      is.list(mt) && length(mt) == 2L && identical(mt[[1L]], a_sc) &&
      is.list(mt[[2L]]) && identical(mt[[2L]][[1L]], a_sc) &&
      identical(mt[[2L]][[2L]], a_b_no))
check("interaction pairs dedup", nrow(w$pairs) == 2L &&
      any(w$pairs$name == "contBinom" & w$pairs$type == "nominal"))
check("interaction .types sibling emitted", !is.null(w$options$modelTerms.types))

# ── 4. SEM model node (keepOriginalOption shape, §3.7 dispositions) ──────────
opts <- list(
  syntax = list(modelOriginal = "국어 점수 ~ `reaction time` + x_lat",
                model         = "국어 점수 ~ `reaction time` + x_lat",
                value   = list("국어 점수", "reaction time"),
                columns = list("국어 점수", "reaction time"),
                types   = list("ordinal", "scale"),
                optionKey = "value"),
  .meta = list())
w <- walk_and_rewrite_options(opts, schema)
syn <- w$options$syntax
a_rt <- alias_encode("reaction time", "scale")
a_ko <- alias_encode("국어 점수", "ordinal")
check("SEM node keeps all members",
      all(c("modelOriginal", "model", "columns", "optionKey", "types") %in% names(syn)))
check("SEM value aliased per parallel types", identical(syn$value, list(a_ko, a_rt)))
check("SEM columns aliased", identical(syn$columns, list(a_ko, a_rt)))
check("SEM model rewritten in place",
      identical(syn$model, sprintf("%s ~ `%s` + x_lat", a_ko, a_rt)))
check("SEM modelOriginal stays raw", identical(syn$modelOriginal, "국어 점수 ~ `reaction time` + x_lat"))

# ── 5. bare strings, shouldEncode meta, encodeThis ───────────────────────────
opts <- list(
  splitBy = "contBinom",                      # bare string: pair collected, NOT rewritten
  factors = list("contNormal", "contBinom"),  # shouldEncode: strict schema-alias rewrite
  levels  = list("l1", "contNormal"),         # no meta: untouched
  .meta = list(factors = list(shouldEncode = TRUE),
               encodeThis = list("contBinom", "notAColumn")))
w <- walk_and_rewrite_options(opts, schema)
check("bare string NOT rewritten (legacy parity)", identical(w$options$splitBy, "contBinom"))
check("bare string pair collected",
      any(w$pairs$name == "contBinom" & w$pairs$type == "nominal"))
check("shouldEncode strict rewrite",
      identical(w$options$factors,
                list(alias_encode("contNormal", "scale"),
                     alias_encode("contBinom", "nominal"))))
check("no-meta array untouched", identical(w$options$levels, list("l1", "contNormal")))
check("encodeThis unknown name ignored",
      !any(w$pairs$name == "notAColumn"))

# ── 6. rewrite_syntax engine (legacy encodeRScript parity) ───────────────────
sub_schema <- c(a = "scale", T = "scale", rep = "scale", x = "scale",
                "x y" = "scale", filter = "nominal")
amap <- vapply(names(sub_schema), function(nm) alias_encode(nm, sub_schema[[nm]]),
               character(1L), USE.NAMES = FALSE)
names(amap) <- names(sub_schema)
check("function-call guard: rep( stays", identical(rewrite_syntax("rep(1, 2)", amap), "rep(1, 2)"))
check("function-call guard across whitespace", identical(rewrite_syntax("rep (1)", amap), "rep (1)"))
check("bare use IS rewritten",
      identical(rewrite_syntax("rep + 1", amap), paste0(alias_encode("rep", "scale"), " + 1")))
check("string literals skipped", identical(rewrite_syntax('"a"', amap), '"a"'))
check("boundary: TRUE not shredded by column T",
      identical(rewrite_syntax("filter == TRUE", amap),
                paste0(alias_encode("filter", "nominal"), " == TRUE")))
check("name with spaces",
      identical(rewrite_syntax("lm(x y ~ a)", amap),
                sprintf("lm(%s ~ %s)", alias_encode("x y", "scale"), alias_encode("a", "scale"))))
check("longest name first",
      identical(rewrite_syntax("x y", amap), alias_encode("x y", "scale")))
check("inserted alias cannot rematch",
      identical(rewrite_syntax(rewrite_syntax("x y", amap), amap),
                alias_encode("x y", "scale")))

# ── 7. lax decode (schema-gated, single pass, boundary guard) ────────────────
.state <- new.env(parent = emptyenv())
.state$schemaNames <- names(schema)
txt <- paste0("mean of ", alias_encode("국어 점수", "ordinal"),
              " by ", alias_encode("contBinom", "nominal"))
check("lax decode", identical(alias_decode_lax(txt, schema), "mean of 국어 점수 by contBinom"))
check("lax decode schema gate blocks unknown",
      identical(alias_decode_lax("jasp_enc_hex_61_scale here", schema), "jasp_enc_hex_61_scale here"))
check("lax decode boundary guard",
      identical(alias_decode_lax(paste0("x", alias_encode("contBinom", "nominal")), schema),
                paste0("x", alias_encode("contBinom", "nominal"))))
check("lax decode trims trailing identifier chars",
      identical(alias_decode_lax(paste0(alias_encode("contBinom", "nominal"), "Extra"), schema),
                "contBinomExtra"))
tree <- list(schema = list(name = alias_encode("contBinom", "nominal")),
             title  = paste0("Descriptives of ", alias_encode("contNormal", "scale")))
dec <- lax_decode_tree(tree)
check("tree decode (values + nesting)",
      identical(dec$schema$name, "contBinom") && grepl("contNormal", dec$title, fixed = TRUE))
check("strict decode gated passthrough",
      identical(alias_decode_strict("jasp_enc_hex_61_scale", schema), "jasp_enc_hex_61_scale") &&
      identical(alias_decode_strict(alias_encode("contBinom", "nominal"), schema), "contBinom"))
# Regression (2026-08-15): JSON nulls arrive as R NULLs; `node[[i]] <- NULL` DELETES the
# element, shrinking the list mid-iteration -> subscript out of bounds. Every jaspBase table
# carries them (footnotes' cols/rows). The walk must skip them untouched.
null_tree <- list(results = list(
  ttest = list(footnotes = list(list(cols = NULL, myOrder = 0, rows = NULL,
                                       symbol = "<em>Note.</em>", text = "Welch's t-test.")),
               data = list(), status = "complete"),
  .meta = list()))
dec_null <- tryCatch(lax_decode_tree(null_tree), error = function(e) e)
check("lax tree decode survives JSON nulls",
      !inherits(dec_null, "error") &&
      is.null(dec_null$results$ttest$footnotes[[1L]]$cols) &&
      identical(dec_null$results$ttest$footnotes[[1L]]$text, "Welch's t-test.") &&
      length(dec_null$results$ttest$footnotes[[1L]]) == 5L)

# ── 8. coercion semantics (data.R:74-99 parity) ──────────────────────────────
suppressWarnings(check("factor->scale non-numeric levels -> NA",
      all(is.na(coerce_col(factor(c("a", "b", "a")), "scale")))))
check("factor->scale numeric levels give VALUES",
      identical(coerce_col(factor(c("10", "2")), "scale"), c(10, 2)))
check("numeric->nominal", is.factor(coerce_col(c(1.5, 2.5), "nominal")))
check("numeric->ordinal is ordered", is.ordered(coerce_col(c(1.5, 2.5), "ordinal")))
check("factor->ordinal gains ordered class", is.ordered(coerce_col(factor("a"), "ordinal")))

# ── 9. wire shape survives toJSON/fromJSON (auto_unbox) ──────────────────────
suppressMessages(library(jsonlite))
opts <- list(deps = list(value = "contNormal", types = "scale"),
             modelTerms = list(value = list("contNormal", list("contNormal", "contBinom")),
                               types = list("scale", list("scale", "nominal"))),
             .meta = list())
w <- walk_and_rewrite_options(opts, schema)
js <- toJSON(w$options, auto_unbox = TRUE, null = "null", digits = NA)
back <- fromJSON(js, simplifyVector = FALSE)
check("scalar stays scalar on the wire", is.character(back$deps) && length(back$deps) == 1L)
check("interaction array stays array on the wire",
      is.list(back$modelTerms) && is.list(back$modelTerms[[2L]]))
check(".types sibling on the wire", !is.null(back$deps.types) && !is.null(back$modelTerms.types))

# ── 10. encoding-torture CSV headers (test_data/encoding_torture.csv) ────────
# Headers exactly as they exist AFTER lane normalization (csv2arrow.rs:297-312):
# empty -> V{n}, pure-integer -> V{name}, duplicates -> _{pos}, else unchanged.
torture <- c("subject id", "reaction time", "국어 점수", "weight.kg", "T", "if",
             "3rd measurement", "treatment_group", "score", "jasp_enc_hex_61_scale",
             "V2020", "V12", "groep")
t_schema <- stats::setNames(rep("scale", length(torture)), torture)
for (nm in torture) {
  a <- alias_encode(nm, "scale")
  d <- alias_decode(a)
  check(sprintf("torture roundtrip '%s'", nm),
        !is.null(d) && identical(d$name, nm) && identical(d$type, "scale") &&
        identical(make.names(a), a))
}
# Collision probe: 'jasp_enc_hex_61_scale' is a REAL column here AND parses as the alias of
# a column "a" (hex 61). "a" is absent from the schema -> the gate must block substitution;
# the column's OWN alias round-trips distinctly.
probe <- "jasp_enc_hex_61_scale"
check("torture probe: lax decode gated", identical(alias_decode_lax(probe, t_schema), probe))
check("torture probe: strict decode gated", identical(alias_decode_strict(probe, t_schema), probe))
probe_alias <- alias_encode(probe, "scale")
check("torture probe alias distinct + round-trips",
      !identical(probe_alias, probe) && identical(alias_decode(probe_alias)$name, probe))
# The probe's genuine alias inside text still decodes (the gate only blocks impostors)
check("torture probe genuine alias decodes",
      identical(alias_decode_lax(paste0("mean: ", probe_alias), t_schema),
                paste0("mean: ", probe)))

# ── 11. REAL GUI wire shape (captured from a live TTestIndependentSamples run) ──
# Filled types on the bound slot, UNFILLED slot as {"types": [], "value": ""} — exactly as
# boundValues() emits it. The walk must pair the bound column and leave the empty slot alone.
gui_opts <- jsonlite::fromJSON('{
  ".meta": { "dependent": {"shouldEncode": true}, "group": {"shouldEncode": true} },
  "alternative": "twoSided",
  "dependent": { "types": ["scale"], "value": ["col_9"] },
  "group": { "types": [], "value": "" },
  "naAction": "perDependent", "student": false, "welch": true
}', simplifyVector = FALSE)
gui_schema <- stats::setNames(rep("scale", 30), paste0("col_", 0:29))
w <- walk_and_rewrite_options(gui_opts, gui_schema)
check("GUI shape: bound slot paired",
      nrow(w$pairs) == 1L && w$pairs$name == "col_9" && w$pairs$type == "scale")
check("GUI shape: dependent rewritten",
      identical(w$options$dependent, list(alias_encode("col_9", "scale"))))
check("GUI shape: empty slot untouched scalar", identical(w$options$group, ""))
check("GUI shape: .types sibling", identical(w$options$dependent.types, list("scale")))
check("GUI shape: .meta dropped", is.null(w$options$.meta))

# ── 12. LAZY ACCESSOR mode (the runner path on wide files) ──────────────────
# Membership via the idx env; types resolved ONLY for columns the options actually use.
lz_nms <- paste0("col_", 0:9)
lz_idx <- list2env(as.list(setNames(seq_along(lz_nms), lz_nms)), parent = emptyenv())
lz_calls <- 0L
lz_type_of <- function(nm) { lz_calls <<- lz_calls + 1L; "scale" }
lz_opts <- list(dependent = list(types = list(), value = list("col_3")),
                splitBy = "col_7")           # bare string -> pair via type_of
w <- walk_and_rewrite_options(lz_opts, list(names = lz_nms, idx = lz_idx, type_of = lz_type_of))
check("lazy: pairs resolved on demand",
      nrow(w$pairs) == 2L &&
      all(w$pairs$name == c("col_3", "col_7")) && all(w$pairs$type == "scale"))
check("lazy: types only resolved for USED columns",
      lz_calls == 2L)                        # one per used column — no full-width pass
check("lazy: unknown columns pass through untouched",
      identical(walk_and_rewrite_options(
        list(x = "not_a_column"),
        list(names = lz_nms, idx = lz_idx, type_of = lz_type_of))$options$x, "not_a_column"))

cat(sprintf("\n%s (%d failure%s)\n", if (nfail == 0L) "ALL PASS" else "FAILURES",
            nfail, if (nfail == 1L) "" else "s"))
if (nfail > 0L) quit(status = 1L)
