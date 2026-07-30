#!/usr/bin/env Rscript
# Smoke test for jaspRunner/R/data.R — the single unified read path.
# Builds a small dataset (ordinal w/ labels, nominal w/o labels, scale, boolean filter)
# and checks read_jasp_data returns the right types/values, honours column order, the
# guarded relabel (value==label untouched), scale-read-of-categorical, and filtering.

suppressMessages(library(arrow))
source("jaspRunner/R/data.R")

cat("=== smoke test: jaspRunner/R/data.R ===\n")
pass <- 0L; fail <- 0L
ck <- function(d, c) { if (isTRUE(c)) { cat("  PASS:", d, "\n"); pass <<- pass + 1L } else { cat("  FAIL:", d, "\n"); fail <<- fail + 1L } }

dir <- tempdir()
cond  <- ordered(c("1","3","2","1","3","2"), levels = c("1","2","3"))     # ordinal, labelled
grp   <- factor(c("10","20","10","20","10","20"), levels = c("10","20"))   # nominal, NO labels (value==label)
score <- c(7, 14, 21, 28, 35, 42)                                          # scale
flag  <- c(TRUE, FALSE, TRUE, TRUE, FALSE, TRUE)                          # filter
ord_type <- dictionary(index_type = int32(), value_type = utf8(), ordered = TRUE)
nom_type <- dictionary(index_type = int32(), value_type = utf8(), ordered = FALSE)
sch <- schema(
  field("cond",  ord_type, metadata = list("jasp:display_name" = "Condition",
        "jasp:labels" = '{"1":"low","2":"med","3":"high"}')),
  field("grp",   nom_type, metadata = list("jasp:display_name" = "Group")),   # no jasp:labels
  field("score", float64(), metadata = list("jasp:display_name" = "Score")),
  field("flag",  boolean(), metadata = list("jasp:display_name" = "Flag"))
)
tbl <- Table$create(cond = cond, grp = grp, score = score, flag = flag, schema = sch)
path <- file.path(dir, "smoke.arrow"); write_feather(tbl, path, compression = "lz4")

# spec deliberately in a NON-file order to test out[sel] reordering
spec <- list(list(name = "score", as = "scale"),
             list(name = "cond",  as = "ordinal"),
             list(name = "grp",   as = "nominal"))
df <- read_jasp_data(path, spec)

ck("returns a data.frame", is.data.frame(df))
ck("column order == spec order (reordered)", identical(names(df), c("score", "cond", "grp")))
ck("cond is ordered factor", is.ordered(df$cond))
ck("cond levels relabelled -> low/med/high", identical(levels(df$cond), c("low", "med", "high")))
ck("cond values correct", identical(as.character(df$cond), c("low","high","med","low","high","med")))
ck("grp is plain factor (not ordered)", is.factor(df$grp) && !is.ordered(df$grp))
ck("grp levels are VALUES (guard skipped relabel)", identical(levels(df$grp), c("10", "20")))
ck("score is numeric", is.numeric(df$score))
ck("score values correct", identical(df$score, score))

dfs <- read_jasp_data(path, list(list(name = "cond", as = "scale")))
ck("scale-read of ordinal returns numeric VALUES", identical(dfs$cond, c(1, 3, 2, 1, 3, 2)))

dff <- read_jasp_data(path, spec, filters = "flag")
ck("filter subsets to flag==TRUE (4 rows)", nrow(dff) == 4L)
ck("filter keeps the right score rows", identical(dff$score, score[flag]))

cat(sprintf("\n=== %d passed, %d failed ===\n", pass, fail))
if (fail > 0) quit(status = 1) else cat("data.R OK\n")
