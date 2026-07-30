# HANDOVER — Module Advertisement & Discovery

**Status:** Design complete, implementation pending. This document hands off the module advertisement/discovery feature to the next session.

**What's done (this session):**
- Provisioner feature complete and verified end-to-end (orchestrator spawns real Rscript runners on demand)
- Two-level libset scanning (base → libpath → module) using iterator combinators
- Renamed `scratch_root` → `orchestrator_dir_root` (env var `JASP_ORCH_DIR_ROOT`)
- All tests pass (14 unit tests + clippy + provision e2e + recompute e2e)

**What's NOT done:** Module advertisement and discovery (this handover).

---

## The Feature

The frontend needs to discover available modules and access their assets (QML forms, icons, help). Currently the frontend hardcodes module/analysis; there's no discovery mechanism.

**The model:**
- Orchestrator maintains a catalog of available modules: `{name, version, base_uri}` per module
- Orchestrator merges two sources:
  - **Libset scan:** orchestrator scans `JASP_ORCH_LIBSET` to find provisionable modules
  - **Runner advertisements:** runners advertise their modules in the `register` message
- Frontend queries orchestrator for the catalog (`list_modules` query)
- Orchestrator **pushes updates when the set actually changes** (not blindly on every runner event)
- Frontend lazy-loads assets from `base_uri` (Description.qml, qml/, icons/, help/)

**Lightweight metadata:** The wire carries only `{name, version, base_uri}`. The frontend parses `Description.qml` from the base URI to get ribbon metadata (title, icon, menu placement).

---

## Key Design Decisions

### Base URI
- **Source:** The directory found by scanning (jasp* + Description.qml)
- **Format:** `file://<that-directory>/` (e.g., `file:///home/sp42/jaspModuleTools/workdir/jaspTTests/jaspTTests/`)
- **Name/version:** Read from the module's DESCRIPTION file inside that directory
- **Scheme:** `file://` for desktop now; extend to `http://` for webapp later (scheme is the only thing that changes)

**Important:** The base URI is the directory we found by scanning. We don't construct it from `<libpath>/<module-name>` or worry about whether the directory name matches the module name. We just use the directory we found. Read name/version from DESCRIPTION inside.

### Push-on-actual-change
- Orchestrator pushes the module list **only when the set actually changes**
- If a runner disconnects but its modules are still advertised by other runners, **no update**
- Only push when the actual set of available modules changes (a module appears or disappears)

**Why:** Avoid unnecessary churn. A dev runner disconnecting shouldn't trigger an update if its modules are still available from other runners.

### Lightweight metadata
- Wire carries: `{name, version, base_uri}` — nothing else
- Frontend parses `Description.qml` from base URI for ribbon metadata (title, icon, menu)
- Keeps the wire protocol simple; frontend does the QML parsing

---

## Implementation Steps

### 1. Orchestrator: Extend `scan_libset` to emit `{name, version, base_uri}`
**File:** `orchestrator/src/provisioner.rs`

Currently `scan_libset` returns `(HashMap<String, PathBuf>, HashMap<PathBuf, Vec<String>>)` (module → libpath, libpath → modules).

**Change to:** Return `Vec<ModuleInfo>` where:
```rust
struct ModuleInfo {
    name: String,      // from DESCRIPTION Package field
    version: String,   // from DESCRIPTION Version field
    base_uri: String,  // file://<the-directory-we-found>/
}
```

**How:**
- Scan for jasp* directories with Description.qml (already done)
- For each found directory, read its DESCRIPTION file (use `read.dcf` equivalent in Rust, or parse manually)
- Extract `Package` and `Version` fields
- Construct `base_uri = format!("file://{}/", dir_path.display())`
- Return `Vec<ModuleInfo>`

### 2. Wire Protocol: Extend `Capability` and add messages
**File:** `orchestrator/src/messages.rs`

**Extend `Capability::Analysis`:**
```rust
Capability::Analysis {
    name: String,
    version: String,
    base_uri: String,  // NEW
}
```

**Add messages:**
```rust
// Frontend → orchestrator: request module catalog
Message::ListModules,

// Orchestrator → frontend: module catalog (response to ListModules AND push-on-change)
Message::Modules {
    modules: Vec<ModuleInfo>,  // {name, version, base_uri}
}
```

### 3. Runner: Advertise `base_uri`
**File:** `refactor_design/runner_jaspbase.R`

Currently the runner advertises:
```r
capabilities = list(list(kind = "analysis", name = MODULE, version = MODULE_VER))
```

**Change to:**
```r
capabilities = list(list(
  kind = "analysis",
  name = MODULE,
  version = MODULE_VER,
  base_uri = paste0("file://", file.path(LIBDIR, MODULE), "/")
))
```

The runner knows its `LIBDIR` (JASP_RUNNER_LIBDIR) and `MODULE`, so it can construct the base URI.

### 4. Orchestrator: Maintain catalog and push-on-actual-change
**File:** `orchestrator/src/main.rs`

**Maintain a catalog:**
```rust
struct Router {
    // ... existing fields ...
    catalog: Vec<ModuleInfo>,  // current catalog (libset scan ∪ runner ads)
}
```

**On startup:** Scan libset, initialize catalog.

**On runner register:**
1. Add runner's advertised modules to catalog (merge by {name, version})
2. Recompute catalog (libset scan ∪ all runner ads)
3. Compare old catalog vs new catalog
4. **Only push `Message::Modules` if they differ**

**On runner disconnect/evict:**
1. Remove that runner's modules from the catalog
2. Recompute catalog (libset scan ∪ remaining runner ads)
3. Compare old catalog vs new catalog
4. **Only push `Message::Modules` if they differ**

**On `ListModules` query:** Respond with `Message::Modules { modules: catalog }`.

**Dedup logic:** When multiple runners advertise the same `{name, version}`, keep one entry. A module is removed from the catalog only when the last runner advertising it disconnects AND it's not a libset module.

### 5. Frontend: Query and receive updates
**File:** `refactor_design/frontend_recompute.R` (or a new frontend)

**On startup:**
1. Send `Message::ListModules` to orchestrator
2. Receive `Message::Modules { modules }` response
3. Build ribbon/menu from the catalog (parse Description.qml from each base_uri)

**On push:**
1. Receive `Message::Modules { modules }` push
2. Rebuild ribbon/menu from the updated catalog

**Lazy-load assets:** For each module, load `Description.qml`, `qml/`, `icons/`, `help/` from `base_uri` as needed.

---

## Key Constraints & Gotchas

### Don't ask about directory names
The base URI is the directory found by scanning. We don't construct it from `<libpath>/<module-name>` or worry about whether the directory name matches the module name. We just use the directory we found. Read name/version from DESCRIPTION inside. **This is not an open question.**

### Push only on actual change
Don't push blindly on every runner event. Compare old catalog vs new catalog; only push if they differ. If a runner disconnects but its modules are still advertised by other runners, no update.

### Lightweight metadata
Wire carries only `{name, version, base_uri}`. Frontend parses Description.qml for ribbon metadata. Keep it simple.

### URI scheme
`file://` for desktop now. Extend to `http://` for webapp later. The scheme is the only thing that changes; the frontend logic stays the same (resolve qml/, icons/, etc. relative to base_uri).

---

## Files to Touch

1. **orchestrator/src/provisioner.rs** — extend `scan_libset` to emit `{name, version, base_uri}`
2. **orchestrator/src/messages.rs** — extend `Capability`, add `ListModules`/`Modules` messages
3. **orchestrator/src/main.rs** — maintain catalog, push-on-actual-change, respond to `ListModules`
4. **refactor_design/runner_jaspbase.R** — advertise `base_uri` in register message
5. **refactor_design/frontend_recompute.R** (or new frontend) — query catalog, receive updates, lazy-load assets

---

## Testing

**Unit tests:**
- Extend `scan_libset_finds_jasp_modules` to verify `{name, version, base_uri}` output
- Test catalog merge logic (libset ∪ runner ads, dedup by {name, version})
- Test push-on-actual-change (only push when catalog differs)

**E2E test:**
- Start orchestrator with libset
- Frontend queries `ListModules`, receives catalog
- Start a dev runner advertising a dev module → frontend receives push with updated catalog
- Disconnect dev runner → frontend receives push with updated catalog (dev module removed)
- Verify frontend can lazy-load assets from base_uri

---

## What's Already Done (Context)

- Provisioner feature complete (orchestrator spawns real Rscript runners on demand)
- Two-level libset scanning (base → libpath → module) using iterator combinators
- Renamed `scratch_root` → `orchestrator_dir_root` (env var `JASP_ORCH_DIR_ROOT`)
- All tests pass (14 unit tests + clippy + provision e2e + recompute e2e)
- Module advertisement/discovery design complete (this document)

---

## What NOT to Do

- **Don't ask about directory names** — the base URI is the directory found by scanning; we don't construct it from module names
- **Don't push blindly** — only push when the set actually changes
- **Don't add rich metadata to the wire** — keep it lightweight; frontend parses Description.qml

---

## Next Session: Implement This Feature

Follow the implementation steps above. Start with step 1 (extend `scan_libset`), work down the list. The foundation is already in place (two-level scanning, provisioner, orchestrator_dir_root). This is the last piece: module advertisement and discovery.

Good luck!

---

## Implementation outcome (done)

Feature implemented and verified. Deliberate deviations from the steps above, all in service of
the router's no-blocking-I/O invariant (§7 of orchestrator-router-design.md):

- **The libset is scanned ONCE, in `Broker::start`, before the router thread exists.** The router
  never re-scans; it merges a cached `libset_modules: Vec<ModuleInfo>` with live runner
  advertisements, pure in-memory, on every register/evict. `scan_libset` now returns a
  `LibsetScan { module_lib, lib_modules, modules }`, built once and shared between the
  provisioner (routing indexes) and the router (catalog seed). `RunnerProvisioner::new` takes
  the scan instead of the raw libset.
- **`Capability::Analysis.base_uri` is `Option<String>`**, not required: capabilities are
  routing keys, the URI is discovery metadata. A runner without asset-serving ability can still
  register; its advertisements route work but are omitted from the catalog. The R runner
  (`runner_jaspbase.R`) always sends it (`file://<LIBDIR>/<module>/`).
- **Deterministic dedup winner:** libset beats runners; among runners the earliest registration
  (lowest `seq`) wins. A redundant advertisement → no push; evicting the winner hands the module
  to the next-oldest advertiser → pushed (the asset source moved). Catalog is canonically
  sorted, so equality checks (and the wire) are order-stable.
- **Provisioner events reach the router directly.** `RunnerProvisioner` takes an
  `on_event: Box<dyn Fn(ProvEvent) + Send>` closure that `main.rs` wires onto the router's
  mailbox; the provisioner calls it from its own thread (an unbounded mpsc send never blocks,
  so this is safe). There used to be a dedicated relay thread + event channel here — removed as
  ceremony. The subsystem pattern is: a subsystem whose work blocks gets its own thread plus a
  mailbox for inbound requests, and reports outcomes by sending straight onto the router's
  mailbox itself.
- `ModuleInfo { name, version, base_uri }` lives in `messages.rs` (it is wire protocol).
  Name/version come from the module's DESCRIPTION (DCF); `base_uri` is the scanned directory
  itself, percent-encoded (`provisioner::file_uri`), trailing slash.
- New messages: `Message::ListModules` (unit variant) and `Message::Modules(ModulesMsg)` —
  reply (`reply_to` = query id, session-scoped) and push (unsolicited) share the one shape.

Verification: 4 new integration tests (query correlation + uri-less exclusion; push-on-actual-
change incl. takeover; libset stability across churn) + extended scan test + `file_uri` test;
clippy clean; `--schema` carries the new types. E2E: `refactor_design/run_modules_test.sh` +
`frontend_modules.R` + `probe_fake_runner.R` (a registration-only fake runner — no jaspBase —
which keeps the test free of analysis-engine environment issues) — PASS. Provision e2e
re-run — PASS (no regression from the scan/constructor refactor or the new wire field).
