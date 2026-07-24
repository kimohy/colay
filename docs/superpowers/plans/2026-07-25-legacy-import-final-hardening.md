# Legacy Import Final Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Do not delegate this plan; the parent explicitly assigned the final round to this worker.

**Goal:** Eliminate source path ABA races, recover importer scratch after crashes without touching active attempts, and make the prior review documentation match verified behavior.

**Architecture:** Introduce a safe handle-owning `SourceOpenGuard` for all repository-state source files and a lock-owning `LegacyImportScratch` rooted beside the selected global database. Route SQLite, JSONL, artifact, migration, and rewrite work through those owners, then align the approved-graph authority fixture and evidence claims.

**Tech Stack:** Rust 2024/stable 1.95, `same-file`, `fs2`, `rusqlite`, SHA-256, platform-specific safe `OpenOptionsExt`.

## Global constraints

- Add no unsafe code and do not change the stable Rust 1.95 toolchain.
- Read source database, JSONL, and artifacts only through retained guarded handles.
- On Windows omit delete sharing for retained parent and file handles.
- Delete scratch only below the exact private importer scratch root and only after obtaining the attempt owner lock.
- Preserve source and published evidence, append-only audit semantics, schemas, and transaction/approval ordering.

---

### Task 1: Add source identity ABA regressions and guarded opens

**Files:**
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`
- Modify: `crates/orchestrator-state/Cargo.toml`
- Create: `crates/orchestrator-state/src/source_guard.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Modify: `crates/orchestrator-state/src/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/artifacts.rs`

**Interfaces:**
- Produces: `SourceOpenGuard::open(root: &Path, path: &Path) -> StateResult<Self>`.
- Produces: `SourceOpenGuard::{reader,revalidate,sqlite_path}` over the retained final/parent handles.
- Produces: `GuardedSourceConnection` that owns a read-only `Connection` and guard.
- Updates: artifact staging to accept verified bytes instead of reopening a source path.

- [ ] **Step 1: Add deterministic ABA regression**

Add a private `cfg(test)` two-phase hook around final handle open. In a high-level importer unit fixture, swap a nested source parent to an alternate containing matching-looking evidence after the pre-open identity, then restore the original before the post-open comparison. Assert inspection/import refuses and byte/hash/count snapshots of source and target are unchanged.

- [ ] **Step 2: Run the ABA regression RED**

Run: `cargo test -p orchestrator-state source_parent_aba -- --nocapture`

Expected: the old canonical path checks accept the restored path or no hook boundary exists.

- [ ] **Step 3: Add the narrow safe identity dependency and guard**

Add workspace `same-file = "1"` and consume it only from `orchestrator-state`. Implement component-wise pre/open/post `same_file::Handle` equality. Retain parent and file handles; on Windows set `share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)` and `FILE_FLAG_BACKUP_SEMANTICS` for directories. Reject non-directory parents, non-regular final files, links/reparse components, identity mismatches, and revalidation mismatches.

- [ ] **Step 4: Route source consumers through the guard**

Replace `open_source_read_only`, `read_contained_source`, and staging's path reopen. Keep the SQLite guard alive through all source connection operations and revalidate before/after status, integrity, daemon rejection, identity calculation, backup, document/JSONL/artifact validation, and snapshot completion. On Linux use `/proc/self/fd/<parent-fd>/<basename>` for SQLite; otherwise use the guarded path. JSONL/artifacts read and hash through the retained file.

- [ ] **Step 5: Run ABA and focused source tests GREEN**

Run:

```text
cargo test -p orchestrator-state source_parent_aba -- --nocapture
cargo test -p orchestrator-state --test legacy_import nested_link -- --nocapture
cargo test -p orchestrator-state --test legacy_import -- --nocapture
```

Expected: all selected tests pass.

### Task 2: Add recoverable, lock-scoped importer scratch

**Files:**
- Create: `crates/orchestrator-state/src/import_scratch.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Modify: `crates/orchestrator-state/src/legacy_import.rs`
- Modify: `crates/orchestrator-state/tests/legacy_import.rs`

**Interfaces:**
- Produces: `LegacyImportScratch::acquire(paths: &GlobalStatePaths, fingerprint: &str) -> StateResult<Self>`.
- Produces: `LegacyImportScratch::{path,finish}` while retaining fingerprint and owner locks.
- Updates: `LegacyImporter::inspect(source, paths)` so caller-selected global state owns inspection scratch.
- Retains: `prepare_rewrite_scratch(validated: &Path, scratch: &Path) -> StateResult<()>`.

- [ ] **Step 1: Add crash-residue and sidecar regression**

Create a valid stale attempt below the exact fingerprint scratch directory containing an unlocked `owner.lock`, database files, and `-wal`/`-shm` sidecars. Retry inspection/apply and assert the stale attempt is removed, a clean attempt succeeds, and no residue survives normal completion.

- [ ] **Step 2: Add active-lock preservation regression**

Hold an attempt `owner.lock` from the test, run an import attempt for the same fingerprint, and assert the active directory and sidecars remain byte-for-byte while the new attempt succeeds and cleans itself. Add a sibling/outside sentinel and assert it is never removed.

- [ ] **Step 3: Run scratch regressions RED**

Run: `cargo test -p orchestrator-state --test legacy_import scratch_ -- --nocapture`

Expected: anonymous `TempDir` storage provides no durable scratch namespace or scavenging behavior.

- [ ] **Step 4: Implement locked scratch lifecycle**

Create a private `import-scratch/<fingerprint>` below `paths.database.parent()`. Validate the 64-hex fingerprint, exact containment, private permissions, no links/reparse points, and `attempt-<uuid>` child names. Hold `import.lock` for the attempt. Scavenge only valid attempt children whose `owner.lock` can be acquired; preserve `WouldBlock` attempts. Create and lock the current owner before database work. Normal drop removes only its own exact attempt; crash leaves recoverable files.

- [ ] **Step 5: Move inspection/apply migrated and rewrite databases**

Change inspection to receive `GlobalStatePaths`, derive its deterministic source-identity scratch fingerprint after guarded status/identity validation, then create `source.db` and `migrated.db` in locked scratch. Apply reinspection and migration use the plan source fingerprint and place `source.db`, `migrated.db`, and `rewrite.db` in its attempt directory. Keep scratch owners outside all related connections so drops close databases before cleanup.

- [ ] **Step 6: Run scratch and state tests GREEN**

Run:

```text
cargo test -p orchestrator-state --test legacy_import scratch_ -- --nocapture
cargo test -p orchestrator-state --test legacy_import -- --nocapture
cargo test -p orchestrator-state --test migration_contract -- --nocapture
cargo test -p orchestrator-state --all-features
```

Expected: all commands exit 0.

### Task 3: Align authority fixture and documentation claims

**Files:**
- Modify: `crates/orchestrator-state/tests/legacy_import.rs`
- Modify: `docs/superpowers/specs/2026-07-25-legacy-import-review-fixes-design.md`
- Modify: `docs/superpowers/plans/2026-07-25-legacy-import-review-fixes.md`
- Modify: `.superpowers/sdd/global-task-3-report.md`

- [ ] **Step 1: Strengthen the approved-graph fixture**

Seed a completed sealed requirement in the graph session. Populate `GraphValidationAuthority` with that revision, validation hash/checks, base commit, and redacted root; store it in both the graph revision and approval. Assert the imported typed authority remains complete and references the expected imported requirement revision.

- [ ] **Step 2: Correct prior design and plan wording**

Change `prepare_rewrite_scratch` documentation to `StateResult<()>`. Replace the unverified exact published-database hash claim with the actual assertions: unchanged source evidence hashes and original sealed proposal/preview IDs and hashes queried from published `legacy.db`. Retain only manifest claims covered by the existing seal/replay tests.

- [ ] **Step 3: Run graph/integration and focused tests**

Run:

```text
cargo test -p orchestrator-state --test legacy_import approved_graph_collision -- --nocapture
cargo test -p orchestrator-state --test legacy_import integration_collision -- --nocapture
cargo test -p orchestrator-state --test legacy_import -- --nocapture
```

Expected: all commands exit 0.

### Task 4: Verify, report, and commit

**Files:**
- Modify: `.superpowers/sdd/global-task-3-report.md`

- [ ] **Step 1: Run formatting and state gates**

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo test -p orchestrator-state --all-features
```

- [ ] **Step 2: Run required full gates**

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

- [ ] **Step 3: Append exact evidence**

Record ABA and scratch RED causes, focused GREEN counts, authority/evidence assertions, each command/exit result, and any rerun evidence without overstating hash coverage.

- [ ] **Step 4: Inspect and commit only scoped files**

Run `git diff --check`, inspect `git diff --stat` and `git status --short`, then commit with `fix: close legacy import lifecycle races`.
