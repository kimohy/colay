# Phase 4 implementation plan: parallel task execution

Date: 2026-07-21  
Input boundary: Phase 3 approved graph revisions and queued session tasks  
Output boundary: independently verified task worktrees retained for Phase 5 integration

## Non-negotiable boundary

- Only tasks from the current approved graph may be scheduled.
- A task is ready only after every dependency is `Completed`, its resource claims can be acquired, and both global and provider limits have capacity.
- Every writable worker uses its own managed worktree, coordinator lease, writable worker lease, and session resource claims.
- Provider CLIs are invoked only through existing official-CLI adapters with separated argv and bounded process I/O. Tests configure only compiled fake binaries.
- Runtime changes outside declared scopes checkpoint and block the task; claims are never widened implicitly.
- Task-targeted chat is durable instruction input. It cannot be delivered to another task and is applied only at a safe provider boundary.
- Phase 4 verifies each task independently but never integrates, merges, pushes, publishes, or deletes a worktree.

## Task 1: provider-neutral scheduling contracts

**Files:**
- Create `crates/orchestrator-domain/src/scheduling.rs`
- Modify `crates/orchestrator-domain/src/{lib,ids,state}.rs`

**Test first:** diamond readiness, current-revision exclusion, failed/blocked dependency exclusion, deterministic ready order, global/provider capacity, component-aware claim overlap, repository-wide exclusion, fairness tie-breaks, and instruction transitions.

Add `InstructionId`, `ResourceClaimId`, `ScheduleCandidate`, `ScheduleCapacity`, `ReadinessBlocker`, `ResourceScope`, and `TaskInstructionState` (`Queued`, `Applying`, `Applied`, `Rejected`, `Interrupted`). Keep all selection and overlap logic I/O-free and deterministic. Export the component-aware path-overlap primitive already used by graph validation rather than duplicating string-prefix rules.

Commit: `feat: define parallel scheduling contracts`

## Task 2: configuration limits and schema v7

**Files:**
- Create `migrations/0007_parallel_execution.sql`
- Modify state config/migration modules, `config.example.toml`, and migration tests

Add `orchestrator.provider_parallel_limits`, keyed by provider, defaulting to the global limit when absent. Bump config schema to v5 with a comment-preserving v4 -> v5 migration that adds an empty map only when absent. Validate known providers and positive limits.

State schema v7 adds:

```text
task_instructions
resource_claims
task_schedule_claims
```

`task_schedule_claims` binds one daemon, graph revision, task, provider, claim time, and terminal release. Unique active-task constraints prevent duplicate dispatch. Resource claims store normalized path components or an explicit repository-wide flag, lease expiry, and release reason. Migration tests must cover v1 -> v7, v6 -> v7 backup, historical event hashes, constraints, and future-schema rejection.

Commit: `feat: persist task scheduling and instruction state`

## Task 3: atomic readiness and resource claims

**Files:**
- Create `crates/orchestrator-state/src/scheduling.rs`
- Modify `crates/orchestrator-state/src/lib.rs`

**Test first:** two SQLite connections competing for the same task, independent tasks admitted up to global capacity, provider-specific capacity, dependency completion, cross-session path overlap, component-aware non-overlap (`src/a` versus `src/ab`), repository-wide exclusion, expired claim recovery, deterministic graph order, and current-head supersession.

Implement one `BEGIN IMMEDIATE` claim operation. It reads active schedule/resource claims, current approved graph membership, task/dependency states, and requested limits; runs the domain selector; then inserts the schedule and all resource claims atomically. Release is idempotent and records a bounded reason. Recovery may expire only claims whose daemon lease is no longer authoritative.

Commit: `feat: atomically claim ready graph tasks`

### Checkpoint I

Inspect concurrent SQLite claims and verify that limits and path ownership remain correct without relying on process-local semaphores alone.

## Task 4: durable task instructions

**Files:**
- Create `crates/orchestrator-state/src/instructions.rs`
- Modify daemon command processing and recovery

**Test first:** a task-targeted final user message creates exactly one instruction for that task; session-level messages do not; claims have one winner; legal state transitions are one-way; stale `Applying` becomes `Interrupted`; content is redacted before persistence; foreign-session and terminal-task targets fail closed.

`AppendMessage` remains the conversation record, but when `task_id` is present the same transaction also inserts the instruction. An executor claims queued instructions only between provider invocations, marks them `Applying`, includes their IDs/content in the next prompt, and marks them `Applied` only after the invocation starts successfully. It never parses a rendered label to identify a task.

Commit: `feat: queue safe-boundary task instructions`

## Task 5: reusable official-CLI task executor

**Files:**
- Create `crates/orchestrator-engine/src/task_executor.rs`
- Create `crates/orchestrator-cli/src/task_executor.rs`
- Modify CLI library exports and focused execution helpers

Define an engine-facing `TaskExecutor` trait and typed request/result containing task/revision/provider/profile/scopes/instructions/worktree identity. The production executor reuses `GitWorktreeManager`, official provider adapters, `ProcessAdapterRuntime`, checkpoint/artifact collection, secret preflight, and `VerificationEngine` rather than spawning a shell command.

For a new task it performs legal `Queued -> Analyzing -> Planned -> Running` transitions, creates/persists one worktree, acquires existing coordinator/worker leases, runs a bounded provider invocation, persists normalized events/attempt output and a sealed checkpoint, verifies acceptance evidence, and reaches `Completed` only with `verification_passed`. On failure it preserves the worktree and transitions to `Blocked` or `Failed` according to confirmed process termination. Changed paths are checked against declared graph scopes before completion.

Fake executor tests cover success, timeout, quota, crash, cancellation, out-of-scope mutation, instruction continuation, lease release, and no merge/push/cleanup.

Commit: `feat: execute approved graph tasks in isolated worktrees`

## Task 6: daemon scheduler ownership and bounded concurrency

**Files:**
- Create `crates/orchestrator-daemon/src/scheduling.rs`
- Modify daemon service loop and CLI daemon composition

**Test first:** two independent slow tasks overlap in time; global limit is exact; same-provider limit is exact; dependent task starts only after verified completion; scheduler heartbeat continues; TUI disconnect is irrelevant; stop cancels workers at safe boundaries; completion releases claims and immediately enables the next task; crash recovery never double starts a task.

The daemon owns a `JoinSet` of task jobs. SQLite claims are authoritative; Tokio semaphores provide local backpressure. Each loop reaps completions, renews active schedule/resource/coordinator/worker leases, processes controls/instructions, and claims fair ready work until capacity is full. A job owns its executor future and cancellation token. Panic/drop paths retain enough durable state for conservative recovery.

Production composition probes only configured official CLIs and builds provider adapters from verified capability evidence. If no eligible writable provider exists, queued tasks stay visible with a readiness blocker rather than failing the graph.

Commit: `feat: schedule approved tasks with bounded concurrency`

## Task 7: live activity and instruction projection

**Files:**
- Modify state workspace projection
- Modify TUI chat model/render/state
- Modify CLI chat driver

**Test first:** running/provider/profile/elapsed/worktree/changed-file/test data refreshes; queued tasks show dependency/capacity/claim blockers; task-targeted submission never retargets the composer; instruction rows show queued/applying/applied/rejected/interrupted; cross-session data cannot appear; 200 ms refresh stays bounded.

Add relational readiness and instruction summaries to `WorkspaceTask`/`TaskInspector`. `@task-<uuid>` and an explicitly selected task target submit a normal typed message command whose task identity is separate from text. `@all` remains unavailable until a later explicit broadcast design. Add clear text symbols for running, waiting on dependency, waiting on capacity, blocked scope, and attention.

Commit: `feat: show parallel task activity and instructions`

## Task 8: real-process fake-provider E2E

**Files:**
- Create `crates/orchestrator-cli/tests/chat_parallel_execution.rs`
- Extend fake provider fixture logging

In an isolated Git repository, run the real daemon and fake official CLI. Plan and approve a graph with two independent tasks and one dependent task. Assert two invocations overlap, the configured global/provider maxima are never exceeded, each task has a distinct worktree/branch/leases/claims, the dependent starts only after both prerequisites verify, a task-targeted instruction is applied only to its task after a safe boundary, closing the initiating client does not stop work, and reconnect restores final status. Assert the user branch is unchanged and all worktrees remain.

Commit: `test: prove bounded parallel chat execution`

### Checkpoint J

Inspect invocation timestamps, argv, worktree roots, SQLite leases/claims/instructions, Git branches, and user-branch HEAD. Confirm no real provider, merge, push, or cleanup occurred.

## Task 9: Phase 4 documentation and audit

Update README and architecture/operations/testing/threat-model/migrations/release docs. Create `docs/superpowers/audits/2026-07-21-parallel-task-execution-phase4.md` with direct evidence for readiness, fairness, exact limits, isolation, scope enforcement, safe instruction delivery, heartbeat/reconnect/recovery, fake-only execution, and retained unintegrated worktrees.

Run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git diff --check
```

Commit: `docs: describe bounded parallel task execution`

## Phase 4 exit criteria

1. Independent approved tasks truly overlap within exact global/provider limits.
2. Dependencies wait for independently verified completion.
3. Each writer has isolated Git, lease, and path ownership.
4. Scope violations block without widening claims.
5. Task instructions are durable, target-exact, and safe-boundary applied.
6. Daemon stop/crash/reconnect cannot duplicate a writable invocation.
7. No result reaches an integration or user branch, and no worktree is deleted.
8. All tests use fake providers and the full repository gates pass.
