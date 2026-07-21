# Phase 3 audit: approved task-graph planning

Date: 2026-07-21  
Scope: chat goal -> read-only official-CLI planning -> deterministic validation -> exact typed approval -> queued session graph  
Boundary: no scheduling, worker execution, worktree allocation, integration, merge, push, or cleanup

## Result

Phase 3 is accepted. A conversation goal can produce an immutable, provider-neutral task-graph revision through a bounded read-only official CLI process. Invalid proposals remain inspectable without an approval hash. A user must review the full text plan card and press `y`; the daemon then verifies the current revision and exact SHA-256 seal in one short transaction before creating queued tasks and relational dependencies.

## Direct evidence

| Invariant | Evidence |
|---|---|
| Deterministic validation | Domain tests cover schema/identity, missing/duplicate/self dependencies, cycles, provider/profile policy, parallel limits, independent scope overlap, topological order, and stable canonical hashes. |
| Invalid retention | State graph/workspace tests retain validation and redacted error data while asserting `proposal_hash = NULL` and no approval card. |
| Read-only planning | `chat_plan_fake_provider` inspects separated fake-CLI argv for `--sandbox read-only`, configured model/effort, cwd, timeout, and 1 MiB stdout bound. |
| Heartbeat continuity | Daemon planning tests run a slow fake planner while a 10 ms heartbeat advances; stop/cancellation aborts the owned planner process. |
| Crash reconciliation | Durable command tests replay a command-derived revision/attempt once after restart and conservatively recover claimed graph commands. |
| Zero pre-approval writes | `chat_plan_approval` observes `(tasks, worktrees, worker_leases, task_dependencies) = (0, 0, 0, 0)` after a real daemon completes planning. |
| Wrong-hash rejection | The real daemon marks a typed wrong-hash approval failed and the same four counts remain zero. |
| Exact idempotent approval | Exact approval produces `(2, 0, 0, 1)` for the two-node fixture; replay returns the original durable command and creates nothing twice. |
| Session isolation | Workspace tests use two sessions and verify graph-head membership, display order, relational dependency IDs/labels, bounded fallback only without a graph, and selected-inspector confinement. |
| Approval UX | Reducer/render tests cover newest eligible goal selection, complete revision/hash/node/dependency/scope/provider/profile/risk/concurrency display, `y` only, `n`/Esc cancel, compact/offline/invalid blocking, and stale-hash overlay closure. |
| Reconnect | The E2E test opens a second SQLite connection and recovers the approved revision, two ordered tasks, and one dependency. |
| No listener or real provider | Control remains SQLite-only. Process tests configure only `colay-e2e-fake-provider`; its invocation log records exactly one planner call. |

## Migration and persistence

SQLite schema v6 adds graph revisions, durable planning attempts, graph heads, exact approvals, session-task membership, and dependencies. The migration catalog remains append-only and checksum verified. v1-to-v6 tests preserve historical event hashes; a separate v5-to-v6 test verifies the command-table rebuild and backup. Planning errors are redacted before persistence. Display strings never reconstruct approval authority.

## Phase boundary

Approved tasks are deliberately left in `Queued`. Phase 4 owns dependency-aware scheduling, per-provider/global concurrency, isolated worktrees, and task-target instructions. Phase 5 owns verification, integration preview/approval, conflict recovery, merge, push, and cleanup. No Phase 3 claim implies those later capabilities.

## Verification commands

The implementation was checked with focused domain/state/engine/daemon/TUI/CLI suites, the real-daemon fake-provider process test, formatting, `git diff --check`, workspace Clippy with `-D warnings`, and the full all-features workspace test suite. The final command outputs are recorded in the implementation session; all gates must remain green before Phase 4 begins.
