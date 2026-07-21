# Phase 4 audit: bounded parallel task execution

Date: 2026-07-21

## Decision

Phase 4 is accepted. Tasks from the current approved session graph are claimed
atomically, execute through configured official-CLI adapters in isolated Git
worktrees, and complete only after sealed checkpoint and independent
verification evidence. No Phase 4 path integrates, merges, pushes, publishes,
or deletes a task result.

## Direct evidence

| Property | Evidence |
|---|---|
| Current graph and dependency readiness | `orchestrator-state::scheduling` loads only the approved graph head. Its tests require every dependency to be completed with a latest passing verification before admission. |
| Fair, exact capacity | `claim_next_ready_task` runs under `BEGIN IMMEDIATE`, orders graph candidates deterministically, and evaluates global plus per-provider active counts. Two independent SQLite connections cannot claim the same task. |
| Scope isolation | Normalized component-aware resource claims distinguish `src/a` from `src/ab`, block true ancestor/descendant overlap, and make repository-wide ownership exclusive. Claims are never widened after dispatch. |
| Real concurrency | `orchestrator-daemon::execution::scheduler_runs_disjoint_tasks_in_parallel_and_releases_all_claims` observes two simultaneous executor futures. `parallel_task_execution.rs` observes overlapping persisted claim intervals for two real fake-CLI subprocesses. |
| Git isolation | The process test creates a real temporary Git repository and proves distinct active task worktrees and branches remain present while the user's branch is unchanged. Worktree creation alone is serialized; provider turns remain parallel. |
| Completion gate | `TaskExecutionReport::passed_completion_gate` requires a successful structured outcome, a passing verification result, and a checkpoint whose integrity seal recomputes. The daemon cannot enter `completed` without that gate. |
| Target-exact instructions | Message and instruction rows are inserted atomically after relational current-session/current-graph validation. Ordered transitions are durable and display labels are never parsed as identity. |
| Instructions during execution | The process test queues a second instruction after the task is running. The daemon reuses the same validated worktree for a second official-CLI turn. A transaction enters `verifying` only when no queued, applying, or interrupted instruction remains; later targets are rejected outside instruction-accepting states. Both instruction rows reach `applied`. |
| Heartbeat and recovery | Active claims renew while executor futures run. Release is idempotent. Restart reconciliation retains completed attempts, and the process test proves restart creates no duplicate attempt. |
| Failure isolation | A failed report transitions only its claimed task and releases its claims; an unrelated sibling job is neither cancelled nor rolled back. |
| Fake-only execution | Integration coverage resolves only the compiled `colay-e2e-fake-provider`. The fixture uses real child processes and structured streams without provider credentials or network inference. |

## Persisted contracts

SQLite schema v7 adds schedule claims, resource claims, and ordered task
instructions. The migration remains sequential and checksum verified. The
optional `provider_parallel_limits` map is an additive configuration-v4 field;
absence inherits the global limit, while unknown provider names and zero values
fail validation. This additive choice avoids rewriting existing v4 layered
configuration without weakening version strictness.

## Focused verification

The following focused checks passed before the final workspace gate:

```text
cargo test -p orchestrator-state scheduling
cargo test -p orchestrator-state instructions
cargo test -p orchestrator-daemon execution
cargo test -p colay --features test-fixtures --test parallel_task_execution -- --nocapture
cargo clippy -p colay --all-targets --all-features -- -D warnings
```

The real-process test additionally proves a running-task instruction causes one
continuation attempt in the existing worktree, both instructions are applied,
the sibling still has one attempt, active claims return to zero, and restart
does not change either count.

## Residual boundary

Verified worktrees are retained inputs for Phase 5. Phase 4 grants no authority
to apply them to an integration or user branch. Conflict previews, exact-hash
integration approval, deterministic application, and recovery tasks remain a
separate implementation and approval boundary.
