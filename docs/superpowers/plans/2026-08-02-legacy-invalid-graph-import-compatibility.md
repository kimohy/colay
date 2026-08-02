# Legacy Invalid-Graph Import Compatibility Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Preserve legitimate unsealed legacy graph attempts during user-global import, expose import readiness through `doctor`, and make pre-IPC daemon exits actionable.

**Architecture:** Legacy graph validation branches on the persisted proposal/hash pair and status: sealed graphs retain typed summary, authority, identity, and hash checks; unsealed `planning`, `invalid`, `cancelled`, and `superseded` graphs retain arbitrary valid JSON evidence and require absent row authority, while unsealed `awaiting_approval` and `approved` graphs fail closed. Approval rows bind null-safely to every sealed graph authority field. CLI diagnostics reuse the guarded `LegacyImporter::inspect` path without mutating source or target state and publish only fixed, source-value-free, bounded reasons, while startup failures point users to `colay doctor` instead of capturing raw stderr.

**Tech Stack:** Rust 1.95, rusqlite, serde/serde_json, anyhow, existing orchestrator-state snapshot/import infrastructure, Cargo integration tests, WSL 2 Ubuntu 24.04.

## Global Constraints

- Preserve the original repository-local SQLite database and append-only audit evidence; never delete, rewrite, or synthesize invalid graph evidence.
- Keep `STATE_SCHEMA_VERSION` at 16 and add no migration.
- Keep provider wire types out of `orchestrator-domain`; this change requires no provider wire changes.
- Use Rust `Command` with separated executable and arguments; do not add shell interpolation.
- Automated tests and CI use only `orchestrator-test-support` fake binaries and make zero real provider inference requests.
- Keep writable task execution isolated in worktrees; this implementation changes no task/worktree path.
- Do not persist raw daemon stderr, credentials, prompts, validation error contents, or unbounded diagnostics.
- Required final gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

## File Map

- `crates/orchestrator-state/src/legacy_import.rs`: validate sealed and unsealed graph payloads using distinct status/shape invariants; reject approvals of unsealed revisions and null-safely bind approvals to sealed graph authority.
- `crates/orchestrator-state/tests/legacy_import.rs`: reproduce `{"errors":[...]}` invalid graph evidence and verify preservation, failure boundaries, and source immutability.
- `crates/orchestrator-cli/src/app.rs`: add the `legacy_import` doctor check and pass effective configuration into state diagnostics.
- `crates/orchestrator-cli/tests/global_doctor.rs`: exercise import-ready and blocked repository-local stores through the public `doctor` command with fake providers.
- `crates/orchestrator-cli/src/ipc_client.rs`: centralize the bounded pre-IPC contender-exit diagnostic and add `colay doctor` recovery guidance.
- `docs/qa/wsl-nightly-error-tracker.md`: record `WSL-019`, source verification, and public-nightly verification state.

### Task 1: Preserve unsealed legacy graph evidence

**Files:**
- Modify: `crates/orchestrator-state/tests/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/legacy_import.rs:1275-1340`

**Interfaces:**
- Consumes: `LegacyImporter::inspect(&RepositoryStatePaths, &GlobalStatePaths) -> StateResult<Option<LegacyImportPlan>>` and `LegacyImporter::apply(&Database, WorkspaceId, &LegacyImportPlan, &GlobalStatePaths) -> StateResult<LegacyImportResult>`.
- Produces: status/shape-aware behavior inside `validate_source_graphs(&Connection) -> StateResult<()>`; no new public API.

- [ ] **Step 1: Add an invalid-graph preservation fixture and failing regression**

Add a helper beside `seed_source_graph` that creates a graph in the private post-migration schema-13 fixture stage used to model the guarded inspection snapshot of an authoritative schema-8 source, then converts it to the exact unsealed shape:

```rust
fn seed_invalid_source_graph(fixture: &ImportFixture) -> TestResult<GraphSeed> {
    let graph = seed_source_graph(fixture, "invalid", false)?;
    Connection::open(&fixture.source.database)?.execute(
        "UPDATE graph_revisions
         SET proposal_hash = NULL,
             proposal_json = NULL,
             validation_json = ?1,
             requirement_revision_id = NULL,
             validation_hash = NULL,
             base_commit = NULL
         WHERE workspace_id = ?2 AND revision_id = ?3",
        params![
            "{ \"errors\" : [ \"cycle\" ] }",
            RESERVED_LEGACY_WORKSPACE,
            graph.revision_id.to_string(),
        ],
    )?;
    Ok(graph)
}
```

Add `invalid_graph_evidence_imports_without_source_mutation`. It must hash source evidence before inspection, call inspect/apply, resolve the mapped revision through `legacy_import_id_mappings`, and assert the target row preserves the deliberately noncanonical validation SQLite `TEXT` byte-for-byte together with `("invalid", None, None)`. Assert zero `graph_approvals` for the mapped revision and unchanged source hashes.

- [ ] **Step 2: Run the preservation regression and verify RED**

Run:

```text
cargo test -p orchestrator-state --test legacy_import invalid_graph_evidence_imports_without_source_mutation -- --nocapture
```

Expected: FAIL from `LegacyImporter::inspect` with `JSON serialization failed: missing field 'node_count'`.

- [ ] **Step 3: Add failing shape-integrity regressions**

Add table-driven or separate fixture tests that start from `seed_source_graph(fixture, "awaiting_approval", false)` and mutate one invariant at a time:

```rust
// proposal present, hash absent
UPDATE graph_revisions SET proposal_hash = NULL WHERE revision_id = ?1;

// unsealed graph with row authority
UPDATE graph_revisions
SET proposal_hash = NULL, proposal_json = NULL,
    validation_json = '{"errors":["cycle"]}',
    validation_hash = ?2
WHERE revision_id = ?1;
```

Bind `"e".repeat(64)` as `?2`. Each test calls `LegacyImporter::inspect`, expects `persisted record is invalid`, and verifies target mutation counts and source evidence remain unchanged. Add an unsealed-approval test by clearing proposal/hash and inserting a `graph_approvals` row with `"f".repeat(64)` as its fixture hash; inspection must reject it rather than allowing SQL `NULL` comparison to hide the mismatch.

Add separate regressions for unsealed `awaiting_approval` and `approved` rows, plus a characterization matrix that retains unsealed `planning`, `invalid`, `cancelled`, and `superseded`. Add a sealed approval regression that sets the approval session/requirement/validation/base authority columns to `NULL` while the graph remains authoritative; null-safe comparison must reject it before target mutation. Add malformed unsealed validation JSON coverage by enabling SQLite `ignore_check_constraints` only on the fixture corruption connection, then assert rejection with corrupted source bytes and target mutation counts unchanged.

- [ ] **Step 4: Implement shape-aware validation**

In `validate_source_graphs`, select `status`, parse `validation_json` first as `serde_json::Value`, normalize row authority without lossy `Option::zip`, and branch on `(proposal, hash)`:

```rust
let validation_value: serde_json::Value = serde_json::from_str(&validation_json)?;
let row_authority = match (requirement, validation_hash, base) {
    (None, None, None) => None,
    (Some(requirement), Some(validation_hash), Some(base)) => {
        Some((requirement, validation_hash, base))
    }
    _ => {
        return Err(StateError::InvalidRecord(format!(
            "legacy graph revision {revision} has incomplete validation authority"
        )));
    }
};

match (proposal, hash) {
    (None, None) if row_authority.is_none() => match status.as_str() {
        "planning" | "invalid" | "cancelled" | "superseded" => {}
        "awaiting_approval" | "approved" => return Err(/* proposal seal required */),
        _ => return Err(/* unsupported status */),
    },
    (Some(json), Some(hash)) => {
        let validation: GraphValidationSummary =
            serde_json::from_value(validation_value)?;
        // Retain the existing proposal identity, schema, authority, and hash checks.
    }
    _ => {
        return Err(StateError::InvalidRecord(
            "legacy graph revision has an incomplete proposal seal".to_owned(),
        ));
    }
}
```

Change the approval mismatch query to reject `revision.proposal_hash IS NULL`, `revision.proposal_json IS NULL`, or null-safe inequality (`IS NOT`) between approval and revision proposal hash, session ID, requirement revision ID, validation hash, or base commit. Do not modify `rewrite_graphs`; its `proposal_json IS NOT NULL` filter is the intended sealed-only rewrite boundary.

- [ ] **Step 5: Run state regressions and verify GREEN**

Run:

```text
cargo test -p orchestrator-state --test legacy_import invalid_graph -- --nocapture
cargo test -p orchestrator-state --test legacy_import graph -- --nocapture
cargo test -p orchestrator-state --test legacy_import unsealed_ -- --nocapture
cargo test -p orchestrator-state --test legacy_import graph_approval_authority_mismatch_is_refused_before_target_mutation -- --nocapture --exact
cargo test -p orchestrator-state --all-features
```

Expected: all selected and crate tests PASS; the existing approved graph collision/hash test remains green.

- [ ] **Step 6: Commit the state-layer correction**

```text
git add crates/orchestrator-state/src/legacy_import.rs crates/orchestrator-state/tests/legacy_import.rs
git commit -m "fix: preserve unsealed legacy graph evidence"
```

### Task 2: Diagnose pending legacy import through doctor

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs:522-790`
- Modify: `crates/orchestrator-cli/tests/global_doctor.rs`

**Interfaces:**
- Consumes: `RepositoryStatePaths::from_config`, `LegacyImporter::inspect`, `LegacyImportPlan::source_schema_version`, `LegacyImportPlan::source_fingerprint`, `RootConfig`, `GlobalStatePaths`, and the existing `Check` constructors.
- Produces: private `legacy_import_check(repository: &Path, config: &RootConfig, paths: &GlobalStatePaths) -> Check`; adds the serialized doctor check named `legacy_import`.

- [ ] **Step 1: Add a WSL-shaped doctor fixture**

Extend `DoctorFixture` with `seed_repository_legacy_invalid_graph_schema_v8`. Reuse `seed_repository_legacy_schema_v8`, then insert one session, one final user conversation message, and one graph revision in the pre-workspace schema:

```sql
INSERT INTO graph_revisions(
    revision_id, session_id, goal_message_id, ordinal, status,
    proposal_hash, proposal_json, validation_json, planner_provider,
    created_at, completed_at
) VALUES (?1, ?2, ?3, 1, 'invalid', NULL, NULL, ?4, 'codex', ?5, ?5);
```

Use `serde_json::to_string(&json!({"errors":["cycle"]}))` for `?4`. This source migrates through schema 13 inside inspection and reproduces the user's missing-`node_count` path without embedding user content.

- [ ] **Step 2: Add public doctor RED regressions**

Add `legacy_import_doctor_reports_import_ready_invalid_graph_source`:

1. seed a current global workspace and fake provider configuration;
2. seed the repository-local invalid graph store;
3. hash the repository-local database before the command;
4. run `colay --json doctor`;
5. assert exit success, `check_named(..., "legacy_import")["status"] == "pass"`, `pending == true`, and the reported source schema is 8; and
6. assert the source hash and global workspace/import-ledger counts are unchanged.

Add `legacy_import_doctor_fails_an_incomplete_proposal_seal_without_mutation` by changing the seeded row to retain `proposal_json = NULL` but set a 64-character `proposal_hash`. Doctor remains a reporting command and therefore exits successfully; assert the report has `data.passed == false`, the `legacy_import` check is `fail`, its detail equals the fixed incomplete-proposal remediation reason, and no provider inference or state mutation occurs. Add a public long-sensitive-identifier regression asserting the generic fixed remediation reason, maximum 256-character detail, marker absence, successful process exit, `passed=false`, source/target immutability, and zero inference requests.

- [ ] **Step 3: Run doctor tests and verify RED**

Run:

```text
cargo test -p colay --test global_doctor --features test-fixtures legacy_import -- --nocapture
```

Expected: FAIL because doctor currently omits the `legacy_import` check.

- [ ] **Step 4: Implement the import-readiness check**

Import `LegacyImporter` in `app.rs`. Pass `effective.config()` into `doctor_state_checks`. Add:

```rust
fn legacy_import_check(
    repository: &Path,
    config: &RootConfig,
    paths: &GlobalStatePaths,
) -> Check {
    let source = match StatePaths::from_config(repository, config) {
        Ok(source) => source,
        Err(_) => return legacy_import_failure_check(LEGACY_IMPORT_PATH_DETAIL),
    };
    match LegacyImporter::inspect(&source, paths) {
        Ok(Some(plan)) => Check::with_data(
            "legacy_import",
            true,
            json!({
                "pending": true,
                "source_schema_version": plan.source_schema_version,
                "source_fingerprint": plan.source_fingerprint,
                "source_database": source.database,
            }),
        ),
        Ok(None) => Check::with_data(
            "legacy_import",
            true,
            json!({"pending": false, "source_database": source.database}),
        ),
        Err(error) => legacy_import_inspection_failure_check(&error),
    }
}
```

Map every inspection failure to a fixed, actionable, source-value-free reason, cap the result at 256 characters, and preserve a distinct fixed reason only for `legacy graph revision has an incomplete proposal seal`. Never publish the source error string, record identifier, nested SQLite/JSON error, or raw input. Call the check immediately after acquiring maintenance ownership, before any early return for absent or pending global schema. When maintenance ownership is held by a live daemon, resolve the configured repository-local path without opening it: emit pass with `pending=false` if its database is absent and warn with the exact `import readiness is unavailable through live-daemon IPC` detail plus source database path if it exists. Do not start a provider or mutate global rows.

- [ ] **Step 5: Run focused and existing doctor tests**

Run:

```text
cargo test -p colay --test global_doctor --features test-fixtures legacy_import -- --nocapture
cargo test -p colay --test global_doctor --features test-fixtures -- --nocapture
cargo test -p colay --test default_startup --features test-fixtures doctor -- --nocapture
```

Expected: all PASS, `inference_requests` stays zero, and existing read-only/WAL checks remain green.

- [ ] **Step 6: Commit doctor diagnostics**

```text
git add crates/orchestrator-cli/src/app.rs crates/orchestrator-cli/tests/global_doctor.rs
git commit -m "feat: diagnose pending legacy imports"
```

### Task 3: Make pre-IPC daemon exits actionable

**Files:**
- Modify: `crates/orchestrator-cli/src/ipc_client.rs:411-490,800-1320`

**Interfaces:**
- Consumes: existing `wait_until_ready`, contender polling, and bounded exit-status string.
- Produces: private `daemon_contenders_exited_message(exit: &str) -> String` used by startup and unit tests.

- [ ] **Step 1: Write the failing diagnostic unit test**

Add the helper import to the `ipc_client` test module and test the exact recovery boundary:

```rust
#[test]
fn contender_exit_diagnostic_points_to_doctor_without_claiming_a_cause() {
    let message = daemon_contenders_exited_message("exit status: 1");
    assert!(message.contains("exited before IPC readiness"));
    assert!(message.contains("last exit: exit status: 1"));
    assert!(message.contains("run `colay doctor`"));
    assert!(!message.contains("node_count"));
}
```

- [ ] **Step 2: Run the unit test and verify RED**

Run:

```text
cargo test -p colay --features test-fixtures --bin colay contender_exit_diagnostic -- --nocapture
```

Expected: compile FAIL because the helper does not exist.

- [ ] **Step 3: Implement and use the bounded message helper**

Add:

```rust
fn daemon_contenders_exited_message(exit: &str) -> String {
    format!(
        "user daemon contenders exited before IPC readiness; last exit: {exit}; run `colay doctor` for startup diagnostics"
    )
}
```

Replace only the existing contender-exit branch with `bail!("{}", daemon_contenders_exited_message(&exit))`. Do not capture, inherit, or persist daemon stderr and do not change child cleanup behavior.

- [ ] **Step 4: Run IPC and daemon lifecycle tests**

Run:

```text
cargo test -p colay --features test-fixtures --bin colay -- --nocapture
cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture
cargo test -p colay --test daemon_lifecycle --features test-fixtures -- --nocapture
```

Expected: all PASS with unchanged startup ownership, cleanup, and readiness behavior.

- [ ] **Step 5: Commit startup guidance**

```text
git add crates/orchestrator-cli/src/ipc_client.rs
git commit -m "fix: point daemon startup failures to doctor"
```

### Task 4: Record WSL-019 and verify the user flow

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

**Interfaces:**
- Consumes: exact test outputs, source commit identity, copied WSL source hash, compiled binary identity, and doctor/daemon results.
- Produces: `WSL-019` record with status `source-fixed; deployed-nightly verification pending` until a public nightly containing the fix exists.

- [ ] **Step 1: Run formatting and focused cross-component gates**

Run:

```text
cargo fmt --all -- --check
git diff --check
cargo test -p orchestrator-state --test legacy_import --all-features -- --nocapture
cargo test -p colay --test global_doctor --features test-fixtures -- --nocapture
cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture
cargo test -p colay --test daemon_lifecycle --features test-fixtures -- --nocapture
```

Expected: every command PASS.

- [ ] **Step 2: Run the required Windows repository gates**

Run exactly:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all workspace targets, features, tests, and doc tests PASS.

- [ ] **Step 3: Build and verify a Linux candidate against a private copy**

From WSL, allocate `qa_root` with `mktemp -d /home/kimohy/.cache/colay-wsl019.XXXXXX`, copy the branch worktree from `/mnt/c/Users/kimoh/Documents/Codex/2026-07-18/principal-rust-engineer-ai-agent-orchestration/.worktrees/legacy-invalid-graph-import` to `$qa_root/source`, and build `colay` plus `colay-e2e-fake-provider` with `--features test-fixtures` and `CARGO_TARGET_DIR=$qa_root/target`. Create `$qa_root/repository`, copy `/home/kimohy/.colay` to `$qa_root/repository/.colay`, and set `COLAY_HOME=$qa_root/home` for every candidate command. Do not remove the QA directory or any existing worktree.

Record before/after SHA-256 for the original `/home/kimohy/.colay/orchestrator.db`; they must match. With `COLAY_TEST_FAKE_PROVIDERS_ONLY=1`, run:

```text
colay --json doctor
colay --json migrate apply
colay --json daemon start
colay --json daemon status
colay --json daemon stop
```

Expected: doctor reports import-ready, daemon reaches online, daemon stops, SQLite integrity is `ok`, foreign-key violations are zero, and the copied invalid graph remains `invalid` with absent proposal/hash and an `errors` validation object. Separately run the WSL-built integration tests `cargo test -p colay --test global_doctor --features test-fixtures legacy_import -- --nocapture` and `cargo test -p colay --test chat_plan_fake_provider --features test-fixtures -- --nocapture`; these establish the fake-provider diagnostic and conversation paths without using the copied user configuration. No real provider is invoked.

- [ ] **Step 4: Update WSL-019 from collected evidence**

Add a section containing:

- nightly `0.1.1-nightly.20260801.3f4e2f7` and NVM executable path;
- generic contender error and bounded foreground `node_count` diagnostic;
- schema-16 global DB health, the authoritative schema-8 source, and the unsealed invalid graph structure observed only after its guarded private snapshot migrates through schema 13;
- root cause in unconditional `GraphValidationSummary` deserialization;
- exact focused/full Windows and WSL source verification results;
- original source database before/after hash equality; and
- public nightly verification marked pending.

Do not include user prompt content or the historical validation error string.

- [ ] **Step 5: Commit QA evidence**

```text
git add docs/qa/wsl-nightly-error-tracker.md
git commit -m "docs: record WSL legacy graph import regression"
```

### Task 5: Final exact-HEAD verification and review handoff

**Files:**
- Verify only; modify a file only if a test exposes a defect, then repeat the affected TDD cycle.

**Interfaces:**
- Consumes: Tasks 1-4 commits.
- Produces: clean branch and reproducible verification evidence suitable for code review; no push, PR, merge, or nightly publication without a later explicit request.

- [ ] **Step 1: Verify the exact committed tree**

Run:

```text
git status --short --branch
git diff --check origin/main...HEAD
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: clean worktree, only intentional branch commits, and all required gates PASS.

- [ ] **Step 2: Inspect the final diff and commit sequence**

Run:

```text
git diff --stat origin/main...HEAD
git log --oneline --decorate origin/main..HEAD
git diff origin/main...HEAD -- crates/orchestrator-state/src/legacy_import.rs crates/orchestrator-cli/src/app.rs crates/orchestrator-cli/src/ipc_client.rs docs/qa/wsl-nightly-error-tracker.md
```

Confirm no schema migration, provider invocation, source-data mutation, worktree deletion, credential handling, or unrelated changes are present.

- [ ] **Step 3: Present the implementation for review**

Report the root cause, changed behavior, commits, Windows/WSL test evidence, unchanged original WSL database hash, and remaining deployed-nightly verification. Stop before push/PR/merge/release unless the user explicitly authorizes those external state changes.
