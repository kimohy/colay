# SQLite Idle Writer Starvation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make idle daemon polling read-only so direct commands are not starved by unnecessary SQLite `BEGIN IMMEDIATE` transactions.

**Architecture:** Each claim path first runs a read-only `SELECT EXISTS` on the already locked connection. It returns `None` without a transaction when no candidate exists; when work may exist, it enters the unchanged immediate transaction and re-queries before updating, preserving atomic ownership and TOCTOU safety.

**Tech Stack:** Rust 2024, rusqlite WAL connections, existing state unit tests, and no provider process.

## Global Constraints

- Preserve schema v8 and append-only audit behavior.
- Do not weaken concurrent claim single-winner semantics.
- Do not add unbounded SQLite retries.
- Tests use file-backed SQLite and no real provider inference.

---

### Task 1: Read-only precheck for empty client-command queues

**Files:**
- Modify: `crates/orchestrator-state/src/client_commands.rs`

**Interfaces:** Private helper `pending_command_exists(&Connection, ClientCommandQueue) -> StateResult<bool>`; public claim APIs remain unchanged.

- [x] Add a file-backed regression that opens a second connection, holds `BEGIN IMMEDIATE`, and verifies empty general, session, and orchestration claim calls all return `None` rather than `SQLITE_BUSY`.
- [x] Run the focused regression and observe the expected lock failure before implementation.
- [x] Add read-only `SELECT EXISTS` probes using the same action filters as the transaction queries; return before `transaction_with_behavior` only when false.
- [x] Re-run all client-command tests and preserve the concurrent single-winner test.
- [x] Commit the command and scheduler fix together as `bf49188` (`fix: avoid idle SQLite writer claims`).

### Task 2: Read-only precheck for an idle scheduler

**Files:**
- Modify: `crates/orchestrator-state/src/scheduling.rs`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

**Interfaces:** Private helper `queued_schedule_candidate_exists(&Connection) -> StateResult<bool>`; `claim_next_ready_task` remains unchanged.

- [x] Add a file-backed regression with an active daemon lease, no session task, and a second connection holding `BEGIN IMMEDIATE`; `claim_next_ready_task` must return `None`.
- [x] Run the focused test and observe the expected lock failure.
- [x] Add a read-only `SELECT EXISTS` for current approved, unarchived, queued, unpaused session tasks before opening the immediate transaction. The transaction keeps candidate, dependency, capacity, and claim revalidation.
- [x] Run state tests, fmt, clippy, npm tests, and the full fake-provider workspace suite.
- [x] Mark `WSL-003` fixed after the lock regressions and full suite pass.

## Plan Self-Review

- Coverage: fixes the three observed idle writer paths without changing schema or claim ownership.
- Safety: a false-negative race delays work until the next bounded poll; a false positive only enters the existing atomic transaction.
- Scope: daemon startup, provider lease recovery, and conversation persistence remain separate plans.
