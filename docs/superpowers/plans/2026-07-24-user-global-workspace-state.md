# User-Global Workspace State Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace repository-local state with one user-global SQLite database partitioned by stable workspace identifiers, and make normal CLI state mutations flow through one daemon writer.

**Architecture:** `orchestrator-state` resolves OS-global paths, registers a current workspace, and exposes workspace-scoped persistence plus user-global metadata. The CLI starts one user daemon, uses a typed local IPC protocol for commands, imports encountered legacy state once, and treats `run` as a conversation-first operation whose exact approved graph is the only path to a writable worktree.

**Tech Stack:** Rust 2024, rusqlite 0.37, Tokio local sockets/named pipes, serde JSON, UUIDv7, clap, `orchestrator-test-support` fake provider binaries.

## Global Constraints

- One SQLite database per OS user environment; Windows and WSL never share a SQLite file.
- Every workspace-owned durable row is scoped by a non-null `workspace_id`; user-global metadata is explicitly separate.
- Normal CLI/TUI operation does not write SQLite directly; one user daemon owns writes.
- Provider calls, Git commands, and filesystem copies never run inside SQLite transactions.
- Missing local or global config uses validated defaults and cannot block state migration.
- Provider safe mode cannot block database creation, backup, forward migration, import inspection, or state doctor.
- Plan conversation is allowed outside Git; committed Git is required only for approved writable materialization.
- Legacy repository state is imported idempotently and is never modified or deleted.
- Persisted changes preserve append-only audit semantics, redaction, exact approvals, and worktree isolation.
- Product and tests use separated executable/argument arrays; no shell interpolation.
- Tests use fake providers and never invoke real Codex, Claude, Gemini, or Agy inference.
- Required final verification is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

### Task 1: OS-global path resolution and workspace registry

**Files:**
- Create: `crates/orchestrator-state/src/global_paths.rs`
- Create: `crates/orchestrator-state/src/workspace_registry.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Modify: `crates/orchestrator-state/src/error.rs`
- Modify: `crates/orchestrator-state/src/migrations.rs`
- Create: `migrations/0012_global_workspaces.sql`
- Test: `crates/orchestrator-state/tests/global_workspace_state.rs`

**Interfaces:**
- Produces: `GlobalStatePaths::resolve(environment: &StateEnvironment) -> StateResult<GlobalStatePaths>`.
- Produces: `WorkspaceId`, `WorkspaceKind`, `WorkspaceStatus`, `WorkspaceRegistration`, and `Database::{resolve_workspace, attach_workspace, load_workspace}`.
- Consumes later: every CLI command receives a `WorkspaceRegistration` and uses `paths.for_workspace(workspace_id)`.

- [ ] **Step 1: Write failing global-path tests**

```rust
#[test]
fn colay_home_override_is_independent_of_current_directory() -> TestResult {
    let root = tempfile::tempdir()?;
    let environment = StateEnvironment::with_colay_home(root.path().join("home"));
    let paths = GlobalStatePaths::resolve(&environment)?;
    assert_eq!(paths.database, root.path().join("home/state/state.db"));
    assert_eq!(paths.workspaces, root.path().join("home/data/workspaces"));
    Ok(())
}

#[test]
fn two_directories_receive_distinct_stable_workspace_ids() -> TestResult {
    let fixture = GlobalFixture::new()?;
    let first = fixture.database.resolve_workspace(&fixture.first, WorkspaceKind::Directory)?;
    let again = fixture.database.resolve_workspace(&fixture.first, WorkspaceKind::Directory)?;
    let second = fixture.database.resolve_workspace(&fixture.second, WorkspaceKind::Directory)?;
    assert_eq!(first.workspace_id, again.workspace_id);
    assert_ne!(first.workspace_id, second.workspace_id);
    Ok(())
}
```

- [ ] **Step 2: Run the focused test and confirm RED**

Run: `cargo test -p orchestrator-state --test global_workspace_state -- --nocapture`

Expected: compilation fails because `GlobalStatePaths`, `StateEnvironment`, and workspace registry APIs do not exist.

- [ ] **Step 3: Add migration 12 and minimal path/registry implementation**

Migration 12 creates:

```sql
CREATE TABLE workspaces (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('git', 'directory')),
    status TEXT NOT NULL CHECK (status IN ('active', 'detached', 'archived')),
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
);
CREATE TABLE workspace_paths (
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
    canonical_path TEXT NOT NULL,
    comparison_key TEXT NOT NULL,
    git_common_dir TEXT,
    is_current INTEGER NOT NULL CHECK (is_current IN (0, 1)),
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, comparison_key)
);
CREATE UNIQUE INDEX workspace_paths_one_current_path
ON workspace_paths(comparison_key) WHERE is_current = 1;
```

Implement exact public types:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct WorkspaceId(Uuid);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GlobalStatePaths {
    pub root: PathBuf,
    pub database: PathBuf,
    pub backups: PathBuf,
    pub workspaces: PathBuf,
    pub runtime: PathBuf,
    pub config: PathBuf,
}

impl GlobalStatePaths {
    pub fn for_workspace(&self, workspace_id: WorkspaceId) -> WorkspaceStatePaths;
}
```

`COLAY_HOME` produces `state/`, `data/`, `runtime/`, and `config.toml` beneath the override. Native defaults use XDG on Unix and APPDATA/LOCALAPPDATA on Windows.

- [ ] **Step 4: Run state tests and confirm GREEN**

Run: `cargo test -p orchestrator-state --test global_workspace_state -- --nocapture`

Expected: all global path and registry cases pass.

- [ ] **Step 5: Commit**

```text
git add migrations/0012_global_workspaces.sql crates/orchestrator-state
git commit -m "feat: add user-global workspace registry"
```

### Task 2: Workspace-scoped audit chains and persistence context

**Files:**
- Create: `migrations/0013_workspace_partitions.sql`
- Modify: `crates/orchestrator-state/src/migrations.rs`
- Modify: `crates/orchestrator-state/src/database.rs`
- Modify: `crates/orchestrator-state/src/event_log.rs`
- Modify: `crates/orchestrator-state/src/records.rs`
- Modify: `crates/orchestrator-state/src/sessions.rs`
- Modify: `crates/orchestrator-state/src/conversations.rs`
- Modify: `crates/orchestrator-state/src/graphs.rs`
- Modify: `crates/orchestrator-state/src/client_commands.rs`
- Modify: `crates/orchestrator-state/src/leases.rs`
- Modify: `crates/orchestrator-state/src/scheduling.rs`
- Modify: `crates/orchestrator-state/src/instructions.rs`
- Modify: `crates/orchestrator-state/src/integrations.rs`
- Modify: `crates/orchestrator-state/src/daemon_instances.rs`
- Test: `crates/orchestrator-state/tests/workspace_isolation.rs`

**Interfaces:**
- Produces: `Database::workspace(workspace_id) -> WorkspaceDatabase<'_>`.
- Produces: user-global daemon and provider methods remain on `Database`; workspace state methods move to `WorkspaceDatabase`.
- Produces: `WorkspaceOutboxRecord { workspace_id, sequence, event }` and per-workspace reconciliation.

- [ ] **Step 1: Write failing isolation and audit-chain tests**

```rust
#[test]
fn tasks_and_sessions_cannot_cross_workspace_boundaries() -> TestResult {
    let fixture = PartitionFixture::new()?;
    fixture.first.create_task(&fixture.task())?;
    assert!(fixture.second.load_task(fixture.task_id)?.is_none());
    assert!(fixture.second.load_session(fixture.session_id)?.is_none());
    Ok(())
}

#[test]
fn each_workspace_event_chain_starts_at_one() -> TestResult {
    let fixture = PartitionFixture::new()?;
    let first = fixture.first.append_event(fixture.event())?;
    let second = fixture.second.append_event(fixture.event())?;
    assert_eq!(first.sequence, Some(1));
    assert_eq!(second.sequence, Some(1));
    Ok(())
}
```

- [ ] **Step 2: Run the focused test and confirm RED**

Run: `cargo test -p orchestrator-state --test workspace_isolation -- --nocapture`

Expected: compilation fails because `WorkspaceDatabase` and scoped event methods do not exist.

- [ ] **Step 3: Add partition migration and scoped wrapper**

Migration 13 rebuilds all workspace-owned tables with a leading non-null `workspace_id`, composite uniqueness, and composite foreign keys. The exact workspace-owned table set is: `tasks`, `task_attempts`, `worktrees`, `coordinator_leases`, `worker_leases`, `usage_snapshots`, `routing_audits`, `routing_usage_links`, `control_requests`, `checkpoints`, `handovers`, `verification_results`, `sessions`, `conversation_messages`, `conversation_attempts`, `requirement_revisions`, `planning_attempts`, `graph_revisions`, `graph_tasks`, `graph_task_dependencies`, `graph_approvals`, `client_commands`, `task_instructions`, `integration_batches`, `integration_items`, `task_events`, and `event_log_state`.

Use one reserved migration workspace only while converting a legacy image; new global databases never expose it. `WorkspaceDatabase` stores `&Database` and `WorkspaceId`, and every SQL statement binds the identifier. `event_log_state` uses `workspace_id` as its primary key and `task_events` uses `(workspace_id, sequence)` as its primary key.

- [ ] **Step 4: Run state unit and isolation tests**

Run: `cargo test -p orchestrator-state --all-features -- --nocapture`

Expected: state crate unit, migration, and isolation tests pass with no cross-workspace visibility.

- [ ] **Step 5: Commit**

```text
git add migrations/0013_workspace_partitions.sql crates/orchestrator-state
git commit -m "feat: partition durable state by workspace"
```

### Task 3: Global artifact layout and idempotent legacy import

**Files:**
- Create: `crates/orchestrator-state/src/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/artifacts.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Modify: `crates/orchestrator-state/src/migrations.rs`
- Create: `migrations/0014_legacy_imports.sql`
- Test: `crates/orchestrator-state/tests/legacy_import.rs`

**Interfaces:**
- Produces: `LegacyImporter::inspect(source: &RepositoryStatePaths) -> StateResult<Option<LegacyImportPlan>>`.
- Produces: `LegacyImporter::apply(global: &Database, target: WorkspaceId, plan: &LegacyImportPlan, paths: &GlobalStatePaths) -> StateResult<LegacyImportResult>`.
- Produces: `legacy_imports(source_fingerprint, workspace_id, manifest_hash, imported_at, result_json)`.

- [ ] **Step 1: Write failing import tests**

```rust
#[test]
fn legacy_repository_state_imports_once_without_source_mutation() -> TestResult {
    let fixture = ImportFixture::new()?;
    let before = fixture.source_hashes()?;
    let first = fixture.import()?;
    let second = fixture.import()?;
    assert!(first.imported);
    assert!(!second.imported);
    assert_eq!(before, fixture.source_hashes()?);
    assert_eq!(fixture.target.task_count()?, 1);
    Ok(())
}
```

- [ ] **Step 2: Run the focused test and confirm RED**

Run: `cargo test -p orchestrator-state --test legacy_import -- --nocapture`

Expected: compilation fails because `LegacyImporter` does not exist.

- [ ] **Step 3: Implement staged import and import ledger**

Migration 14 creates `legacy_imports` and `legacy_import_id_mappings`. The importer opens the source read-only, validates integrity and migration checksums, uses SQLite backup for a source snapshot, copies content into `<workspace>/imports/<fingerprint>.staging`, verifies SHA-256, imports rows in one transaction, appends an import-anchor event when the target chain is non-empty, atomically renames staging, and records the fingerprint. A matching fingerprint returns `LegacyImportResult { imported: false, ... }`.

- [ ] **Step 4: Run import and state tests**

Run: `cargo test -p orchestrator-state --test legacy_import -- --nocapture`

Expected: import, no-op replay, corrupt-event refusal, and source-preservation cases pass.

- [ ] **Step 5: Commit**

```text
git add migrations/0014_legacy_imports.sql crates/orchestrator-state
git commit -m "feat: import repository state into global workspaces"
```

### Task 4: User-daemon bootstrap, automatic migration, and local IPC

**Files:**
- Create: `crates/orchestrator-daemon/src/ipc.rs`
- Modify: `crates/orchestrator-daemon/src/lib.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Create: `crates/orchestrator-cli/src/ipc_client.rs`
- Modify: `crates/orchestrator-cli/src/main.rs`
- Modify: `crates/orchestrator-cli/Cargo.toml`
- Modify: `Cargo.toml`
- Test: `crates/orchestrator-cli/tests/global_daemon.rs`

**Interfaces:**
- Produces: newline-delimited `IpcRequest { request_id, workspace_id, action, payload }` and `IpcResponse { request_id, outcome }` with schema version `1`.
- Produces: `DaemonClient::{connect_or_start, request, stream}`.
- Consumes: `GlobalStatePaths`, workspace registry, `Database`, and existing daemon planning/execution services.

- [ ] **Step 1: Write failing daemon ownership tests**

```rust
#[test]
fn two_repositories_share_one_global_daemon_and_database() -> Result<()> {
    let fixture = GlobalDaemonFixture::new()?;
    let first = fixture.run_in(&fixture.first, ["status"])?;
    let second = fixture.run_in(&fixture.second, ["status"])?;
    assert!(first.status.success() && second.status.success());
    assert_eq!(fixture.database_files()?.len(), 1);
    assert_eq!(fixture.online_daemon_instances()?, 1);
    Ok(())
}

#[test]
fn old_schema_migrates_before_untrusted_provider_is_evaluated() -> Result<()> {
    let fixture = GlobalDaemonFixture::with_schema(8)?;
    let output = fixture.run(["daemon", "start"])?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fixture.schema_version()?, STATE_SCHEMA_VERSION);
    Ok(())
}
```

- [ ] **Step 2: Run the test and confirm RED**

Run: `cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture`

Expected: assertions fail because daemon and DB are repository-local and migration readiness still precedes startup incorrectly.

- [ ] **Step 3: Implement singleton bootstrap and IPC**

Enable Tokio `net`. Resolve global paths before repository config. Acquire an owner-only singleton lock, open the global database, call `migrate_with_backup`, register/import the requested workspace, then publish the Unix socket or Windows named pipe. Move provider probing after IPC readiness and state migration. IPC handlers submit all mutations to the daemon writer queue; no handler retains a transaction across an await.

- [ ] **Step 4: Run daemon tests and confirm GREEN**

Run: `cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture`

Expected: one daemon/database, auto-migration, permission, and second-writer rejection cases pass.

- [ ] **Step 5: Commit**

```text
git add Cargo.toml crates/orchestrator-daemon crates/orchestrator-cli
git commit -m "feat: route global state through user daemon"
```

### Task 5: Conversation-first `run` and exact writable promotion

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/args.rs`
- Modify: `crates/orchestrator-cli/src/chat_tui.rs`
- Modify: `crates/orchestrator-daemon/src/planning.rs`
- Modify: `crates/orchestrator-daemon/src/conversation.rs`
- Modify: `crates/orchestrator-engine/src/worktree.rs`
- Test: `crates/orchestrator-cli/tests/global_plan_first.rs`
- Modify: `crates/orchestrator-cli/tests/default_startup.rs`
- Modify: `crates/orchestrator-cli/tests/chat_plan_approval.rs`

**Interfaces:**
- Produces: `run` creates a session and `RequestConversationTurn` command through IPC.
- Produces: `plan_only` is persisted as a session promotion fence.
- Consumes: existing immutable requirement revision, validation authority, exact graph approval, and graph materialization APIs.

- [ ] **Step 1: Replace old direct-run expectations with failing plan-first tests**

```rust
#[test]
fn run_in_non_git_directory_creates_conversation_without_writable_state() -> Result<()> {
    let fixture = PlanFixture::non_git()?;
    let output = fixture.colay(["run", "hello"])?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert_eq!(fixture.sessions()?, 1);
    assert_eq!(fixture.tasks()?, 0);
    assert_eq!(fixture.worktrees()?, 0);
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}

#[test]
fn plan_only_session_cannot_be_promoted_by_same_command() -> Result<()> {
    let fixture = PlanFixture::committed_git()?;
    fixture.colay(["run", "--plan-only", "change code"])?;
    assert_eq!(fixture.tasks()?, 0);
    assert_eq!(fixture.worktrees()?, 0);
    Ok(())
}
```

- [ ] **Step 2: Run tests and confirm RED**

Run: `cargo test -p colay --test global_plan_first --features test-fixtures -- --nocapture`

Expected: non-Git run fails at the old direct writable Git preflight.

- [ ] **Step 3: Route run to durable conversation and move Git preflight**

Remove the legacy `run_task` persistence path from the command dispatcher. Create or resume a workspace session, append the user message, enqueue a conversation turn, and wait/stream its result through IPC. Retain `--task-file` parsing as requirement input. Store a promotion fence for `--plan-only`. Keep Git repository, `HEAD`, base drift, provider writable eligibility, and policy checks exclusively in `approve_graph_and_materialize_tasks`; create worktrees beneath the global workspace path.

- [ ] **Step 4: Run plan-first and existing approval tests**

Run: `cargo test -p colay --test global_plan_first --test chat_plan_approval --features test-fixtures -- --nocapture`

Expected: plan-first cases pass and exact approval still materializes once after preflight.

- [ ] **Step 5: Commit**

```text
git add crates/orchestrator-cli crates/orchestrator-daemon crates/orchestrator-engine
git commit -m "feat: make run conversation first"
```

### Task 6: Resume attachment and global operational commands

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: `crates/orchestrator-daemon/src/execution.rs`
- Modify: `crates/orchestrator-state/src/leases.rs`
- Test: `crates/orchestrator-cli/tests/global_resume.rs`

**Interfaces:**
- Produces: `ResumeDisposition::{Attached, Requeued, Rejected}` and streamed active-task status.
- Converts: status, pause, resume, cancel, handover, checkpoint, routing explanation, TUI commands, and provider/profile/usage mutations to daemon IPC requests.

- [ ] **Step 1: Write failing active-lease resume test**

```rust
#[test]
fn resume_attaches_when_current_daemon_owns_active_task() -> Result<()> {
    let fixture = ResumeFixture::active_task()?;
    let output = fixture.colay(["resume", fixture.task_id.as_str()])?;
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    assert!(String::from_utf8_lossy(&output.stdout).contains("attached"));
    assert_eq!(fixture.worker_attempts()?, 1);
    Ok(())
}
```

- [ ] **Step 2: Run the test and confirm RED**

Run: `cargo test -p colay --test global_resume --features test-fixtures -- --nocapture`

Expected: command fails with the current lease-conflict error.

- [ ] **Step 3: Implement attachment and complete IPC conversion**

When the active lease owner matches the healthy global daemon, return `Attached` and subscribe to task events. Only verified stale/replay-safe work returns `Requeued`. Uncertain external process ownership returns `Rejected` with the exact diagnostic and takeover command. Convert remaining ordinary operational commands to the typed IPC protocol so direct DB open remains only under maintenance entry points.

- [ ] **Step 4: Run resume and CLI suites**

Run: `cargo test -p colay --features test-fixtures -- --nocapture`

Expected: CLI integration tests pass; active resume never creates a duplicate attempt.

- [ ] **Step 5: Commit**

```text
git add crates/orchestrator-cli crates/orchestrator-daemon crates/orchestrator-state
git commit -m "fix: attach resume to active global task"
```

### Task 7: Global doctor, compatibility alias, and maintenance UX

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/args.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: `docs/operations.md`
- Modify: `docs/migrations.md`
- Test: `crates/orchestrator-cli/tests/default_startup.rs`
- Test: `crates/orchestrator-cli/tests/global_doctor.rs`

**Interfaces:**
- Produces: `doctor` state, daemon, workspace, audit, artifact, Git, runtime, and all configured provider checks.
- Produces: `compatibility` as a behavioral alias of `doctor providers`.
- Produces: idempotent `migrate apply` using maintenance ownership and never provider safe-mode authority.

- [ ] **Step 1: Write failing doctor and maintenance tests**

```rust
#[test]
fn migrate_apply_is_idempotent_with_untrusted_provider_and_missing_local_config() -> Result<()> {
    let fixture = DoctorFixture::old_global_schema_untrusted_provider()?;
    assert!(fixture.colay(["migrate", "apply"])?.status.success());
    assert!(fixture.colay(["migrate", "apply"])?.status.success());
    assert_eq!(fixture.schema_version()?, STATE_SCHEMA_VERSION);
    Ok(())
}
```

- [ ] **Step 2: Run tests and confirm RED**

Run: `cargo test -p colay --test global_doctor --features test-fixtures -- --nocapture`

Expected: old safe-mode guard rejects migration or doctor points at repository-local state.

- [ ] **Step 3: Implement global diagnostics and maintenance boundary**

Doctor resolves global state without creating repository-local files, starts or queries the daemon for fresh provider assessments, verifies per-workspace event heads and artifacts, and reports current workspace identity. Offline state repair acquires the same singleton maintenance lock before opening SQLite. Remove safe-mode checks from migration apply and keep future-schema writes disabled.

- [ ] **Step 4: Run doctor/default-startup tests**

Run: `cargo test -p colay --test global_doctor --test default_startup --features test-fixtures -- --nocapture`

Expected: all diagnostics and idempotent migration cases pass.

- [ ] **Step 5: Commit**

```text
git add crates/orchestrator-cli docs/operations.md docs/migrations.md
git commit -m "feat: diagnose and maintain user-global state"
```

### Task 8: Windows/WSL concurrency and rollout verification

**Files:**
- Create: `crates/orchestrator-cli/tests/global_concurrency.rs`
- Modify: `docs/testing.md`
- Modify: `docs/release.md`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

**Interfaces:**
- Verifies all interfaces produced by Tasks 1-7.

- [ ] **Step 1: Add concurrent-client and platform-path regressions**

```rust
#[test]
fn concurrent_clients_never_observe_sqlite_busy() -> Result<()> {
    let fixture = ConcurrencyFixture::new()?;
    let outputs = fixture.run_parallel_status_and_plan_clients(32)?;
    assert!(outputs.iter().all(|output| output.status.success()));
    assert!(outputs.iter().all(|output| {
        !String::from_utf8_lossy(&output.stderr).contains("database is locked")
    }));
    Ok(())
}
```

- [ ] **Step 2: Run focused platform-neutral stress test**

Run: `cargo test -p colay --test global_concurrency --features test-fixtures -- --nocapture`

Expected: 32 clients complete without SQLite busy or duplicate workspace/task rows.

- [ ] **Step 3: Run required repository gates**

Run: `cargo fmt --all -- --check`

Expected: exit 0.

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: exit 0 with no warnings.

Run: `cargo test --workspace --all-features`

Expected: exit 0 with all fake-provider tests passing.

- [ ] **Step 4: Run Windows-native and WSL smoke matrices**

Windows runs global path/Unicode/case, named-pipe ACL, one-daemon, non-Git plan, import, and 32-client stress cases. WSL runs XDG defaults, Unix socket ownership, non-Git home plan, independent database resolution, import, and stress cases. Both use `COLAY_HOME` temporary roots and fake providers.

- [ ] **Step 5: Update tracker and commit verified rollout evidence**

```text
git add crates/orchestrator-cli/tests/global_concurrency.rs docs/testing.md docs/release.md docs/qa/wsl-nightly-error-tracker.md
git commit -m "test: verify global state across Windows and WSL"
```

