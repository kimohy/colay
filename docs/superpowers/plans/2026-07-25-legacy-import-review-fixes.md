# Legacy Import Review Fixes Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the four audit-critical legacy-import review findings while preserving original source and published SQLite evidence byte-for-byte.

**Architecture:** Validate the migrated source completely, copy it to a private `rewrite.db`, assert and remove only six exact immutable UPDATE triggers in that scratch, and attach only the scratch for deterministic transformations. Gate all target mutations with a transaction-local chain/cursor check, validate typed requirement revisions before scratch creation, and centralize contained source reads with component and canonical-path checks.

**Tech Stack:** Rust 2024, rusqlite/SQLite, orchestrator-domain typed contracts, SHA-256, tempfile, platform-gated filesystem links.

## Global Constraints

- Never mutate the original repository state, the inspected SQLite backup, or the published `legacy.db` evidence.
- Remove only the six approved immutable UPDATE triggers from private scratch after exact normalized-definition checks.
- Validate the target chain and cursor inside the import transaction before backup, row, ledger, anchor, or file publication mutation.
- Keep `orchestrator-domain` vendor-neutral and I/O-free.
- Use separated `Command` executable/args; tests never invoke real provider inference.
- Do not change migration schema 14 or public importer signatures.

---

### Task 1: Transform only a verified private scratch image

**Files:**
- Modify: `crates/orchestrator-state/tests/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/legacy_import.rs`

**Interfaces:**
- Produces: `prepare_rewrite_scratch(validated: &Path, scratch: &Path) -> StateResult<()>`.
- Produces: `validate_and_drop_rewrite_triggers(connection: &Connection) -> StateResult<()>`.
- Consumes: existing collision mapping and typed rewrite functions unchanged.

- [ ] **Step 1: Write failing approved-graph collision regression**

Seed a schema-13 source with a completed, non-planning `TaskGraphProposal`, a sealed requirement revision, validation authority, and `graph_approvals` row. Seed the target with the same graph revision ID. Import and assert the mapped revision is present, `proposal_hash` equals a fresh `task_graph_proposal_hash`, the graph and approval retain complete validation authority, the approval carries the new proposal hash, source evidence hashes are unchanged, and published `legacy.db` retains the original proposal identity and sealed hash.

- [ ] **Step 2: Run the graph regression and verify RED**

Run: `cargo test -p orchestrator-state --test legacy_import approved_graph_collision -- --nocapture`

Expected: FAIL with SQLite `graph revision payload is immutable` from updating the attached migrated snapshot.

- [ ] **Step 3: Write failing integration collision regression**

Seed a batch, source, approval, and application whose batch/session/graph/task/checkpoint/verification IDs collide in the target. Import and assert every mapped row references the new IDs, `IntegrationPreview::verify_hash()` succeeds, and the approval/application hashes equal the rewritten preview hash. Assert unchanged source and published evidence hashes.

- [ ] **Step 4: Run the integration regression and verify RED**

Run: `cargo test -p orchestrator-state --test legacy_import integration_collision -- --nocapture`

Expected: FAIL with SQLite `integration preview payload is immutable`.

- [ ] **Step 5: Add exact trigger validation and private scratch preparation**

Add a six-entry constant of `(trigger_name, normalized_expected_sql)`. Implement SQL normalization as ASCII-whitespace collapsing, query each `sqlite_schema.sql`, compare exactly after normalization, then issue a separately quoted `DROP TRIGGER` for those six fixed names only. Copy the validated migrated image to `rewrite.db`, set private permissions, configure SQLite, validate triggers, drop them, and re-run integrity/foreign-key checks.

- [ ] **Step 6: Attach scratch, not the validated migrated image**

Keep the migrated validation connection open read-only long enough to validate event chain and all documents; close it without mutation. Create `rewrite.db`, attach it as `legacy_import_source`, and let existing collision/remap/reseal code mutate only scratch.

- [ ] **Step 7: Run both regressions and the focused suite GREEN**

Run: `cargo test -p orchestrator-state --test legacy_import -- --nocapture`

Expected: graph and integration fixtures plus the existing 14 tests pass.

### Task 2: Gate target mutation on complete chain and cursor validation

**Files:**
- Modify: `crates/orchestrator-state/tests/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/legacy_import.rs`

**Interfaces:**
- Produces: `validate_target_event_chain(transaction: &Transaction<'_>, workspace_id: &str) -> StateResult<Vec<TaskEvent>>`.
- Produces: `validate_event_log_cursor(connection: &Connection, workspace_id: &str, events: &[TaskEvent]) -> StateResult<()>`.

- [ ] **Step 1: Write corrupt/gapped target chain regression**

Create two valid target events, corrupt or remove sequence 1 while keeping sequence 2, record target task/ledger/backup/import-file counts, and call apply. Assert an `InvalidEventChain` error and exact equality of all counts afterward, including no published fingerprint directory.

- [ ] **Step 2: Run target-chain regression and verify RED**

Run: `cargo test -p orchestrator-state --test legacy_import corrupt_earlier_target_event -- --nocapture`

Expected: FAIL because the importer creates its pre-import backup before detecting the invalid chain.

- [ ] **Step 3: Add cursor inconsistency regression**

Set `event_log_state.last_exported_sequence/hash` to a non-prefix pair and assert the same zero-mutation outcome.

- [ ] **Step 4: Validate chain and cursor before backup or import**

Begin the target transaction, check workspace existence, call the full chain validator, validate an optional `event_log_state` row against the exact chain prefix, and only then create the pre-import backup. Pass the validated event count into row import rather than recounting untrusted events.

- [ ] **Step 5: Run target regressions GREEN**

Run: `cargo test -p orchestrator-state --test legacy_import target -- --nocapture`

Expected: all selected target-chain and target-projection tests pass.

### Task 3: Validate typed requirement revisions before transform

**Files:**
- Modify: `crates/orchestrator-state/tests/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/legacy_import.rs`

**Interfaces:**
- Produces: `validate_source_requirements(connection: &Connection) -> StateResult<()>`.
- Consumes: `RequirementSnapshot`, exact domain ID parsers, `SchemaVersion::V1`, and `canonical_sha256`.

- [ ] **Step 1: Write snapshot-hash mismatch regression**

Seed a valid requirement revision, replace `snapshot_hash` with another 64-hex value, inspect, and assert rejection with no target rows/files/backups.

- [ ] **Step 2: Run hash regression and verify RED**

Run: `cargo test -p orchestrator-state --test legacy_import requirement_snapshot_hash -- --nocapture`

Expected: FAIL because inspection currently accepts the row.

- [ ] **Step 3: Write row/snapshot consistency regression**

Seed a valid typed snapshot but invert the persisted `complete` bit; separately use invalid typed JSON or an invalid row identity/ordinal. Assert each is rejected before target mutation.

- [ ] **Step 4: Implement typed validation**

For every reserved-workspace row, parse `RequirementRevisionId`, `SessionId`, and `MessageId`; require ordinal greater than zero and `schema_version == SchemaVersion::V1`; deserialize `RequirementSnapshot`; require `complete == snapshot.is_complete()`; and compare `canonical_sha256(&snapshot)` with `snapshot_hash`. Invoke this from `validate_source_documents` before any rewrite.

- [ ] **Step 5: Run requirement regressions GREEN**

Run: `cargo test -p orchestrator-state --test legacy_import requirement -- --nocapture`

Expected: all selected requirement tests pass.

### Task 4: Revalidate source containment at every open/copy

**Files:**
- Modify: `crates/orchestrator-state/tests/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/artifacts.rs`

**Interfaces:**
- Produces: `canonical_contained_source(root: &Path, file: &Path) -> StateResult<(PathBuf, PathBuf)>`.
- Updates: `LegacyImportStaging::stage_file` to receive the source root and read through the containment helper or equivalent internal checks.

- [ ] **Step 1: Write nested link regression**

On Unix, replace a nested artifact directory with a symbolic link to an external directory containing matching bytes. On Windows, create a directory symlink when permitted and otherwise platform-skip only the link-creation branch. Assert inspection or apply returns `SymlinkEscape` and adds no target evidence.

- [ ] **Step 2: Run link regression and verify RED**

Run: `cargo test -p orchestrator-state --test legacy_import nested_link -- --nocapture`

Expected: FAIL because final-component-only checks follow a linked parent directory.

- [ ] **Step 3: Centralize contained opens**

Immediately before each database, JSONL, and artifact open/copy, call `reject_symlink_components` on root and full path, canonicalize both, require containment, open, canonicalize again, require the same contained canonical path, and hash/read using the opened handle. Use the same logic from manifest collection and staging.

- [ ] **Step 4: Run link regression and focused suite GREEN**

Run: `cargo test -p orchestrator-state --test legacy_import -- --nocapture`

Expected: all focused import tests pass.

### Task 5: Verify, report, and commit

**Files:**
- Modify: `.superpowers/sdd/global-task-3-report.md`

**Interfaces:**
- Produces: durable RED/GREEN and full-gate evidence for the parent task.

- [ ] **Step 1: Run focused and state verification**

```text
cargo test -p orchestrator-state --test legacy_import -- --nocapture
cargo test -p orchestrator-state --test migration_contract -- --nocapture
cargo test -p orchestrator-state --all-features
```

Expected: exit 0 for every command.

- [ ] **Step 2: Run required workspace gates**

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: exit 0 for every command.

- [ ] **Step 3: Append evidence to the report**

Record each focused RED reason, subsequent GREEN count, source/evidence immutability assertions, exact gate commands, exit results, and any environmental flakes with isolated/full rerun evidence.

- [ ] **Step 4: Inspect and commit only scoped files**

Run `git diff --check`, inspect staged statistics, and commit with:

```text
git commit -m "fix: harden legacy state import validation"
```
