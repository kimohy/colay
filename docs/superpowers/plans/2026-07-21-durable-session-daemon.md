# Durable Session and Repository Daemon Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the durable session, conversation, command-inbox, and repository-daemon foundation required for a reconnectable chat TUI without changing the existing task execution behavior.

**Architecture:** Provider-neutral session contracts live in `orchestrator-domain`; SQLite projections and leases live in focused `orchestrator-state` modules; a new `orchestrator-daemon` crate owns only the heartbeat/stop loop; the CLI exposes daemon lifecycle commands and starts the same binary as a detached local child. SQLite is the only durable control boundary and no network listener is introduced.

**Tech Stack:** Rust 1.95, edition 2024, Tokio, rusqlite/SQLite STRICT tables, Clap, chrono, serde, UUID v7, existing hash-chained JSONL event outbox.

## Global Constraints

- Preserve `domain <- policy/state/process/codex-compat <- providers <- engine <- daemon/cli/tui` dependency direction.
- `orchestrator-domain` remains provider-neutral and I/O-free.
- Open no TCP port and add no HTTP service, remote access, telemetry, identity rotation, quota bypass, credential access, or usage-page scraping.
- Persist only redacted message and command content; this phase accepts presentation-safe/redacted input and does not move the existing redactor into the domain.
- Keep existing task, checkpoint, worktree, handover, and event hashes readable and verifiable.
- Use Rust `Command` with separated arguments; never add shell interpolation.
- Tests invoke no real Codex, Claude, or Gemini inference.
- The daemon is repository-scoped; one unexpired daemon lease may schedule work for a repository.
- Normal daemon stop releases its lease; stale daemon takeover is allowed only at or after lease expiry.
- Phase 1 does not schedule tasks or replace the five-panel TUI. It supplies independently releasable durable infrastructure for Phase 2.
- Required final verification: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

## File Structure

| Path | Responsibility |
| --- | --- |
| `crates/orchestrator-domain/src/session.rs` | Session state, message, and client-command contracts |
| `crates/orchestrator-domain/src/ids.rs` | Typed session, message, command, and daemon instance IDs |
| `crates/orchestrator-domain/src/event.rs` | Optional session linkage on the existing audit event |
| `crates/orchestrator-state/src/paths.rs` | Trusted repository-local state path resolution |
| `migrations/0004_durable_sessions.sql` | Durable session, message, command, and daemon tables |
| `crates/orchestrator-state/src/sessions.rs` | Session and conversation-message projections |
| `crates/orchestrator-state/src/client_commands.rs` | Idempotent command submit/claim/complete/recovery |
| `crates/orchestrator-state/src/daemon_instances.rs` | Repository daemon lease, heartbeat, stop, and release records |
| `crates/orchestrator-daemon/src/lib.rs` | Tokio heartbeat/stop service loop |
| `crates/orchestrator-cli/src/daemon.rs` | Background child launch and lifecycle command application logic |
| `crates/orchestrator-cli/src/args.rs` | `colay daemon {start|serve|status|stop|restart}` parsing |
| `crates/orchestrator-cli/tests/daemon_lifecycle.rs` | Process-level daemon lifecycle contract |

## Task 1: Provider-Neutral Durable Session Contracts

**Files:**
- Create: `crates/orchestrator-domain/src/session.rs`
- Modify: `crates/orchestrator-domain/src/ids.rs`
- Modify: `crates/orchestrator-domain/src/event.rs`
- Modify: `crates/orchestrator-domain/src/schema.rs`
- Modify: `crates/orchestrator-domain/src/lib.rs`

**Interfaces:**
- Produces: `SessionId`, `MessageId`, `ClientCommandId`, `DaemonInstanceId`.
- Produces: `SessionState::validate_transition`, `ConversationMessage`, `ClientCommand`, `ClientCommandAction`, and `ClientCommandState`.
- Extends: `TaskEvent.session_id: Option<SessionId>` with absent-field serialization compatibility.

- [ ] **Step 1: Write failing domain tests**

Add tests in `session.rs` that prove allowed and rejected transitions, terminal-state immutability, non-empty session title, non-empty message content for final messages, and client-command idempotency-key validation. Add this historical serialization test in `event.rs`:

```rust
#[test]
fn absent_session_id_is_omitted_for_historical_event_hash_compatibility() -> Result<(), Box<dyn std::error::Error>> {
    let event = TaskEvent {
        schema_version: SchemaVersion::new(SchemaVersion::V3),
        sequence: 1,
        event_id: EventId::new(),
        session_id: None,
        task_id: None,
        occurred_at: Utc::now(),
        event_type: EventType::CompatibilityWarning,
        from_state: None,
        to_state: None,
        reason: None,
        actor: EventActor::System,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload: json!({}),
        previous_hash: None,
        event_hash: String::new(),
    };
    assert!(serde_json::to_value(event)?.get("session_id").is_none());
    Ok(())
}
```

- [ ] **Step 2: Run the domain test target and observe failure**

Run: `cargo test -p orchestrator-domain session`

Expected: FAIL because `session` contracts and IDs do not exist.

- [ ] **Step 3: Add typed IDs and session contracts**

Append these ID declarations in `ids.rs`:

```rust
uuid_id!(SessionId);
uuid_id!(MessageId);
uuid_id!(ClientCommandId);
uuid_id!(DaemonInstanceId);
```

Implement `session.rs` with these public shapes:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Drafting,
    Planning,
    AwaitingApproval,
    Running,
    NeedsAttention,
    Integrating,
    Verifying,
    Completed,
    Stopping,
    Cancelled,
}

impl SessionState {
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    pub fn validate_transition(self, next: Self) -> Result<(), SessionTransitionError> {
        let allowed = match self {
            Self::Drafting => matches!(next, Self::Planning | Self::Stopping),
            Self::Planning => matches!(next, Self::AwaitingApproval | Self::NeedsAttention | Self::Stopping),
            Self::AwaitingApproval => matches!(next, Self::Planning | Self::Running | Self::Stopping),
            Self::Running => matches!(next, Self::NeedsAttention | Self::Integrating | Self::Stopping),
            Self::NeedsAttention => matches!(next, Self::Planning | Self::Running | Self::Integrating | Self::Stopping),
            Self::Integrating => matches!(next, Self::NeedsAttention | Self::Verifying | Self::Stopping),
            Self::Verifying => matches!(next, Self::Completed | Self::NeedsAttention | Self::Stopping),
            Self::Stopping => next == Self::Cancelled,
            Self::Completed | Self::Cancelled => false,
        };
        if self == next {
            return Err(SessionTransitionError::NoOp(self));
        }
        allowed.then_some(()).ok_or(SessionTransitionError::NotAllowed { from: self, to: next })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole { User, Orchestrator, Agent, System }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    UserMessage, OrchestratorMessage, AgentMessage, Plan, ToolSummary,
    StateChange, ApprovalRequest, Warning, Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState { Streaming, Final, Interrupted, Rejected }

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub message_id: MessageId,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub role: MessageRole,
    pub kind: MessageKind,
    pub state: MessageState,
    pub content_redacted: String,
    pub created_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientCommandAction { CreateSession, AppendMessage, StopDaemon }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientCommandState { Pending, Claimed, Completed, Failed }

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientCommand {
    pub command_id: ClientCommandId,
    pub session_id: Option<SessionId>,
    pub task_id: Option<TaskId>,
    pub action: ClientCommandAction,
    pub payload: serde_json::Value,
    pub idempotency_key: String,
    pub state: ClientCommandState,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
}
```

Use a typed `SessionTransitionError` and constructor validation methods returning typed errors. Export the new module and types from `lib.rs`.

Add `SchemaVersion::V4` and make `state_current()` return version 4. Extend `TaskEvent` as follows and update each struct literal in the workspace with `session_id: None`:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub session_id: Option<SessionId>,
```

- [ ] **Step 4: Run domain tests**

Run: `cargo test -p orchestrator-domain`

Expected: PASS.

- [ ] **Step 5: Commit the domain contract**

```text
git add crates/orchestrator-domain
git commit -m "feat: define durable session contracts"
```

## Task 2: Shared Trusted Repository State Paths

**Files:**
- Create: `crates/orchestrator-state/src/paths.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Modify: `crates/orchestrator-cli/src/app.rs`

**Interfaces:**
- Produces: `RepositoryStatePaths::from_config(repository: &Path, config: &RootConfig) -> StateResult<Self>`.
- Preserves: the existing `.colay` confinement and symlink-escape behavior.

- [ ] **Step 1: Write path-confinement tests**

In `paths.rs`, add tests for default `.colay` paths, absolute or `..` escape rejection, and an existing symlink ancestor that leaves the repository. Assert the public fields `root`, `database`, `events`, `backups`, `tasks`, `checkpoints`, `handovers`, and `worktrees` exactly match the current CLI behavior.

- [ ] **Step 2: Run the new state test and observe failure**

Run: `cargo test -p orchestrator-state paths`

Expected: FAIL because `paths.rs` and `RepositoryStatePaths` do not exist.

- [ ] **Step 3: Move path resolution into the state crate**

Define:

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryStatePaths {
    pub root: PathBuf,
    pub database: PathBuf,
    pub events: PathBuf,
    pub backups: PathBuf,
    pub tasks: PathBuf,
    pub checkpoints: PathBuf,
    pub handovers: PathBuf,
    pub worktrees: PathBuf,
}
```

Move the current canonical-repository, lexical normalization, nearest-existing-ancestor canonicalization, and repository-prefix checks from `app.rs` into `RepositoryStatePaths::from_config`. Convert failures to `StateError::InvalidConfig` with the same actionable path detail.

In `app.rs`, import `RepositoryStatePaths as StatePaths`, remove the local `StatePaths`, `confined_local_path`, and `normalize_lexically`, and keep all existing call sites unchanged.

- [ ] **Step 4: Run focused tests**

Run these commands separately:

```text
cargo test -p orchestrator-state paths
cargo test -p colay --bin colay
```

Expected: PASS.

- [ ] **Step 5: Commit the shared paths**

```text
git add crates/orchestrator-state/src/paths.rs crates/orchestrator-state/src/lib.rs crates/orchestrator-cli/src/app.rs
git commit -m "refactor: share repository state paths"
```

## Task 3: Schema Version 4 Durable Session Migration

**Files:**
- Create: `migrations/0004_durable_sessions.sql`
- Modify: `crates/orchestrator-state/src/migrations.rs`
- Modify: `crates/orchestrator-state/tests/migration_contract.rs`

**Interfaces:**
- Produces: SQLite schema version 4.
- Preserves: migrations 1-3 checksums and historical task-event JSON.

- [ ] **Step 1: Extend migration contract tests first**

Rename `v1_to_v3_dry_run_is_non_mutating_and_apply_keeps_a_readable_backup` to `v1_to_v4_dry_run_is_non_mutating_and_apply_keeps_a_readable_backup`, expect pending versions `[2, 3, 4]`, and assert all new tables exist. Add a test that seeds a sealed version-3 `TaskEvent`, migrates, loads it through `Database::event_at`, and proves `verify_hash()` remains true.

- [ ] **Step 2: Run migration tests and observe failure**

Run: `cargo test -p orchestrator-state --test migration_contract`

Expected: FAIL because schema version 4 and its tables are absent.

- [ ] **Step 3: Add the complete migration**

Create `0004_durable_sessions.sql` with these tables and indexes:

```sql
CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY NOT NULL,
    schema_version TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    state TEXT NOT NULL CHECK (state IN (
        'drafting', 'planning', 'awaiting_approval', 'running',
        'needs_attention', 'integrating', 'verifying', 'completed',
        'stopping', 'cancelled'
    )),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    archived_at TEXT
) STRICT;

CREATE TABLE conversation_messages (
    message_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    role TEXT NOT NULL CHECK (role IN ('user', 'orchestrator', 'agent', 'system')),
    kind TEXT NOT NULL CHECK (kind IN (
        'user_message', 'orchestrator_message', 'agent_message', 'plan',
        'tool_summary', 'state_change', 'approval_request', 'warning', 'error'
    )),
    state TEXT NOT NULL CHECK (state IN ('streaming', 'final', 'interrupted', 'rejected')),
    content_redacted TEXT NOT NULL,
    created_at TEXT NOT NULL,
    finalized_at TEXT,
    UNIQUE(session_id, ordinal)
) STRICT;

CREATE TABLE client_commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN ('create_session', 'append_message', 'stop_daemon')),
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    idempotency_key TEXT NOT NULL UNIQUE CHECK (length(trim(idempotency_key)) > 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'completed', 'failed')),
    requested_by TEXT NOT NULL CHECK (length(trim(requested_by)) > 0),
    requested_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome TEXT
) STRICT;

CREATE TABLE daemon_instances (
    instance_id TEXT PRIMARY KEY NOT NULL,
    pid INTEGER NOT NULL CHECK (pid > 0),
    started_at TEXT NOT NULL,
    heartbeat_at TEXT NOT NULL,
    lease_expires_at TEXT NOT NULL,
    stop_requested_at TEXT,
    released_at TEXT
) STRICT;

CREATE UNIQUE INDEX one_unreleased_repository_daemon
    ON daemon_instances((1)) WHERE released_at IS NULL;
CREATE INDEX conversation_messages_session_ordinal
    ON conversation_messages(session_id, ordinal);
CREATE INDEX client_commands_pending
    ON client_commands(requested_at) WHERE state = 'pending';
CREATE INDEX daemon_instances_heartbeat
    ON daemon_instances(heartbeat_at DESC);

ALTER TABLE task_events
    ADD COLUMN session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT;
CREATE INDEX task_events_session_sequence ON task_events(session_id, sequence);

PRAGMA user_version = 4;
```

Set `STATE_SCHEMA_VERSION` to 4 and append `(4, "durable_sessions", include_str!(...))` to `MIGRATIONS` without modifying prior entries.

- [ ] **Step 4: Run migration and event-chain tests**

Run: `cargo test -p orchestrator-state migration && cargo test -p orchestrator-state --test migration_contract`

Expected: PASS.

- [ ] **Step 5: Commit schema version 4**

```text
git add migrations/0004_durable_sessions.sql crates/orchestrator-state/src/migrations.rs crates/orchestrator-state/tests/migration_contract.rs
git commit -m "feat: add durable session schema"
```

## Task 4: Session and Conversation Persistence

**Files:**
- Create: `crates/orchestrator-state/src/sessions.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Modify: `crates/orchestrator-state/src/database.rs`

**Interfaces:**
- Produces: `NewSessionRecord`, `StoredSession`, `SessionListFilter`, and conversation CRUD methods on `Database`.
- Consumes: Task 1 domain contracts and Task 3 tables.

- [ ] **Step 1: Write failing persistence tests**

Cover atomic session creation plus audit event, duplicate ID rejection, revision-checked transition, terminal transition rejection, per-session message ordering, task-target preservation, streaming-message finalization, and cross-session message-finalization rejection.

Use this constructor pattern:

```rust
let session = NewSessionRecord {
    session_id: SessionId::new(),
    schema_version: SchemaVersion::V1.to_owned(),
    title: "auth refactor".to_owned(),
    state: SessionState::Drafting,
    created_at: now,
};
```

- [ ] **Step 2: Run the focused tests and observe failure**

Run: `cargo test -p orchestrator-state sessions`

Expected: FAIL because the module and APIs do not exist.

- [ ] **Step 3: Implement transactional session records**

Expose these methods:

```rust
impl Database {
    pub fn create_session_with_event(&self, session: &NewSessionRecord, event: TaskEvent) -> StateResult<StoredSession>;
    pub fn load_session(&self, session_id: SessionId) -> StateResult<Option<StoredSession>>;
    pub fn list_sessions(&self, filter: &SessionListFilter) -> StateResult<Vec<StoredSession>>;
    pub fn transition_session_with_event(
        &self,
        session_id: SessionId,
        expected_revision: u64,
        next: SessionState,
        updated_at: DateTime<Utc>,
        event: TaskEvent,
    ) -> StateResult<StoredSession>;
    pub fn append_message(&self, message: &ConversationMessage) -> StateResult<u64>;
    pub fn finalize_message(
        &self,
        session_id: SessionId,
        message_id: MessageId,
        state: MessageState,
        content_redacted: &str,
        finalized_at: DateTime<Utc>,
    ) -> StateResult<ConversationMessage>;
    pub fn messages_after(&self, session_id: SessionId, ordinal: u64, limit: usize) -> StateResult<Vec<(u64, ConversationMessage)>>;
}
```

Use `append_event_in_transaction` so projection and audit event commit atomically. Require `event.session_id == Some(session.session_id)` and reject a mismatched task/session event. Allocate message ordinal with `coalesce(max(ordinal), 0) + 1` inside the same transaction. Only `Streaming` messages may finalize, and the final state must be `Final`, `Interrupted`, or `Rejected`.

Update `append_event_in_transaction` to insert the relational `session_id` column while preserving serialized event JSON.

- [ ] **Step 4: Run state session tests**

Run: `cargo test -p orchestrator-state sessions`

Expected: PASS.

- [ ] **Step 5: Commit session persistence**

```text
git add crates/orchestrator-state/src/sessions.rs crates/orchestrator-state/src/lib.rs crates/orchestrator-state/src/database.rs
git commit -m "feat: persist sessions and conversation messages"
```

## Task 5: Idempotent Client Command Inbox

**Files:**
- Create: `crates/orchestrator-state/src/client_commands.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`

**Interfaces:**
- Produces: submit, claim, complete, fail, and stale-recovery methods on `Database`.
- Consumes: `ClientCommand` domain contract and `client_commands` table.

- [ ] **Step 1: Write command-inbox tests**

Prove that identical idempotency keys return the original command, a reused key with different action/target/payload is rejected, concurrent claim has one winner, completed commands cannot be claimed, stale `CreateSession` and `AppendMessage` claims are requeued, and stale `StopDaemon` requires reconciliation rather than blind replay.

- [ ] **Step 2: Run the focused tests and observe failure**

Run: `cargo test -p orchestrator-state client_commands`

Expected: FAIL because the command inbox APIs do not exist.

- [ ] **Step 3: Implement command inbox semantics**

Expose:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ClientCommandRecoveryDisposition { StillClaimed, Requeued, ManualReconciliationRequired }

impl Database {
    pub fn submit_client_command(&self, command: &ClientCommand) -> StateResult<ClientCommand>;
    pub fn claim_next_client_command(&self, claimed_at: DateTime<Utc>) -> StateResult<Option<ClientCommand>>;
    pub fn complete_client_command(&self, command_id: ClientCommandId, outcome: &str, completed_at: DateTime<Utc>) -> StateResult<()>;
    pub fn fail_client_command(&self, command_id: ClientCommandId, outcome: &str, completed_at: DateTime<Utc>) -> StateResult<()>;
    pub fn recover_stale_client_commands(
        &self,
        stale_before: DateTime<Utc>,
    ) -> StateResult<Vec<(ClientCommand, ClientCommandRecoveryDisposition)>>;
}
```

Serialize action and state as snake-case strings. Claim the oldest pending row in an `IMMEDIATE` transaction and update with `WHERE state = 'pending'`. Compare canonical `serde_json::Value` plus action and targets when resolving an idempotency collision. Treat `StopDaemon` as manual-reconciliation-only after a stale claim.

- [ ] **Step 4: Run command tests**

Run: `cargo test -p orchestrator-state client_commands`

Expected: PASS.

- [ ] **Step 5: Commit command persistence**

```text
git add crates/orchestrator-state/src/client_commands.rs crates/orchestrator-state/src/lib.rs
git commit -m "feat: add durable client command inbox"
```

## Task 6: Repository Daemon Lease Records

**Files:**
- Create: `crates/orchestrator-state/src/daemon_instances.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`

**Interfaces:**
- Produces: `DaemonInstance`, `DaemonLeaseRequest`, `DaemonStatus`, and lease methods on `Database`.

- [ ] **Step 1: Write daemon lease tests**

Test initial acquisition, second-owner rejection, heartbeat extension, wrong-owner heartbeat rejection, stop request visibility, graceful release, stale takeover exactly at expiry, and one winner under concurrent acquisition from two database connections.

- [ ] **Step 2: Run the focused tests and observe failure**

Run: `cargo test -p orchestrator-state daemon_instances`

Expected: FAIL because daemon lease APIs do not exist.

- [ ] **Step 3: Implement daemon lease operations**

Expose:

```rust
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DaemonInstance {
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
    pub lease_expires_at: DateTime<Utc>,
    pub stop_requested_at: Option<DateTime<Utc>>,
    pub released_at: Option<DateTime<Utc>>,
}

pub struct DaemonLeaseRequest {
    pub instance_id: DaemonInstanceId,
    pub pid: u32,
    pub started_at: DateTime<Utc>,
    pub ttl: TimeDelta,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", content = "instance", rename_all = "snake_case")]
pub enum DaemonStatus {
    Stopped,
    Online(DaemonInstance),
    Stale(DaemonInstance),
}

impl Database {
    pub fn acquire_daemon_lease(&self, request: &DaemonLeaseRequest) -> StateResult<DaemonInstance>;
    pub fn heartbeat_daemon(&self, instance_id: DaemonInstanceId, heartbeat_at: DateTime<Utc>, ttl: TimeDelta) -> StateResult<DaemonInstance>;
    pub fn daemon_status(&self, now: DateTime<Utc>) -> StateResult<DaemonStatus>;
    pub fn request_daemon_stop(&self, instance_id: DaemonInstanceId, requested_at: DateTime<Utc>) -> StateResult<()>;
    pub fn daemon_stop_requested(&self, instance_id: DaemonInstanceId) -> StateResult<bool>;
    pub fn release_daemon(&self, instance_id: DaemonInstanceId, released_at: DateTime<Utc>) -> StateResult<()>;
}
```

Acquisition first marks an unreleased row with `lease_expires_at <= started_at` as released, then inserts the new row in the same transaction. Validate positive PID and TTL. Heartbeat uses ownership and unexpired-lease predicates. `daemon_status` returns `Online` for an unreleased row whose expiry is later than `now`, `Stale` for the newest unreleased expired row, and `Stopped` when no unreleased row exists.

- [ ] **Step 4: Run daemon state tests**

Run: `cargo test -p orchestrator-state daemon_instances`

Expected: PASS.

- [ ] **Step 5: Commit daemon lease state**

```text
git add crates/orchestrator-state/src/daemon_instances.rs crates/orchestrator-state/src/lib.rs
git commit -m "feat: persist repository daemon leases"
```

## Task 7: Daemon Heartbeat and Stop Runtime

**Files:**
- Create: `crates/orchestrator-daemon/Cargo.toml`
- Create: `crates/orchestrator-daemon/src/lib.rs`
- Modify: `Cargo.toml`
- Modify: `Cargo.lock`

**Interfaces:**
- Produces: `DaemonSettings`, `DaemonExit`, `DaemonError`, and `serve`.
- Consumes: Task 6 daemon lease APIs.

- [ ] **Step 1: Write Tokio runtime tests**

Use a migrated temporary database and paused or short Tokio time to prove acquisition, periodic heartbeat, explicit stop request, cancellation-token shutdown, graceful release, and second-runtime lease conflict. Do not spawn a provider process.

- [ ] **Step 2: Run the new crate tests and observe failure**

Run: `cargo test -p orchestrator-daemon`

Expected: FAIL because the workspace member does not exist.

- [ ] **Step 3: Implement the bounded service loop**

Define:

```rust
#[derive(Clone, Copy, Debug)]
pub struct DaemonSettings {
    pub heartbeat_interval: Duration,
    pub lease_ttl: TimeDelta,
}

impl Default for DaemonSettings {
    fn default() -> Self {
        Self {
            heartbeat_interval: Duration::from_secs(1),
            lease_ttl: TimeDelta::seconds(5),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DaemonExit { StopRequested, Cancelled }

pub async fn serve(
    database: &Database,
    instance_id: DaemonInstanceId,
    pid: u32,
    cancellation: CancellationToken,
    settings: DaemonSettings,
) -> Result<DaemonExit, DaemonError>
```

Acquire at `Utc::now()`, create a Tokio interval with missed-tick behavior `Delay`, and select between cancellation and ticks. On every tick, check `daemon_stop_requested`; otherwise renew the heartbeat. Always call `release_daemon` before returning a successful exit. If release fails, return that error rather than claiming a graceful exit.

Add only `chrono`, `orchestrator-domain`, `orchestrator-state`, `thiserror`, `tokio`, and `tokio-util` dependencies.

- [ ] **Step 4: Run daemon crate tests**

Run: `cargo test -p orchestrator-daemon`

Expected: PASS.

- [ ] **Step 5: Commit daemon runtime**

```text
git add Cargo.toml Cargo.lock crates/orchestrator-daemon
git commit -m "feat: run repository daemon heartbeat loop"
```

## Task 8: CLI Daemon Lifecycle Commands

**Files:**
- Create: `crates/orchestrator-cli/src/daemon.rs`
- Create: `crates/orchestrator-cli/tests/daemon_lifecycle.rs`
- Modify: `crates/orchestrator-cli/src/main.rs`
- Modify: `crates/orchestrator-cli/src/args.rs`
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/Cargo.toml`
- Modify: `README.md`

**Interfaces:**
- Produces: `colay daemon start|serve|status|stop|restart`.
- Consumes: `RepositoryStatePaths`, daemon state APIs, and `orchestrator_daemon::serve`.

- [ ] **Step 1: Write parser and process-level tests**

Add Clap tests for all five actions. In `daemon_lifecycle.rs`, create a temporary repository and isolated `COLAY_HOME`, run `colay init`, assert stopped status, start the daemon, poll status until online, assert a second start is idempotent, stop it, poll until stopped, and assert the daemon process releases its row. Use `Command` with separated args and preserve the Windows environment setup pattern from `default_startup.rs`.

- [ ] **Step 2: Run CLI tests and observe failure**

Run: `cargo test -p colay --features test-fixtures daemon`

Expected: FAIL because the command group does not exist.

- [ ] **Step 3: Add CLI command types**

Add:

```rust
#[derive(Clone, Debug, Args)]
pub struct DaemonArgs {
    #[command(subcommand)]
    pub action: DaemonAction,
}

#[derive(Clone, Copy, Debug, Subcommand)]
pub enum DaemonAction {
    Start,
    #[command(hide = true)]
    Serve,
    Status,
    Stop,
    Restart,
}
```

Add `Command::Daemon(DaemonArgs)` and dispatch it before commands that require an existing state database.

- [ ] **Step 4: Implement lifecycle behavior**

`start` initializes repository state if needed, returns the existing healthy instance when present, otherwise spawns the current executable with separated global config and `daemon serve` arguments. Redirect stdin/stdout/stderr to null. On Windows use `CREATE_NO_WINDOW`; on Unix detach stdio and allow the child to continue after the parent exits. Poll the database for up to five seconds and fail with the child status or timeout if no healthy heartbeat appears.

`serve` opens a ready database, creates a UUID v7 instance ID, installs a Ctrl-C cancellation token, and awaits `orchestrator_daemon::serve`.

`status` is read-only and reports `stopped`, `online`, or `stale` with instance ID, PID, heartbeat, and lease expiry. An absent database reports `stopped` without creating state.

`stop` requests stop for the current healthy instance and waits up to ten seconds for release; absent daemon is an idempotent success. `restart` performs stop then start and never starts until the prior row is released or expired.

Emit the existing stable JSON envelope with command names `daemon_start`, `daemon_status`, `daemon_stop`, and `daemon_restart`.

- [ ] **Step 5: Run CLI daemon tests**

Run: `cargo test -p colay --features test-fixtures daemon`

Expected: PASS with no lingering daemon child in the temporary repository.

- [ ] **Step 6: Commit CLI lifecycle**

```text
git add crates/orchestrator-cli README.md
git commit -m "feat: manage repository daemon lifecycle"
```

## Task 9: Phase 1 Documentation and Verification Gate

**Files:**
- Modify: `docs/architecture.md`
- Modify: `docs/operations.md`
- Modify: `docs/migrations.md`
- Modify: `docs/testing.md`
- Modify: `docs/threat-model.md`
- Modify: `README.md`

**Interfaces:**
- Documents: Phase 1 user commands, local-only boundary, recovery semantics, and schema version 4.

- [ ] **Step 1: Add documentation contract checks where applicable**

Extend existing README/command contract tests or add assertions in `daemon_lifecycle.rs` that `colay --help` lists `daemon` and `colay daemon --help` lists public lifecycle actions but not hidden `serve`.

- [ ] **Step 2: Update documentation**

Document:

- daemon commands and repository-local behavior in `README.md` and `operations.md`;
- the daemon application boundary and SQLite-only control plane in `architecture.md`;
- schema version 4 tables, backup, forward migration, and rollback implications in `migrations.md`;
- fake-only daemon lifecycle and crash-recovery testing in `testing.md`; and
- daemon lease spoofing, PID non-authority, local DB permissions, symlink confinement, and no-network guarantees in `threat-model.md`.

State explicitly that Phase 1 does not yet schedule task graphs and does not yet replace the current TUI.

- [ ] **Step 3: Run formatting and focused verification**

Run:

```text
cargo fmt --all -- --check
cargo test -p orchestrator-domain
cargo test -p orchestrator-state
cargo test -p orchestrator-daemon
cargo test -p colay --features test-fixtures daemon
```

Expected: every command exits 0.

- [ ] **Step 4: Run repository-wide quality gates**

Run:

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: both commands exit 0; tests use only fake provider binaries.

- [ ] **Step 5: Perform the Phase 1 completion audit**

Record direct evidence for every Phase 1 requirement: durable session round-trip, message ordering/finalization, idempotent command replay, historical event hash verification, daemon single-owner lease, start/status/stop/restart, TUI-independent daemon survival, no network listener in dependencies or runtime, and clean full-workspace gates. Any missing or indirect evidence keeps Phase 1 open.

- [ ] **Step 6: Commit Phase 1 documentation**

```text
git add README.md docs/architecture.md docs/operations.md docs/migrations.md docs/testing.md docs/threat-model.md
git commit -m "docs: describe durable session daemon"
```

## Execution Checkpoints

- **Checkpoint A — after Task 3:** review domain compatibility and migration backup/hash evidence before adding repository APIs.
- **Checkpoint B — after Task 6:** review transaction, idempotency, and daemon lease race tests before process lifecycle work.
- **Checkpoint C — after Task 8:** exercise start/status/stop/restart manually in a temporary repository and confirm no child remains.
- **Checkpoint D — after Task 9:** run the Phase 1 requirement-by-requirement audit and full workspace gates before writing the Phase 2 plan.
