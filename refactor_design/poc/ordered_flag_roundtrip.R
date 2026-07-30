#!/usr/bin/env Rscript
# PoC: Arrow ordered-flag round-trip through Feather V2 + LZ4
# Validates the load-bearing assumptions of neo-jasp.md §8.3
#
# Tests:
#   1. ordered=TRUE  (ordinal) survives write_feather -> read_feather
#   2. ordered=FALSE (nominal) survives
#   3. float64 (scale) survives
#   4. Dictionary VALUES (not labels) are preserved
#   5. Field metadata (jasp:labels, jasp:display_name, jasp:all_integer) survives
#   6. LZ4 compression is applied and doesn't corrupt anything
#   7. Read-as-scale returns values, not labels
#   8. Reordering = rewriting dictionary order, values stable

suppressMessages(library(arrow))

cat("=== Arrow PoC: ordered-flag round-trip ===\n")
cat("arrow:", as.character(packageVersion("arrow")), "\n\n")

tmpdir <- tempdir()
pass <- 0L; fail <- 0L
check <- function(desc, cond) {
  if (isTRUE(cond)) { cat("  PASS:", desc, "\n"); pass <<- pass + 1L }
  else              { cat("  FAIL:", desc, "\n"); fail <<- fail + 1L }
}

# ── Build a table with all three JASP column types (from R factors) ─────
# Ordinal: values "1","2","3" with labels low/med/high, ordered
cond <- ordered(c("1","3","2","1","3"), levels = c("1","2","3"))
# Nominal: values "10","20" with labels Control/Treatment, unordered
grp  <- factor(c("10","20","10","20","10"), levels = c("10","20"))
# Scale: float64, integer-valued
score <- c(7, 14, 21, 28, 35)

ord_type <- dictionary(index_type = int32(), value_type = utf8(), ordered = TRUE)
nom_type <- dictionary(index_type = int32(), value_type = utf8(), ordered = FALSE)

sch <- schema(
  field("condition", ord_type, metadata = list(
    "jasp:display_name" = "Condition",
    "jasp:labels"       = '{"1":"low","2":"med","3":"high"}')),
  field("group", nom_type, metadata = list(
    "jasp:display_name" = "Group",
    "jasp:labels"       = '{"10":"Control","20":"Treatment"}')),
  field("score", float64(), metadata = list(
    "jasp:display_name" = "Score (points)",
    "jasp:all_integer"  = "true"))
)

tbl <- Table$create(condition = cond, group = grp, score = score, schema = sch)

feather_path <- file.path(tmpdir, "test_dataset.arrow")

# ── Write Feather V2 with LZ4 ───────────────────────────────────────────
cat("--- Write Feather V2 (LZ4) ---\n")
write_feather(tbl, feather_path, compression = "lz4")
check("file written", file.info(feather_path)$size > 0)

# ── Read back ───────────────────────────────────────────────────────────
tbl2 <- read_feather(feather_path, as_data_frame = FALSE)
s2 <- tbl2$schema
f_cond <- s2$GetFieldByName("condition")
f_grp  <- s2$GetFieldByName("group")
f_score <- s2$GetFieldByName("score")

# ── Test 1: ordered flag survives ───────────────────────────────────────
cat("\n--- Test 1: ordered flag ---\n")
check("condition type is DictionaryType", inherits(f_cond$type, "DictionaryType"))
check("condition ordered == TRUE (ordinal)", isTRUE(f_cond$type$ordered))
check("group type is DictionaryType", inherits(f_grp$type, "DictionaryType"))
check("group ordered == FALSE (nominal)", identical(f_grp$type$ordered, FALSE))
check("score type is double/float64 (scale)", f_score$type$ToString() == "double")

# ── Test 2: dictionary values preserved ─────────────────────────────────
cat("\n--- Test 2: dictionary values ---\n")
cond_factor <- tbl2[["condition"]]$as_vector()  # R factor
grp_factor  <- tbl2[["group"]]$as_vector()

check("ordinal dict values (levels) = c('1','2','3')",
      identical(levels(cond_factor), c("1","2","3")))
check("nominal dict values (levels) = c('10','20')",
      identical(levels(grp_factor), c("10","20")))

# ── Test 3: indices preserved (1-based R codes) ─────────────────────────
cat("\n--- Test 3: indices ---\n")
check("ordinal codes (1-based) = c(1,3,2,1,3)",
      identical(as.integer(cond_factor), c(1L,3L,2L,1L,3L)))

# ── Test 4: field metadata survives ─────────────────────────────────────
cat("\n--- Test 4: field metadata ---\n")
cm <- f_cond$metadata; gm <- f_grp$metadata; sm <- f_score$metadata
check("jasp:display_name on condition", identical(unname(cm[["jasp:display_name"]]), "Condition"))
check("jasp:labels on condition",       identical(unname(cm[["jasp:labels"]]), '{"1":"low","2":"med","3":"high"}'))
check("jasp:display_name on group",     identical(unname(gm[["jasp:display_name"]]), "Group"))
check("jasp:labels on group",           identical(unname(gm[["jasp:labels"]]), '{"10":"Control","20":"Treatment"}'))
check("jasp:display_name on score",     identical(unname(sm[["jasp:display_name"]]), "Score (points)"))
check("jasp:all_integer on score",      identical(unname(sm[["jasp:all_integer"]]), "true"))

# ── Test 5: read-as-scale returns VALUES not labels ─────────────────────
cat("\n--- Test 5: read-as-scale ---\n")
scale_from_dict <- as.numeric(levels(cond_factor))[as.integer(cond_factor)]  # parse k levels once, index N rows
check("ordinal read-as-scale = c(1,3,2,1,3) (values, not codes/labels)",
      identical(scale_from_dict, c(1,3,2,1,3)))

wrong_codes  <- as.numeric(grp_factor)                                  # codes 1,2,...
right_values <- as.numeric(levels(grp_factor))[as.integer(grp_factor)]  # values 10,20,...
check("nominal: codes != values (the footgun is real)", !identical(wrong_codes, right_values))
check("nominal read-as-scale = c(10,20,10,20,10) (values)",
      identical(right_values, c(10,20,10,20,10)))

# ── Test 6: read-as-ordinal applies labels overlay ──────────────────────
cat("\n--- Test 6: read-as-ordinal with labels overlay ---\n")
labels_map <- jsonlite::fromJSON(cm[["jasp:labels"]])
label_or_value <- function(values, labels) {
  vapply(values, function(v) {
    l <- labels[[v]]
    if (!is.null(l) && nzchar(l)) unname(l) else v
  }, character(1))
}
got_levels <- unname(label_or_value(levels(cond_factor), labels_map))
cat("    actual display levels:", paste(got_levels, collapse = ", "), "\n")
check("ordinal display levels = c('low','med','high')",
      identical(got_levels, c("low","med","high")))
check("factor is ordered", is.ordered(cond_factor))

# ── Test 7: reorder = rewrite dictionary, values stable ─────────────────
cat("\n--- Test 7: reorder ordinal ---\n")
cond_reord <- factor(cond_factor, levels = c("3","2","1"), ordered = TRUE)
sch2 <- schema(
  field("condition", ord_type, metadata = list(
    "jasp:display_name" = "Condition",
    "jasp:labels"       = '{"1":"low","2":"med","3":"high"}')),
  field("group", nom_type, metadata = list(
    "jasp:display_name" = "Group",
    "jasp:labels"       = '{"10":"Control","20":"Treatment"}')),
  field("score", float64(), metadata = list(
    "jasp:display_name" = "Score (points)",
    "jasp:all_integer"  = "true"))
)
tbl3 <- Table$create(condition = cond_reord, group = grp, score = score, schema = sch2)
fp2 <- file.path(tmpdir, "test_reordered.arrow")
write_feather(tbl3, fp2, compression = "lz4")
tbl4 <- read_feather(fp2, as_data_frame = FALSE)
fc2 <- tbl4$schema$GetFieldByName("condition")
check("reordered: still ordered=TRUE", isTRUE(fc2$type$ordered))
check("reordered: dict values (levels) = c('3','2','1')",
      identical(levels(tbl4[["condition"]]$as_vector()), c("3","2","1")))
scale_after <- as.numeric(levels(cond_reord))[as.integer(cond_reord)]
check("reordered: as-scale STILL returns c(1,3,2,1,3) (values stable)",
      identical(scale_after, c(1,3,2,1,3)))
check("reordered: jasp:labels unchanged (value-keyed)",
      identical(unname(fc2$metadata[["jasp:labels"]]), '{"1":"low","2":"med","3":"high"}'))

# ── Test 8: LZ4 compression applied (realistic data) ─────────────────────
cat("\n--- Test 8: LZ4 compression (realistic 100k-row data) ---\n")
# NOTE: on the 5-row toy table above, LZ4 frame overhead EXCEEDS the savings
# (LZ4 2522 > raw 2314 bytes). That is expected — compression only pays off on
# real data. So we validate the design claim on a realistic low-cardinality table.
set.seed(1)
n <- 100000L
big_tbl <- Table$create(
  cond  = factor(sample(c("Control","Treatment","Placebo"), n, replace = TRUE)),
  group = factor(sample(c("A","B"), n, replace = TRUE)),
  val   = round(rnorm(n, 100, 15), 2)
)
fp_lz4 <- file.path(tmpdir, "big_lz4.arrow")
fp_raw <- file.path(tmpdir, "big_raw.arrow")
write_feather(big_tbl, fp_lz4, compression = "lz4")
write_feather(big_tbl, fp_raw, compression = "uncompressed")
sz_lz4 <- file.info(fp_lz4)$size
sz_raw <- file.info(fp_raw)$size
cat("  100k rows | LZ4:", sz_lz4, "bytes | uncompressed:", sz_raw,
    "bytes | ratio:", round(sz_raw / sz_lz4, 2), "x\n")
check("LZ4 smaller than uncompressed on realistic data", sz_lz4 < sz_raw)
# And confirm the compressed file still round-trips correctly
big_back <- read_feather(fp_lz4, as_data_frame = TRUE)
check("LZ4 file round-trips (100k rows)", nrow(big_back) == n)

# ── Test 9: as_data_frame=TRUE round-trip ───────────────────────────────
cat("\n--- Test 9: as_data_frame=TRUE ---\n")
df <- read_feather(feather_path, as_data_frame = TRUE)
check("df$condition is ordered factor", is.ordered(df$condition))
check("df$group is factor (not ordered)", is.factor(df$group) && !is.ordered(df$group))
check("df$score is numeric", is.numeric(df$score))
check("df$condition levels = values", identical(levels(df$condition), c("1","2","3")))

# ── Summary ─────────────────────────────────────────────────────────────
cat("\n=== RESULTS:", pass, "passed,", fail, "failed ===\n")
if (fail > 0) { cat("SOME TESTS FAILED — ordered-flag design needs revision.\n"); quit(status = 1) } else { cat("ALL TESTS PASSED — Arrow ordered-flag design is validated.\n") }
