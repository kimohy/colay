# Approved Task Graph Planning Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Convert a durable chat goal into a versioned, deterministically validated task DAG that creates executable task records only after explicit approval of the exact proposal hash.

**Architecture:** Provider-neutral graph contracts and deterministic validation live in `orchestrator-domain`; `orchestrator-engine` owns a read-only planner boundary; SQLite stores immutable revisions, validation evidence, membership, and approvals. The daemon processes typed commands without suspending heartbeats, while the TUI renders plan cards and typed confirmation without treating chat text as authority.

**Tech Stack:** Rust 2024, Tokio, async-trait, Serde/JSON, SHA-256 canonical sealing, SQLite/rusqlite, Ratatui/crossterm, official-CLI provider adapters, fake provider fixtures.

## Global Constraints

- Preserve `domain <- policy/state/process/codex-compat <- providers <- engine <- daemon/cli/tui`; domain remains I/O-free and vendor-neutral.
- Planning is read-only through an approved official CLI. Tests use fake adapters/binaries only.
- No task, worktree, lease, or writable invocation exists before an exact graph revision/hash approval.
- Paths are validated `RepoPath` values; repository-wide write scope is explicit.
- Invalid revisions remain auditable and visible but cannot be approved.
- Revisions, approvals, and events are append-only; historical event hashes remain valid.
- Redact before persistence; revision, hash, scope, and approval identity remain typed fields.
- Phase 3 ends with approved queued tasks. Scheduling and parallel workers begin in Phase 4.

---

## Task 1: Provider-Neutral Graph Contract

**Files:**
- Create: `crates/orchestrator-domain/src/graph.rs`
- Modify: `crates/orchestrator-domain/src/ids.rs`
- Modify: `crates/orchestrator-domain/src/lib.rs`

**Interfaces:** Produces `GraphRevisionId`, `PlanningAttemptId`, `TaskGraphProposal`, `TaskGraphNode`, `GraphValidationPolicy`, `ValidatedTaskGraph`, `GraphValidationError`, and `validate_task_graph`.

- [ ] **Step 1: Write failing validation tests** for a diamond DAG, duplicate/blank keys, missing dependencies, self-edge, cycle path, empty scope, explicit repository scope, provider/profile eligibility, invalid concurrency, independent prefix overlap, and dependency-ordered scope reuse.

```rust
#[test]
fn independent_overlapping_scopes_are_rejected() {
    let proposal = proposal([
        node("api", [], ["src/api"]),
        node("tests", [], ["src/api/tests"]),
    ]);
    assert!(validate_task_graph(proposal, &policy()).is_err());
}
```

- [ ] **Step 2: Run** `cargo test -p orchestrator-domain graph`; expect compile failure.
- [ ] **Step 3: Implement the exact contract.**

```rust
pub struct TaskGraphProposal {
    pub schema_version: SchemaVersion,
    pub revision_id: GraphRevisionId,
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub planner_provider: ProviderId,
    pub proposed_at: DateTime<Utc>,
    pub nodes: Vec<TaskGraphNode>,
}

pub struct TaskGraphNode {
    pub key: String,
    pub title: String,
    pub objective: String,
    pub dependencies: Vec<String>,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub provider: Option<ProviderId>,
    pub profile: ModelProfile,
    pub write_scopes: Vec<RepoPath>,
    pub repository_wide_write_scope: bool,
    pub risks: Vec<RiskTag>,
    pub parallel_safety: String,
}
```

Use deterministic error order and component-aware prefix overlap. Seal canonical proposal JSON plus validation summary with SHA-256. Never repair a key, path, provider, or dependency silently.

- [ ] **Step 4: Verify** `cargo test -p orchestrator-domain graph && cargo clippy -p orchestrator-domain --all-targets -- -D warnings`.
- [ ] **Step 5: Commit** `feat: validate provider-neutral task graphs`.

## Task 2: Immutable Graph Persistence and Schema v6

**Files:**
- Create: `migrations/0006_approved_task_graphs.sql`
- Create: `crates/orchestrator-state/src/graphs.rs`
- Modify: `crates/orchestrator-state/src/{lib,migrations}.rs`
- Modify: `crates/orchestrator-state/tests/migration_contract.rs`

**Interfaces:** Produces `GraphRevisionStatus`, `StoredGraphRevision`, `GraphProjection`, `record_graph_attempt`, `load_graph_revision`, `current_graph`, and `approve_graph_and_materialize_tasks`.

- [ ] **Step 1: Write failing tests** proving v5->v6 backup/dry-run, immutable revisions, invalid-attempt retention, session isolation, exact-hash approval, idempotent replay, stale/wrong-hash rejection, and atomic task/dependency/approval/event creation. Inject one FK failure and prove zero partial rows.
- [ ] **Step 2: Run** `cargo test -p orchestrator-state graphs`; expect failure.
- [ ] **Step 3: Add strict v6 tables** `graph_revisions`, `planning_attempts`, `session_tasks`, `task_dependencies`, `session_graph_heads`, and `graph_approvals`. Rebuild `client_commands` in one migration to preserve all rows/indexes while extending actions with `request_plan`, `approve_graph`, `revise_graph`, and `cancel_plan`. Set `PRAGMA user_version = 6`.

```sql
CREATE TABLE graph_revisions (
  revision_id TEXT PRIMARY KEY NOT NULL,
  session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
  ordinal INTEGER NOT NULL CHECK (ordinal > 0),
  status TEXT NOT NULL CHECK (status IN
    ('planning','invalid','awaiting_approval','approved','superseded','cancelled')),
  proposal_hash TEXT CHECK (proposal_hash IS NULL OR length(proposal_hash) = 64),
  proposal_json TEXT CHECK (proposal_json IS NULL OR json_valid(proposal_json)),
  validation_json TEXT NOT NULL CHECK (json_valid(validation_json)),
  planner_provider TEXT,
  created_at TEXT NOT NULL,
  completed_at TEXT,
  UNIQUE(session_id, ordinal)
) STRICT;
```

- [ ] **Step 4: Implement approval materialization.** Reload and decode the sealed proposal inside the transaction, compare the exact hash, create `TaskEnvelope` rows in display order, resolve keys to IDs, write dependencies/approval/head, supersede older heads, and append graph/task/message events. Exact replay returns the existing result; mismatch fails closed.
- [ ] **Step 5: Verify** state graph, migration-contract, and clippy commands; commit `feat: persist approved task graph revisions`.

### Checkpoint E

Review path overlap, the command-table rebuild, historical hashes, and the zero-task-before-approval invariant before continuing.

## Task 3: Read-Only Planner Engine Boundary

**Files:**
- Create: `crates/orchestrator-engine/src/planner.rs`
- Modify: `crates/orchestrator-engine/src/lib.rs`
- Modify: `crates/orchestrator-engine/Cargo.toml`

**Interfaces:** Produces `TaskPlanner`, `PlannerRequest`, `PlannerResponse`, `PlannerFailure`, and `collect_planner_response`.

- [ ] **Step 1: Write failing tests** for one valid JSON object, fenced/prose/multiple JSON rejection, 1 MiB limit, lifecycle error/quota/crash, wrong session/message identity, non-read-only request, and future schema.
- [ ] **Step 2: Run** `cargo test -p orchestrator-engine planner`; expect failure.
- [ ] **Step 3: Implement the async boundary and strict collector.**

```rust
#[async_trait]
pub trait TaskPlanner: Send + Sync {
    async fn propose(&self, request: PlannerRequest)
        -> Result<PlannerResponse, PlannerFailure>;
}

pub struct PlannerRequest {
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub goal_redacted: String,
    pub repository_summary_redacted: String,
    pub validation_policy: GraphValidationPolicy,
}
```

Accept exactly one final JSON object, retain bounded redacted evidence, and delegate all semantic checks to domain validation.

- [ ] **Step 4: Verify** engine planner tests/clippy; commit `feat: define structured task planner boundary`.

## Task 4: Official-CLI Planner Adapter

**Files:**
- Create: `crates/orchestrator-cli/src/task_planner.rs`
- Modify: `crates/orchestrator-cli/src/main.rs`
- Test: `crates/orchestrator-cli/tests/chat_plan_fake_provider.rs`

**Interfaces:** Produces `OfficialCliTaskPlanner::from_config` and a `TaskPlanner` implementation using existing provider adapters and `ProcessAdapterRuntime`.

- [ ] **Step 1: Write failing fake-process tests** proving `SandboxMode::ReadOnly`, separated argv, configured profile/model/effort, bounded timeout/output, no worktree allocation, valid proposal parsing, no eligible provider failure, and malformed output failure.
- [ ] **Step 2: Run** `cargo test -p colay --features test-fixtures chat_plan_fake_provider`; expect failure.
- [ ] **Step 3: Implement policy-backed selection** using existing startup capability evidence. Build a read-only `WorkerRequest`, require only proposal JSON, drain normalized events, wait for confirmed exit, and pass bounded output to the engine collector. Provider name alone never proves capability.
- [ ] **Step 4: Verify** fake planner tests and CLI clippy; commit `feat: plan task graphs through read-only official clis`.

## Task 5: Durable Planning and Approval Commands

**Files:**
- Modify: `crates/orchestrator-domain/src/session.rs`
- Modify: `crates/orchestrator-state/src/client_commands.rs`
- Create: `crates/orchestrator-daemon/src/planning.rs`
- Modify: `crates/orchestrator-daemon/src/{commands,lib}.rs`

**Interfaces:** Produces `RequestPlanCommandPayload`, `ApproveGraphCommandPayload`, `PlanningServices`, and async `process_next_orchestration_command`.

- [ ] **Step 1: Write failing tests** for goal ownership, Drafting->Planning->AwaitingApproval, invalid proposal attention, typed approval, wrong/stale hashes, one-time task creation, crash replay, and a slow fake planner that does not interrupt daemon heartbeats.

```rust
pub struct RequestPlanCommandPayload { pub goal_message_id: MessageId }
pub struct ApproveGraphCommandPayload {
    pub revision_id: GraphRevisionId,
    pub proposal_hash: String,
    pub approved_by: String,
}
```

- [ ] **Step 2: Run** `cargo test -p orchestrator-daemon planning`; expect failure.
- [ ] **Step 3: Implement planning job ownership.** Persist an attempt before spawning a Tokio planner task. Keep heartbeat/stop branches active while it runs. Completion stores a validated or invalid revision plus redacted timeline entry. Cancellation aborts the read-only process and leaves a reconcilable attempt. Approval is one short SQLite transaction.
- [ ] **Step 4: Verify** domain session, state graph/command, daemon planning tests; commit `feat: process durable graph planning commands`.

### Checkpoint F

Review planner cancellation, heartbeat continuity, crash reconciliation, redaction, and the exact approval transaction.

## Task 6: Graph-Aware Workspace Projection

**Files:**
- Modify: `crates/orchestrator-state/src/workspace.rs`
- Modify: `crates/orchestrator-tui/src/chat/model.rs`
- Modify: `crates/orchestrator-cli/src/chat_tui.rs`

**Interfaces:** Produces graph-aware `WorkspaceTask`, dependency rows, invalid-plan attention, and `PlanApprovalCard`.

- [ ] **Step 1: Write failing tests** with two sessions/revisions proving current-session isolation, display order, relational dependency labels, full approval data, invalid errors without an approvable hash, and recent-task fallback only when no graph exists.
- [ ] **Step 2: Run** `cargo test -p orchestrator-state workspace_graph`; expect failure.
- [ ] **Step 3: Extend the single-lock projection** with bounded current graph joins.

```rust
pub struct PlanApprovalCard {
    pub revision_id: String,
    pub proposal_hash: String,
    pub nodes: Vec<PlanNodeSummary>,
    pub proposed_parallelism: usize,
    pub risks: Vec<String>,
}
```

Map redacted fields only and never recover authority by parsing display strings.

- [ ] **Step 4: Verify** state workspace, TUI model, and driver tests; commit `feat: project approved session task graphs`.

## Task 7: TUI Plan Card and Typed Approval

**Files:**
- Modify: `crates/orchestrator-tui/src/chat/{input,state,render,runtime}.rs`
- Modify: `crates/orchestrator-cli/src/chat_tui.rs`

**Interfaces:** Produces `WorkspaceAction::RequestPlan`, `WorkspaceAction::ApproveGraph`, and `Overlay::ApprovalConfirmation`.

- [ ] **Step 1: Write failing tests** proving `/plan` chooses the newest eligible user goal; `/approve` shows revision/hash/tasks/dependencies/scopes/providers/profiles/risks/concurrency; only `y` confirms; Esc/n cancels; changed hash closes a stale overlay; invalid/compact/offline states block approval; graph selection never retargets composer.

```rust
WorkspaceAction::ApproveGraph {
    revision_id: String,
    proposal_hash: String,
    approved_by: String,
}
```

- [ ] **Step 2: Run** TUI chat and CLI chat tests; expect failure.
- [ ] **Step 3: Implement graph connectors, plan card, and typed confirmation.** The driver submits exact UUID-v7 durable commands. Free-form “yes” is just a message. Approved tasks remain `Queued` for Phase 4.
- [ ] **Step 4: Verify** full TUI and fake CLI chat-plan tests; commit `feat: approve exact task graph revisions in chat`.

## Task 8: End-to-End Phase 3 Process Test

**Files:**
- Create: `crates/orchestrator-cli/tests/chat_plan_approval.rs`
- Modify: `crates/orchestrator-cli/src/bin/fake-provider-cli.rs`

**Interfaces:** Proves goal -> proposal -> validation -> exact approval through the real daemon and fake official CLI.

- [ ] **Step 1: Write a failing isolated-repository test.** Submit goal/request-plan; wait for AwaitingApproval; inspect graph; assert zero tasks/worktrees/worker leases; reject wrong hash; approve exact hash; assert queued tasks/dependencies created once; reconnect via a second database; assert one read-only planner invocation; stop daemon and leave no child.
- [ ] **Step 2: Run** `cargo test -p colay --features test-fixtures --test chat_plan_approval -- --nocapture`; expect failure.
- [ ] **Step 3: Wire `OfficialCliTaskPlanner` into hidden daemon serve** and recover stale planning attempts before claiming more work. Expose failures as redacted final timeline/attention entries.
- [ ] **Step 4: Verify** process and daemon tests; commit `test: prove approved chat task graph planning`.

### Checkpoint G

Inspect fake-provider argv and SQLite before/after approval. Zero writable artifact before approval is mandatory.

## Task 9: Phase 3 Documentation and Audit

**Files:**
- Modify: `README.md`, `docs/{architecture,operations,testing,threat-model,migrations,release}.md`
- Create: `docs/superpowers/audits/2026-07-21-approved-task-graph-phase3.md`

**Interfaces:** Documents exact planning/approval semantics and the Phase 4 boundary.

- [ ] **Step 1: Add failing docs assertions** for `/plan`, `/approve`, exact proposal hash, read-only planner, “no writable task before approval”, invalid proposals, and approved tasks queued until Phase 4.
- [ ] **Step 2: Update docs** with plan cards, keys, revision history, replan semantics, provider selection, fake-only testing, v6 rebuild/backups, crash reconciliation, and non-authoritative chat text.
- [ ] **Step 3: Run focused gates:** fmt; domain graph/session; state graphs/workspace/migrations; engine planner; daemon planning; full TUI; fake CLI chat-plan/chat-tui/daemon; diff check.
- [ ] **Step 4: Run repository gates** `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo test --workspace --all-features`.
- [ ] **Step 5: Record direct evidence** for deterministic validation, invalid retention, read-only planning, heartbeat continuity, zero pre-approval writes, wrong-hash rejection, exact idempotent approval, accessibility, reconnect, no listener, no real provider, and full gates.
- [ ] **Step 6: Commit** `docs: describe approved task graph planning`.

### Checkpoint H

Review the Phase 3 audit and fresh workspace gates before Phase 4 scheduling.

## Self-Review

- Tasks 1-3 cover structured proposals and deterministic validation; Tasks 4-5 cover official-CLI planning and durable commands; Tasks 6-7 cover graph UI and typed approval; Task 8 proves the process boundary; Task 9 closes docs/audit.
- Only `approve_graph_and_materialize_tasks` creates tasks, binding session, revision, hash, identity, and time atomically.
- Phase 3 does not claim concurrency, worktree dispatch, task-target instruction delivery, or integration.
- `GraphRevisionId`, proposal hash, approval payload, card, and workspace action carry identical typed identity across layers.
- The plan contains no deferred implementation placeholders.
