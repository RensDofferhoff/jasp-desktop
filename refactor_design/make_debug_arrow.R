#!/usr/bin/env Rscript
# Generates test_data/debug.arrow (Feather V2 + LZ4) from test_data/debug.csv — the Arrow fixture
# the orchestrator injects and the runner reads via jaspRunner/R/data.R::read_jasp_data(). The CSV
# stays the source of truth for the VALUES; this script fixes the §8.3 SCHEMA: `group` as a nominal
# Arrow dictionary, x/y/z as float64, each field carrying jasp:display_name metadata (z is
# integer-valued, so it also carries jasp:all_integer). Re-run whenever debug.csv changes.
#
# Usage (from the project root):
#   Rscript refactor_design/make_debug_arrow.R

suppressMessages(library(arrow))

csv <- "test_data/debug.csv"
out <- "test_data/debug.arrow"

df <- read.csv(csv, stringsAsFactors = FALSE)

nom <- dictionary(index_type = int32(), value_type = utf8(), ordered = FALSE)
sch <- schema(
  field("group", nom,       metadata = list("jasp:display_name" = "Group")),
  field("x",     float64(), metadata = list("jasp:display_name" = "X")),
  field("y",     float64(), metadata = list("jasp:display_name" = "Y")),
  field("z",     float64(), metadata = list("jasp:display_name" = "Z",
                                            "jasp:all_integer"  = "true"))
)

tbl <- Table$create(group = factor(df$group), x = df$x, y = df$y, z = df$z, schema = sch)
write_feather(tbl, out, compression = "lz4")

cat(sprintf("wrote %s (%d bytes, %d rows, %d cols)\n",
            out, file.info(out)$size, nrow(df), ncol(df)))
