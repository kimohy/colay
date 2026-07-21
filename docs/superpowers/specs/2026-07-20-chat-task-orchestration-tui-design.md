# Chat-Based Task Orchestration TUI Design

## Goal

Turn Colay's current read-mostly five-panel dashboard into a chat-first terminal workspace where one conversation can plan, run, inspect, and steer multiple durable tasks. A repository-local background daemon keeps approved work running when the TUI disconnects. The user can move between tasks, inspect live status, queue follow-up instructions, and safely integrate independently verified results.

The design preserves Colay's provider-neutral domain, official-CLI-only provider boundary, isolated writable worktrees, explicit approval gates, append-only audit trail, redaction, and fail-closed recovery behavior.

## Product Decisions

The approved product model is:

- a repository has a background coordinator daemon;
- one chat session can contain multiple tasks;
- the AI proposes a task dependency graph automatically;
- no writable task starts until the user approves the graph;
- dependency-independent, low-conflict tasks may run concurrently within configured limits;
- the primary interface is a chat-first three-pane TUI; and
- parallel results are never merged or pushed without an explicit integration approval.

## Scope

The MVP includes:

- repository-scoped daemon lifecycle and reconnectable TUI clients;
- durable sessions, messages, commands, task graphs, and daemon heartbeats;
- structured AI task decomposition and deterministic graph validation;
- dependency scheduling with global and per-provider concurrency limits;
- isolated worktrees and session-level write-scope claims for writable tasks;
- a unified conversation and activity timeline;
- task switching, focused inspection, and an attention inbox;
- task-targeted instructions applied at safe provider boundaries;
- pause, resume, cancel, retry, checkpoint, and handover controls;
- integration previews, explicit approval, overlap detection, and conflict-resolution tasks;
- daemon crash recovery and idempotent command processing; and
- responsive layouts for common terminal sizes.

The MVP does not include remote access, a web UI, multi-user collaboration, cross-repository sessions, unsafe mid-turn prompt injection, unapproved automatic merge or push, cloud artifact storage, identity or quota workarounds, or natural-language-only approval of destructive operations.

## Information Architecture

The default layout is a chat-first three-pane workspace:

```text
+ COLAY - session: auth-refactor - 3 running / 1 blocked - daemon online +
| TASK GRAPH          | CONVERSATION                  | INSPECTOR   |
|                     |                               | task-03     |
| * auth-refactor     | You                           | RUNNING     |
| |-+ task-01 schema  | Refactor the auth module      | Codex       |
| |-* task-02 API     |                               | premium/high|
| |-* task-03 tests < | Colay                         |             |
| |-! task-04 docs    | Proposed five tasks.          | progress 60%|
| `-o task-05 review  | [Expand task graph]           | elapsed 4m  |
|                     |                               | files 3     |
| ATTENTION           | task-03 - Codex               |             |
| 1 conflict          | * cargo test                  | DEPENDENCIES|
| 1 approval          | `- 31 passed, 1 failed        | task-01 done|
+---------------------+-------------------------------+-------------+
| to: [orchestrator v]  Message or /command...             Enter send|
+-------------------------------------------------------------------+
```

The panes have distinct responsibilities:

- **Task Graph** shows graph hierarchy, task state, dependency waiting, and items needing attention.
- **Conversation** combines user messages, orchestrator plans, agent replies, folded tool summaries, approvals, and lifecycle changes in chronological order.
- **Inspector** shows the selected task's provider, profile, elapsed time, dependencies, worktree, changed files, verification, and queued instructions.
- **Composer** always displays an explicit target and supports messages and command-palette actions.

Selecting a task changes the viewed task and inspector only. It does not silently retarget the composer. The composer defaults to `orchestrator`; users explicitly choose `task-<id>` or `all running`, or use `@task-03` and `@all` prefixes.

## Primary User Flow

```text
goal message
  -> structured graph proposal
  -> deterministic validation
  -> approval card
  -> dependency-aware parallel execution
  -> live activity and attention updates
  -> task verification
  -> integration preview
  -> explicit integration approval
  -> session verification and completion
```

The approval card shows task order, dependencies, proposed providers and profiles, write scopes, concurrency, and risks. The user can approve, edit, or cancel it. Editing creates a new graph revision; the old revision remains auditable.

The session can be replanned after execution begins. A revision that only narrows future work may be approved normally. A revision that broadens write scope, introduces a destructive action, invalidates completed dependencies, or supersedes running work requires a fresh explicit approval and safe checkpointing of affected tasks.

## Navigation and Commands

The core bindings are:

```text
Tab / Shift+Tab     move between panes
j / k               move within the active pane
Enter               select, expand, or submit
Esc                 close overlay or return
Ctrl+P              open task quick switcher
Ctrl+O              open orchestration overview
Ctrl+L              open the selected task's full log
Ctrl+T              choose the composer target
Ctrl+Space          request pause or resume
/                   open command palette
?                   show contextual help
```

The command palette exposes `/tasks`, `/plan`, `/integrate`, `/approve`, `/resolve`, `/pause`, `/resume`, `/cancel`, `/handover`, `/retry`, `/checkpoint`, and `/provider`. Destructive or scope-expanding commands open typed confirmation views rather than executing from the text alone.

## Responsive Behavior

- At 110 columns or wider, all three panes are visible.
- At 80-109 columns, the inspector is an overlay reached from the selected task.
- At 60-79 columns, conversation is primary and Task Graph and Inspector become switchable full views.
- Below 60 columns, Colay keeps execution running and renders a safe compact status plus a terminal-width recommendation.

The UI must not rely on color alone. State symbols, labels, focus borders, and text remain meaningful with color disabled.

## Architecture

Colay retains its one-way dependency direction and adds a daemon at the application boundary:

```text
domain <- policy/state/process/codex-compat <- providers <- engine <- daemon/cli/tui
```

The major components are:

```text
colay tui / colay CLI
  -> durable SQLite command inbox
  -> colay daemon
       Session Manager
       Task Planner
       DAG Scheduler
       Task Executors
       Integration Coordinator
  -> existing engine/providers/process/state/domain
```

Responsibilities are divided as follows:

- `orchestrator-domain` defines provider-neutral sessions, messages, graph revisions, dependencies, instructions, and transition rules. It stays I/O-free.
- `orchestrator-state` persists commands, conversation projections, graph projections, daemon heartbeats, and resource claims while preserving the existing event outbox.
- `orchestrator-engine` owns the structured planner contract, reusable task execution, graph scheduling decisions, and integration planning.
- a new `orchestrator-daemon` crate owns background lifecycle, repository lease acquisition, scheduling loops, recovery, and concurrency.
- `orchestrator-tui` owns presentation models, rendering, input state, navigation, and typed command submission. It does not own orchestration decisions.
- `orchestrator-cli` owns command parsing, daemon bootstrap, noninteractive administration, and compatibility with existing commands.

The current task execution path in the CLI application layer is extracted behind an engine `TaskExecutor` interface. Both the daemon and compatibility-preserving foreground `colay run` path use the same implementation.

## Daemon Lifecycle

`colay tui` checks the repository daemon heartbeat. If no healthy instance exists, it starts `colay daemon serve` as a hidden background process and waits for a valid lease and heartbeat before enabling mutations. The daemon is repository-scoped and holds a single active daemon lease. A second process may inspect state but cannot schedule workers.

The design opens no TCP port and exposes no HTTP service. SQLite remains the durable local command and projection boundary. The TUI reads only rows after its last observed sequence, normally every 100-250 milliseconds. This is fast enough for interactive status while avoiding a second cross-platform IPC protocol in the MVP.

The daemon records its instance ID, process ID, start time, heartbeat, and lease expiry. `colay daemon status`, `stop`, and `restart` expose explicit lifecycle controls. A normal stop checkpoints workers at safe boundaries before releasing leases. An unclean stop is recovered through lease expiry and the existing conservative control-recovery rules.

Closing or crashing the TUI has no effect on worker execution. A new TUI reconstructs its view from durable sessions, final messages, partial-message projections, tasks, attempts, and events.

## Task Planning and Graph Validation

The planner must return a versioned, vendor-neutral `TaskGraphProposal`, not unconstrained prose. Each node contains:

- stable proposal-local task key;
- title and objective;
- dependency keys;
- constraints and acceptance criteria;
- proposed logical model profile;
- declared repository-relative write-scope prefixes;
- risk classification; and
- a parallel-safety explanation.

Before showing an approval action, Colay validates that:

- the graph is acyclic;
- every dependency resolves;
- all paths are normalized repository-relative paths;
- write scopes are not empty for writable work unless repository-wide scope is explicit;
- concurrently eligible tasks have disjoint write scopes;
- risky actions have an approval requirement;
- all selected providers and profiles are eligible; and
- configured global and per-provider concurrency are valid.

Invalid proposals remain visible as failed plan attempts but cannot create writable workers. Colay may ask the planner for a corrected proposal with deterministic validation errors.

## Scheduling and Parallelism

A queued task is ready only when:

```text
all dependencies completed and verified
+ graph revision is current
+ required approvals exist
+ write-scope claims can be acquired
+ an eligible provider exists
+ global and provider concurrency slots are available
```

The default global concurrency is three. Provider-specific limits can lower effective concurrency. The scheduler uses fair ordering by readiness time and graph order; attention or retry work cannot starve newer independent tasks indefinitely.

Every writable task receives its own task branch, worktree, coordinator lease, and writable worker lease. Session-level resource claims reserve declared path prefixes before execution. A runtime checkpoint that discovers changes outside the declared scope blocks the task and requests approval rather than silently widening ownership.

Read-only planner and reviewer work does not acquire writable path claims, but remains bounded by its own concurrency configuration.

## Result Integration

Parallel worktrees remain isolated after task completion. Colay creates an integration preview from sealed checkpoints, verified diffs, exact changed-file sets, and content hashes. It identifies clean results, path overlaps, stale bases, failed verification, and missing evidence.

No result is applied automatically. After the user approves a preview hash, Colay creates or reuses a dedicated integration worktree and applies only the approved, integrity-matching results in deterministic dependency order. If the preview changed after approval, the approval is invalid and a new preview is required.

An overlap, patch failure, or stale dependency stops integration. Colay offers to create a conflict-resolution task with the relevant checkpoint evidence. The resolution result is independently verified and requires a final application approval. Merge to the user's branch, push, worktree deletion, and remote publication remain outside this feature.

## State Models

Session state is separate from existing task state:

```text
Drafting -> Planning -> AwaitingApproval -> Running
                                      Running <-> NeedsAttention
                                      Running -> Integrating
                                      Integrating -> Verifying -> Completed
```

Cancellation uses `Stopping -> Cancelled` after safe checkpoint handling. Daemon connectivity is operational metadata, not a session-state transition.

The existing `TaskState` enum remains authoritative for individual executions. Scheduler readiness is a projection over `Queued` tasks rather than a proliferation of persisted task states. Examples include waiting for dependencies, waiting for a concurrency slot, or blocked by a path claim.

## Persistence Model

A sequential migration adds:

```text
sessions
session_tasks
task_dependencies
conversation_messages
client_commands
task_instructions
resource_claims
daemon_instances
integration_batches
```

`sessions` stores the title, state, active graph revision, and timestamps. `session_tasks` and `task_dependencies` preserve graph revisions and display order. `conversation_messages` stores redacted user, orchestrator, agent, plan, tool-summary, state-change, approval, warning, and error messages. `client_commands` carries an idempotency key plus pending, claimed, and completed state. `task_instructions` tracks queued, applied, rejected, and interrupted delivery. `resource_claims` represents session-level path-prefix ownership. `integration_batches` binds a preview hash, source checkpoints, approval, and application outcome.

Existing task, attempt, worktree, checkpoint, handover, verification, approval, and artifact tables remain authoritative for their current concepts.

`task_events` gains an optional relational `session_id`. New domain serialization must omit an absent session ID so historical event hashes remain reproducible. New events use a bumped supported schema version, while readers continue to accept and verify existing events. Session and task events share the existing single hash-chained `events.jsonl` audit replica.

## Commands and Event Flow

All state-changing clients submit durable commands:

```text
client command with idempotency key
  -> daemon atomically claims it
  -> input is validated and redacted
  -> lifecycle event is appended
  -> projection is updated
  -> command completion is recorded
  -> TUI observes the next sequence
```

Commands never authorize an action solely from free-form display text. Typed action, target, scope, preview hash, and approval identity remain separate fields.

Provider streaming text is accumulated in a bounded mutable projection for live display. At the terminal provider event it becomes one final durable message. If the daemon fails, the partial message remains visible with `interrupted` status. The append-only audit records message start, completion or interruption, message ID, and final content hash instead of every token delta.

Full stdout and stderr are bounded artifacts. Conversation entries store concise summaries and artifact references, preventing unbounded duplication in SQLite.

## Task-Targeted Instructions

A message to a running task creates a durable queued instruction. Unless a provider adapter proves a safe public mid-turn input boundary, Colay waits for the current provider invocation to terminate, creates a checkpoint, and includes the instruction in the next invocation. The TUI displays `queued`, `applying`, `applied`, or `rejected` state.

An explicit interrupt remains a higher-risk control. It first attempts a safe checkpoint and follows existing process-tree termination rules. An unconfirmed process-tree termination blocks replacement writers and retains the lease.

## Error Handling and Recovery

- **TUI disconnect:** the daemon continues; reconnect resumes after the client's last observed sequence.
- **Daemon crash:** the TUI becomes read-only, shows a stale-heartbeat banner, and reconnects after a replacement daemon acquires the expired lease and reconciles work.
- **Provider crash:** after confirmed termination, Colay captures a recovery checkpoint and blocks, retries, or proposes handover according to policy.
- **Quota failure:** missing budget remains unknown; a safe destination handover may be proposed but no quota workaround is attempted.
- **Database contention:** clients use bounded backoff. The idempotency key prevents duplicate command effects.
- **Write-scope violation:** the task is checkpointed and blocked pending explicit scope approval.
- **Cross-task overlap:** integration stops and proposes a conflict-resolution task.
- **Audit or schema failure:** the daemon enters safe mode and disables writable execution.
- **Stale claimed command:** restart-safe commands may be requeued; ambiguous commands require manual reconciliation.

Restart-safe commands include message persistence, focus changes, pause, resume, and cancel. Task creation, plan approval, and retry require state reconciliation before replay. Handover finalization, integration application, and usage override are never blindly replayed.

## Approval Boundaries

Explicit approval is required for:

- initial graph execution;
- a graph revision that broadens scope or supersedes running work;
- a change outside declared write scope;
- destructive actions already covered by Colay policy;
- application of parallel results to the integration worktree;
- a changed integration preview hash; and
- application of conflict-resolution output.

Approval records bind the exact typed action, graph revision or preview hash, scope, identity, and time. Displayed chat text is never itself an approval record.

## Security and Privacy

All persisted messages and command payloads pass through the existing redaction boundary. Provider credentials and authentication stores are never read. Provider inference continues only through approved official CLIs and separated process arguments. The daemon is local-only, repository-scoped, and does not add default telemetry.

Artifacts remain bounded, content-addressed, and repository-local. Presentation snapshots contain only redacted strings and identifiers safe for terminal display. The daemon validates every path against the trusted repository and worktree before acquiring a claim or applying an integration result.

## Delivery Phases

### Phase 1: Durable Session

Add session, message, command, and daemon state; implement daemon lease, heartbeat, lifecycle controls, and reconnectable projection reads.

### Phase 2: Chat-First TUI

Replace the fixed dashboard flow with the responsive three-pane shell, conversation timeline, task quick switcher, explicit composer target, attention inbox, and command palette. Existing provider/profile administration remains accessible through overlays or commands.

### Phase 3: Planning and DAG

Add the structured planner contract, graph validation, graph revisions, approval cards, dependency readiness, and sequential scheduling compatibility.

### Phase 4: Parallel Execution

Add global and per-provider semaphores, session resource claims, concurrent isolated worktrees, live activity projection, and safe-boundary task instructions.

### Phase 5: Integration and Recovery

Add sealed integration previews, approval-bound application, conflict-resolution tasks, daemon crash recovery, and end-to-end recovery coverage.

Each phase must be independently releasable. Phases 1 and 2 support chat over existing single-task execution. Phase 3 enables multi-task orchestration. Phase 4 enables bounded concurrency. Phase 5 completes the approved parallel-result workflow.

This document is the umbrella product and architecture design, not a single implementation batch. Each phase receives its own implementation plan, verification gate, and review before the next phase starts. The first implementation plan covers Phase 1 only; later plans must treat the persisted contracts and acceptance evidence from completed phases as inputs rather than silently revising them.

## Performance and Accessibility Targets

- Local key input should repaint within 50 milliseconds under normal load.
- A committed daemon event should appear in the TUI within 500 milliseconds.
- A 1,000-message session should remain responsive through indexed pagination and folded or virtualized rendering.
- Raw mode and alternate screen must be restored after normal exit, resize, suspend, I/O failure, or panic handling under the supported terminal contract.
- The layout must remain safe at 60 by 20 cells and degrade according to the responsive rules.
- Windows Terminal, macOS Terminal, and representative Linux terminals are supported.

## Testing

Tests use only `orchestrator-test-support` fake provider binaries. CI never invokes real Codex, Claude, or Gemini inference.

Unit tests cover session transitions, graph validation and cycle detection, path-scope intersection, scheduler readiness, fairness, command idempotency, instruction delivery state, and responsive layout computation.

State integration tests cover migration from the current schema, historical event-hash verification, graph revision persistence, concurrent command claiming, daemon lease competition, stale command recovery, partial message recovery, and approval-hash binding.

Engine integration tests cover dependency order, maximum global and provider concurrency, path-claim exclusion, runtime scope violations, safe-boundary instruction application, provider failure and handover, integration preview determinism, stale preview rejection, and conflict detection.

TUI tests use `ratatui::TestBackend` snapshots at 60, 80, 110, and 160 columns. They cover pane traversal, quick task switching, composer-target preservation, scrolling and folding, approval overlays, disconnected read-only mode, and terminal guard restoration.

End-to-end tests cover:

- goal to proposal to approval to parallel execution to verification;
- TUI exit and reconnect while tasks continue;
- daemon termination and durable recovery;
- provider failure followed by safe handover;
- task-targeted instruction delivery after a checkpoint;
- write-scope violation and explicit expansion approval;
- parallel file overlap and conflict-resolution creation; and
- session cancellation after safe checkpoints.

The repository-wide completion gate remains:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Acceptance Criteria

1. A user can submit one natural-language goal and receive a validated task graph.
2. No writable worker starts before graph approval.
3. Independent tasks execute concurrently within configured limits.
4. Closing the TUI does not stop the daemon or running tasks.
5. A new TUI restores the session, conversation, graph, and current task status.
6. Any task is reachable in at most three navigation actions from the main view.
7. A task-targeted message cannot be delivered to another task.
8. Results with overlapping files are not integrated without resolution and approval.
9. A daemon crash does not cause a command or integration batch to execute twice.
10. Historical tasks, checkpoints, and event hashes remain readable and verifiable.
11. The full formatting, lint, and workspace test gates pass.
12. The complete end-to-end workflow is reproducible without real provider inference.

## Documentation Impact

Implementation updates must revise the command reference, architecture, operations, threat model, migrations, testing, and rollback documentation. The README should introduce `colay tui` as the primary interactive entry point while retaining existing noninteractive CLI workflows.
