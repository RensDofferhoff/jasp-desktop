# Runner view-read seam — the plan (analysis-views §7, migration step 3)

**Status:** design converged 2026-09-02 (planning session three); **slices A and B are DONE and
GREEN** — slice A: `refactor_design/tests/view_parity.R`, 145/145 checks over the real binaries
(the full matrix, the system level-string, the slice-B rename); slice B: the runner seam in
`runner_jaspbase.R`, gated by `refactor_design/test_v2_views_ttest_e2e.R` (four runs — fallback /
views x preload / on-demand — all byte-identical; the fallback path untouched, supersede e2e green).
Slice A also REVISED the contract (see D9/D10): coercion semantics are SYSTEM law (the worker's,
averaged to sane system-native behavior — `%.15g` grouping), not R's `as.character` folklore —
the R fallback (`jaspRunner/R/data.R`) CONFORMS (a one-`sprintf` `jasp_level_string` mirror +
matrix fixes) and dies at slice D with the rest of the R data engine. Locale is display-time
only (D10).
**D11 — the storage vocabulary flip — is IMPLEMENTED and GREEN (2026-09-04, the reshaped
slice C's data plane):** base-cache field names are the tokens `jasp_enc_hex_<hex(display)>`
(no type suffix), the wire `ColumnInfo.name` IS the token, blob fields are `<token>_<type>`
(= the R alias — the runner's rename is an identity), `token_map` is `{token: display}`
(display_name IS the decode), and the walk is dual-vocabulary for the migration window
(token-shaped AND classic display-named options both work; the t-test e2e pins their
equivalence with a bridge pair). Remaining for slice C: the C++ half — which now also
 carries **D12: the FRONTEND composes the cast-suffixed symbol at options emission**
 (decided 2026-09-07; see D12 below) — plus the GUI-lane memory gate.
This is the runner half of the views phase — the router/worker half is DONE (see
`orchestrator-v2-design.md` §8 + its §13 post-views addendum, and
`HANDOVER-orchestrator-v2.md`). It sits in the family of:

- `analysis-views-design.md` — the concept; **§7 (the R side's remaining duties) is this
  doc's brief**; AV2/AV4/AV5/AV6 pin the constraints;
- `neo-jasp.md` §8.2 (the encoding contract — frontend language-agnostic, runner
  translates) and §8.3 (the coercion matrix — now the WORKER's, see below);
- `HANDOVER-runner-data-pruning.md` — built the current runner pipeline (§3.2 the walk,
  §2.2 the alias codec); this doc retires its DATA half;
- `jaspbase-plugin.md` — the jaspBase bridge contract (natives §4.1 — unchanged here).

## 1. Thesis

> **The view IS the pruned, aliased, coerced frame.** Today the runner builds that
> frame in R (schema footer → options walk → cache reads → coercion → alias assembly).
> After this plan the runner *reads* it (`read_feather` + rename) and the whole
> data-correctness half of `runner_jaspbase.R` retires: `load_cols`, `read_jasp_data`
> dependency, `coerce_col` (eventually), the dual-role source-vector assembly, the
> lazy-`col_select` natives, and the schema-footer performance machinery.

What survives is exactly the language boundary (§8.2/AV5): the options walk +
rewrite (aliases are R-family policy), `rewrite_syntax`, and the results decode.
**Module-visible behavior does not change at all** — modules still see alias colnames
and alias-valued options; only WHO materialized the frame changes.

The simplification, quantified (of `runner_jaspbase.R`'s ~670 data-pipeline lines):

| Piece | Lines (≈) | Fate |
|---|---|---|
| options walk + meta rewrite (`walk_and_rewrite_options` L578-833) | 255 | **stays, thins** (language boundary; under D12 the suffix composition moves to the frontend — the walk keeps flatten/siblings/grammar/pairs and gains a pre-composed passthrough rung) |
| `rewrite_syntax` (L549-577) | 30 | **stays** |
| alias codec encode/decode + lax tree (L390-548) | 160 | **stays** (decode is the results authority) |
| `coerce_col` + `load_cols` (L834-870) | 40 | **dies** (slice D; `coerce_col` lingers as the migration ladder's last rung) |
| schema footer machinery (L316-360, lazy idx/type caches, the two wide-file perf fixes) | 45 | **dies** except pass-through naming |
| natives: decode→load→coerce dance (L875-956) | 140 | **shrinks to ~50** (frame-first select) |
| frame assembly for preload (run_analysis L1176-…) | ~25 | **dies** (the view frame IS it) |

## 2. The boundary decision (settled here, do not relitigate without new evidence)

**Option A — the frontend encodes options (legacy's shape): REJECTED.** It would shrink
the runner further (the walk moves to C++ where `Common/columnencoder.cpp` still lives),
but it breaks §8.2/AV5's rule that the frontend is language-agnostic: the alias is "a
property of the `analysis_r_classic_jaspbase` runner family" (pruning §2.1) — a future
Julia/Python runner mints its own symbols, and the frontend cannot know which family
will consume the work. The spec (`{name, as}` pairs) is the neutral vocabulary; the
symbol is not. The walk stays in R, once, where it is already survivorship-proven
(58/58 fixtures, `tests/walk_test.R`).

**Consequence:** the runner's walk keeps needing schema types as fallback — served from
the in-memory view frame's own fields (`<real>__<type>` splits), or the base footer for
pass-through. The schema machinery shrinks but does not vanish in the pass-through case.

**Addendum (2026-09-04, D11):** this boundary was revisited and *moved* — new evidence
(the frontend audit below) showed the "cannot know which family" argument was weaker
than assumed and the alias is less R-shaped than assumed once the type suffix is
excluded. The walk stays in R **for the migration window only**; see D11.

**Addendum (2026-09-07, D12):** the boundary moved again, narrowly. The walk itself
stays runner-side (flatten + `.types` siblings + grammar + decode are jaspBase's
contract, i.e. R-family business) — but **the suffix composition moves to the
frontend**. The decisive new fact: slice C's staple must implement `(token, type)`
pairing in C++ anyway (views do not work without it), so composing `token_type` at
the emission point — where `value[i]` and `types[i]` sit adjacent — is one line, and
the suffix is the GENERAL dual-role convention (already forced by the blob's field
names), not a jaspBase quirk. Family appeasement (the flat shape + siblings) no longer
shapes the wire vocabulary. See D12 for the full rationale and the accepted costs.

## 3. The seam — what the runner does per work (slice B)

`run_analysis` today (L1129-…): schema footer → walk → (preload) `load_cols` + assemble.
After slice B, with `view_refs` present on the work envelope (v2 injects it at dispatch;
**classic never does — its absence is the migration bridge**):

1. **Read** each ref's blob once: `read_feather(path)` (the AV5 artifact — factors and
   the `ordered` flag fall out natively; validated in the v2 e2e). Multi-dataset works:
   one ref per dataset input, ordered like `dataset_ids` (AV1); single-dataset keeps the
   scalar `dataset` argument (the scalar-vs-list decision stays parked, AV §13).
2. **Rename** fields to aliases: split at the LAST `__` → `(real name, type)` →
   `alias_encode(name, type)` (the stateless `jasp_enc_hex_<hex>_<type>` codec — hex is
   *derived*, identical to the codec's own; the metadata `token_map` is cross-checked
   and a mismatch is LOUD — it would mean a non-deterministic build, which AV8 forbids).
   `__base_row` is dropped from the module frame (analyses that report per-row results
   get it later, when a consumer is real).
3. **Pass-through refs** (`view_id: null`, path = the base cache): no eager read. Keep
   today's lazy path against the base (the footer schema machinery + `load_cols` serve
   it) — reading the whole base into R eagerly would regress memory exactly where
   pruning won it (terror_tall: 6 GB → 0.6 GB must not become 6 GB again).
4. **Natives go frame-first**:
   - `.readDatasetToEndNative(...)`: for each requested symbol `s` — if `s` is a frame
     column → serve it (the requested type is already materialized; the `as.*` args
     become no-ops for it). Miss ladder: (a) a sibling field of the same name coerced
     (`coerce_col(frame[[<name__othertype>]], as)` — covers "bound nominal, read
     numeric" = the values-parse semantics), (b) during migration only, the base-cache
     lazy path (today's code, whole). Post-migration (slice D) a miss is a LOUD error —
     it means the spec derivation missed a column, and silent coercion would hide a
     frontend bug (the audit's superset guarantee is what makes this safe to harden).
   - `all.columns=TRUE` → the pass-through frame (schema-typed aliases — today's
     behavior, now sourced from the pass-through ref's base, or the base path directly).
   - `.readDataSetRequestedNative` (preload=true) → the (renamed) view frame — the
     assembly loop dies.
   - `.readDataSetHeaderNative` → names/types split from the frame fields; zero callers
     today (audited), stays completeness-only.
5. **The walk runs BEFORE all of this** — options arrive in either vocabulary during
   the migration window (classic display names; post-D12 also pre-composed
   `token_type` values, which pass through untouched); the schema-type fallback
   consults the frame fields first.
6. **Results decode** (schema-gated lax tree): unchanged.

`.meta` keeps riding the wire (the walk needs `shouldEncode`/`isRCode`).

## 4. The slices (each leaves the tree green; classic keeps running)

### Slice A — the coercion-parity gate (gate 2) — DONE, GREEN, contract revised (D9)

`refactor_design/tests/view_parity.R`: spawn v2 + worker (fake analysis-runner socket —
the dispatched work hands back `view_refs` + `dataset_paths` directly), open
`test_data/debug.csv` + `test_data/encoding_torture.csv` + a generated numeric-torture
CSV (nulls override so a literal NaN cell parses as a real f64 NaN), staple a battery of
specs (the full §8.3 matrix × the torture columns: dict→scale/nominal/ordinal, f64→all
three, label overlay + level remap via edit-lane schema_change, nulls, NaN, dual-role
two-types-one-column), let the fill build the blob, then compare
`read_jasp_data(base, spec)` (the migration-era R fallback, now CONFORMING per D9)
against `read_feather(blob)` + rename: `identical()` frames — types, values, NAs,
factor levels AND level order. Plus a dedicated level-string table (boundary
magnitudes, round-trip pins: `1e14`→`"1e+14"`, `1234567890123456` exact, `2^60` →
shortest-round-trip padded, `0.1+0.2` honest, `1/3`, …) judged against the R mirror
`jasp_level_string` (see D9 —
the contract is now the SYSTEM level-string format, not R's `as.character`; the
migration-era fallback conforms via the mirror and dies at slice D). **This gate
blocked three real divergences before going green (ordered-leak on nominal casts,
NaN-as-level, the as.character escalation rabbit-hole) — those motivated D9.**
It blocks slice B's flip and is a permanent regression test.

### Slice B — the runner seam (this doc's §3) — **DONE, GREEN**

Runner-only; production frontend unchanged. Implemented in `runner_jaspbase.R`:
`view_refs` present → `view_frame_from_ref` reads each materialized blob once (rename
via the stateless codec, token_map cross-checked LOUD per D2, `__base_row` dropped per
D8), pass-through refs never eager-read (D3), the walk's type fallback consults frame
fields first (D6), preload = the frame directly (D5) with a lazy-append safety rung
for hand-stapled spec misses, and the natives are FRAME-FIRST with the miss ladder
(frame hit → sibling-field coerce → migration lazy rung). `coerce_col`'s
numeric→categorical branch now flows through `factor_from_numeric` (jasp_level_string
— D9; never bare `factor()`). ABSENT `view_refs` → today's path byte-identical.

Validated by `refactor_design/test_v2_views_ttest_e2e.R` over the REAL stack (v2 +
provisioned worker + the real jaspBase runner, one libset runner via JASP_ORCH_LIBSET):
a dual-role t-test on encoding_torture.csv (dependent `score` dict→scale, grouping `T`
dict→nominal, staple also casts `T` as scale — the dual-role superset) run as FOUR
analyses — fallback-vs-views at preloadData TRUE (the view frame IS the preload
frame) and at FALSE (the on-demand frame-first natives) — **results byte-identical in
both comparisons**; the runner log pins that the seam served (view read + preload
frame (view), 0.003 s vs the fallback's 0.123 s assembly). The supersede e2e stays
green (the fallback path untouched). NOTE: a full jaspAnova interaction e2e needs the
GUI's complete default options object (jaspBase fills no defaults for absent keys) —
that rides with slice C, where the real frontend's options arrive complete; the walk's
interaction-array shape stays pinned by walk_test.R §3.

### Slice C — the frontend derives and staples specs (AV2; the memory win) — RESHAPED by D11 + D12

**The D11 data plane is DONE (2026-09-04):** ingest mints the tokens, the wire renders
`name` = token / `display_name` = decode, the worker resolves by token, the runner is
token-native with the walk kept for classic-shaped options. **D12 (2026-09-07,
decided, NOT yet implemented)** adds the frontend composition. The C++ half is now:

1. **Emission composition (D12):** the bound controls emit `value` already carrying
   its slot's cast — `token_type` — composed from the `types` entry sitting next to
   it at the emission point (one base emission site preferred over per-control
   edits). Dispositions preserved by construction: bare/runtime-cast strings (e.g.
   `splitBy`) stay BARE — they are distinct control shapes the emission point can
   distinguish; `modelOriginal`/model text is untouched (grammar rewriting stays
   runner-side); unfilled slots stay empty. A declared `types` entry wins; a missing
   one leaves the value bare (the runner's schema-fallback rung serves it).
2. **Staple from the same pairs:** `createWorkJson` collects + dedupes the
   `(token, type)` pairs it now composes anyway and staples `work["views"] =
   [{dataset_id, columns: [{name, as}]}]`. The pairing logic is written ONCE, in C++,
   for both the emission and the staple. Keep stapling **supersets** (AV §12).
3. **Runner passthrough rung (small, R):** `rewrite_name` gains a pre-composed rung —
   an arriving `token_type` that decodes to a schema column passes through UNTOUCHED
   and its `(display, type)` joins the pairs (~5 lines). Classic display-named
   options keep the full encode path until slice D — the walk is bilingual either way.
4. AV3 `fullDataset` flag in `Description.qml` (next to `preloadData`, the 22-module
   precedent): `true` → staple `{all: true}` (pass-through) instead of derived pairs.
   Registry lint (fail `all.columns=TRUE`/`.allColumnNamesDataset()` without the flag)
   follows the audit's 3 offender modules (jaspSem conditional, jaspMetaAnalysis ×2,
   jaspBain ×2 — 5 sites).
5. **Gate: the real GUI lane.** The terror_tall memory story re-measured (the §8.6
   recipes of the pruning handover); PLUS the D12/D11 GUI checklist: plot labels show
   display names (the `decodeplot` → `decodeColNames` → runner-natives chain is
   verified in-code; run at least one plot-producing analysis), the rlang/jags
   model-editor extraction matches `displayName` post-D11 (pre-D11 name==display made
   this invisible — verify `boundcontrolrlangtextarea`/`boundcontroljagstextarea`),
   and the e2e gains a pre-suffixed-options run pinning C++-composed symbols ≡
   R-composed (byte-identical results, same harness as the existing bridge pair).

### Slice D — retirement (after soak + the classic freeze, §12 step 7)

Delete the fallback rung: `load_cols`, the `read_jasp_data` dependency — the runner
stops sourcing `jaspRunner/R/data.R` (L52) entirely; the engine lives on in the parity
harness — `coerce_col` (unless the sibling-coerce rung stays — decide by soak
evidence), the schema-footer caches, `.state$cols`. A view miss becomes the loud
error. Estimated runner data-pipeline: ~670 → ~500 lines, with the survivors all
language-boundary (walk/codec/decode) rather than data-correctness. The R coercion
engine — §8.3's benchmarked semantics — then exists in exactly ONE place (the worker),
which was AV4's whole point.

## 5. Decisions (this doc pins them)

| # | Decision | Why |
|---|---|---|
| D1 | Runner reads views; encoding stays runner-side — the *runner-side encoding* half is SUPERSEDED by D11 (2026-09-04): encoding moves to ingest; the runner still reads views | §8.2/AV5 law as understood at convergence; D11's audit revisited it with new evidence |
| D2 | Field→alias rename derives hex (stateless codec), cross-checks `token_map`, mismatch is loud | The codec is the protocol (pruning §2.1); the map is a determinism tripwire |
| D3 | Pass-through refs never eager-read | the terror_tall memory regression; the lazy base path already exists |
| D4 | Natives are frame-first with a miss ladder; the ladder's last rung retires in slice D | correctness during migration, loudness after (a miss = a derivation bug) |
| D5 | `preloadData` true/false keeps its jaspBase meaning but unifies on ONE frame | the view is already in RAM; the flag only chooses whether `dataset` is passed |
| D6 | The walk's type-fallback consults frame fields before the schema footer | kills the footer read for typed views; footer remains for pass-through only |
| D7 | Spec derivation = the revived C++ walk's `colsPlusTypes`, superset-tolerant | AV2's "formalizing the preload pipeline that already exists"; one catalog, ported back where it started |
| D8 | `__base_row` dropped from module frames for now | no consumer yet; revisit with per-row-result analyses (outliers/influence) |
| D9 | **Coercion semantics are SYSTEM law, not R's** — the worker's matrix is the contract: nominal is always unordered, non-finite is null (never a "NaN" category), and f64→nominal level strings use the system `level_string` = plain **`%.15g`** (15 significant digits, trailing zeros trimmed, the C `%g` range rule: scientific iff exponent < −4 or ≥ 15). The grouping is deliberately lossy at the fringe — values agreeing at 15 significant digits are ONE category (cast code dedupes on the rendered string; R/classic have always grouped this way). The migration-era R fallback (`read_jasp_data`) CONFORMS via the `jasp_level_string` mirror (one `sprintf`) + matrix fixes, and dies at slice D | Multi-runtime law (§8.2/AV5): a future Julia/Python runner must not inherit R cosmetics; legacy JASP cast in C++ anyway (the R engine was itself an interim approximation, so R-exactness was never the real contract); chasing bit-exact round-trip labels (`0.30000000000000004`) buys nothing real — fringe collisions are invisible at display precision and arguably the correct grouping semantics. The parity gate (slice A) pins worker vs fallback, not worker vs `as.character` |
| D10 | **Locale is a display-time concern, never baked into data.** Level strings (and every identity key) stay canonical ASCII `.`-form in blobs/results/syntax; localization happens at render time for the *viewer*. The grid already does this (frontend re-requests TSV with its own `ViewRender` params). If results-table label localization is ever wanted: the cheap mechanism is a canonical-string check at render (parse → re-render at %.15g → equal? then localize); the proper one is a format hint on jaspResults columns. Neither built; noted for the future | Baked locale freezes the *runner's* locale into shared data (wrong for every other viewer); it would enter the view_id hash (locale flip → full cache invalidation) and break cross-locale string identity (saved filters, generated R syntax). Precision is semantics (which values group); separators are cosmetics (how a value is drawn) — semantics live in data, cosmetics in the viewer |
| D11 | **THE STORAGE VOCABULARY FLIP (converged 2026-09-04, IMPLEMENTED same day — the data plane is green).** The base cache's field names became the encoded canonical identity: `jasp_enc_hex_<hex(utf8 display name)>`, **no type suffix** (types stay schema metadata). **`display_name` IS the decode**: the wire `ColumnInfo.name` IS the token (display_name carries the real name); field metadata already carried it. The R alias = storage name + `_` + cast type (the worker appends at blob build — blob fields ARE the aliases, so the runner's rename is an identity). Everything analysis-side speaks tokens; the walk stays alive for the migration window only (classic-shaped display-named options still arrive — the t-test e2e's bridge pair pins their byte-equivalence). The one irreducible R piece (`rewrite_syntax` for user-typed R code) stays runner-side. Implementation: `csv2arrow` mints (`TOKEN_PREFIX`/`token_of`/`jasp_field(display)`), the edit engine uniquifies in DISPLAY space + matches both vocabularies (token first), the worker resolves token-first (display bridge), `read_jasp_data` resolves either form, the runner's schema accessors take either form (`schema_display_name`/`token_of`) with displays extracted lazily (work starts stay O(1) in schema reads). `token_map` = `{token: display_name}`. **Audit evidence** (2026-09-04, this repo): 40 QML `columnName` reads are all opaque-token binding/logic (FilterConstructor, type lookups); display flows through `displayName` (gridmodel.cpp L303/337/433, columnmodel.cpp L139+); the whole Desktop tree has 7 encode/decode calls, all in the data layer; results decode already exists (analysis.cpp L591) | One encoder in the system (ingest) instead of three name-mangling schemes (real / `real__type` / hex); the dual-walk drift risk is eliminated rather than ladder-bounded; slice C becomes trivial (the spec = the bound tokens — no deriver heuristic); storage vocabulary is family-neutral without the type suffix (hex-of-UTF-8 is language-free); renames/retypes cost the same as today (edits rewrite files anyway). Costs accepted: artifacts are hex to humans (decode helper for logs), migration is a coordinated flip across lane+wire+worker+runner+fixtures |
| D12 | **FRONTEND COMPOSES THE SUFFIX (decided 2026-09-07, NOT yet implemented — rides with slice C).** The bound controls emit option values already cast-suffixed: `value = token + "_" + type`, composed at the emission point from the adjacent `types` entry. The runner walk gains a pre-composed passthrough rung (decode → pair → leave untouched) and KEEPS: the flatten + `.types`-sibling reshape, SEM/JAGS/`isRCode` grammar rewrites, results decode, `all.columns` schema-type aliasing, and the classic display-name encode path until slice D. **This consciously overrides the suffix half of §2's Option-A rejection / pruning §2.1's "symbols are runner-family property"** — the owner's call, with new evidence those decisions predated. | The new fact: slice C's staple must implement `(token, type)` pairing in C++ regardless (views do not work without it), so composition at the emission point — where value and types sit adjacent — is one line, written once for both the emission and the staple. And the suffix is NOT a jaspBase quirk (that's the flat shape + siblings, which stay runner-side): it is the general dual-role convention, already forced system-wide by the blob's field names; family appeasement should not shape the general wire vocabulary. Costs accepted (known, reversible): two spelling paths during the migration window (C++ composes new options, R composes classic-shaped — the e2e bridge runs pin identical symbols); the emission's value shape changes frontend-wide (one base site preferred); the runner keeps its traversal regardless. Fallback if it bites: the runner's encode half stays alive through slice D anyway — reverting is deleting the C++ composition line and the passthrough rung |

## 6. Considered and rejected (do not relitigate)

| Idea | Why it died |
|---|---|
| Frontend encodes options (Option A, legacy shape) | breaks frontend language-agnosticity (§8.2); symbols are runner-family property; a second runtime would strand it — **narrowly superseded by D12 (2026-09-07) for the SUFFIX only**: the staple forced `(token, type)` pairing into C++ anyway and the suffix is the general dual-role convention, not R property. The rest of Option A (the reshape, the grammar) stays dead |
| Runner eagerly reads pass-through views | regresses the pruning memory win exactly where it was won |
| Keep silent coercion on view miss forever | hides spec-derivation bugs; the superset guarantee makes loudness safe |
| Move `rewrite_syntax`/encodeRScript to the frontend | same family as Option A — it is language work, and the R engine is built and pinned (**still dead under D12** — grammar is irreducibly R-side) |
| Rename via the metadata map as authority (not the codec) | makes the runner depend on blob provenance for a pure function; the map's job is the tripwire + results decode |
| Views replace the `dataset_paths` bridge now | classic never injects `view_refs`; the bridge dies with classic (slice D) |
| Move the whole walk (flatten + siblings) into the frontend | that is Option A entire, not the narrow D12 carve-out: it changes the options wire format wholesale (flat shape from every bound control) to save a traversal the runner keeps anyway for grammar/decode/classic — the arithmetic never worked. Revisit only post-slice-D, possibly as jaspBase natively accepting the nested shape (backlog note) |

## 7. File map (where each change lands)

| What | Where |
|---|---|
| Parity harness (slice A — DONE) | `refactor_design/tests/view_parity.R` (new; sources the conforming fallback from `jaspRunner/R/data.R`) |
| System level-string + matrix conformance (slice A, D9) | `orchestrator/crates/data_runner/src/analysisview.rs` (`level_string`, CatF64/CatDict), `jaspRunner/R/data.R` (`jasp_level_string` mirror, nominal-unordered, non-finite→NA) |
| Seam: read+rename+frame-first natives (slice B — DONE) | `refactor_design/runner_jaspbase.R` (view_frame_from_ref + the seam in run_analysis + frame-first natives + factor_from_numeric) |
| e2e drivers with hand-stapled specs | `refactor_design/test_v2_views_ttest_e2e.R` (DONE — the four-run dual-role gate + the D11 bridge pair) |
| **D11 storage vocabulary flip (DONE — the data plane)** | `csv2arrow.rs` (`TOKEN_PREFIX`/`token_of`/`jasp_field(display)`/ColumnInfo), `dataedit.rs` (display-space pools + both-forms matching), `analysisview.rs` (token-first resolve, `<token>_<type>` fields, `{token: display}` map), `arrowview.rs` (token-first resolve), `wire/messages.rs` (docs), `jaspRunner/R/data.R` (both-forms resolve), `runner_jaspbase.R` (`token_of`/`token_decode`, `schema_display_name`/`schema_displays`, dual-vocabulary walk, identity view read) |
| Spec staple + emission composition + fullDataset flag (slice C — the C++ half + the D12 R rung, next) | `QMLComponents/boundcontrols/*` (the boundValues emission base — compose `token_type` where `types` rides; keep bare/runtime-cast strings bare), `Desktop/analysis/analysis.cpp` `createWorkJson` (~L339: the same pairs → `work["views"]`), `refactor_design/runner_jaspbase.R` (`rewrite_name` pre-composed passthrough rung, ~5 lines), `QMLComponents/modules/analysisentry.*` (fullDataset flag), modules' `Description.qml` (3 offenders) |
| Retirement (slice D) | runner deletions (§4 slice D list) |

## 8. Validation ladder (cumulative)

1. `Rscript refactor_design/tests/view_parity.R` — the gate (slice A, blocks B). **GREEN:
   145/145 (2026-09-04, re-run over the D11 token vocabulary: blob fields are
   `<token>_<type>`, the token_map decodes {token: display}, and the parity matrix is
   unchanged).**
2. `walk_test.R` still green — now with §13, the D11 token fixtures (token-shaped
   options ≡ display-shaped options; the torture display priority; tokens never
   rewritten inside R code). **GREEN (ALL PASS).**
3. `test_v2_views_ttest_e2e.R`: dual-role t-test, views vs fallback, identical
   jaspResults JSON (slice B) — post-D11 the options and staple speak TOKENS, plus a
   bridge pair re-running classic display-named options (both paths). **GREEN: 6 runs
   (fallback/views × preload/on-demand, token options — all byte-identical — plus the
   2-run bridge pair, also identical); supersede e2e green.**
4. Full orchestrator suite (classic 41+1i + classic e2e 7 · data_runner 94+1i · v2 19 ·
   v2 views e2e 1 · wire 5 = 167) + clippy clean, every slice. **GREEN (2026-09-04).**
5. GUI lane (slice C's C++ half + D12): the pruning handover's §8.6 recipes re-run —
   terror_tall RSS, preload pair, jaspSem `all.columns`, the special-character
   dual-role dataset, revision re-run hygiene — PLUS: plot labels show display names
   (one plot-producing analysis; the `decodeplot` → `decodeColNames` → runner-natives
   chain is verified in-code), the rlang/jags model-editor extraction matches
   `displayName` post-D11, and a pre-suffixed-options e2e run pinning C++-composed ≡
   R-composed symbols.

*The frame was always the analysis's; now it arrives that way — named in one tongue.* 📐
