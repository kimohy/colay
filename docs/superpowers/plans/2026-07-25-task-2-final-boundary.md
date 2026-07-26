# Task 2 Final Boundary Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the final daemon lease, scoped state boundary, workspace liveness, and test SQLite configuration findings.

**Architecture:** Split already-owned and self-acquiring daemon setup before a shared guarded runtime loop. Keep database paths in test fixture owners, use local production-equivalent connection openers, and refresh repository liveness only after a named five-minute interval through a deterministic explicit-time resolver.

**Tech Stack:** Rust 2024, Tokio, rusqlite, chrono, existing orchestrator state/domain APIs.

## Global Constraints

- Tests use only `orchestrator-test-support` fake binaries.
- No production raw SQLite or database-path escape from `WorkspaceDatabase`.
- Already-owned leases have uninterrupted RAII coverage before every fallible operation.
- Exact final gates are formatter, all-target check, full Clippy, and full all-features tests.

---

### Task 1: Cover already-owned leases at entry

**Files:**
- Modify: `crates/orchestrator-daemon/src/lib.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`

**Interfaces:**
- Consumes: existing `OwnedLeaseGuard`, `StartupLeaseGuard`, `ExecutionServices`, `DaemonSettings`.
- Produces: an already-owned entry that arms RAII before validation and a shared runtime loop that always receives an armed guard.

- [x] Add tests that acquire a startup lease, pass invalid execution services or zero runtime intervals to `serve_with_full_orchestration_on_owned_lease`, and assert an error plus `DaemonStatus::Stopped`.
- [x] Run the two tests and observe the active lease remains, proving RED.
- [x] Arm `OwnedLeaseGuard` as the first owned-entry operation, validate only under that guard, split setup from the common loop, and keep `StartupLeaseGuard` armed through the awaited handoff.
- [x] Run daemon unit and CLI daemon lifecycle tests to GREEN.

### Task 2: Bound repository liveness writes

**Files:**
- Modify: `crates/orchestrator-state/src/workspace_registry.rs`

**Interfaces:**
- Produces: `REPOSITORY_WORKSPACE_TOUCH_INTERVAL` of five minutes and a private explicit-time repository resolver used by deterministic unit tests.

- [x] Add explicit-time unit tests proving an existing registration is unchanged inside five minutes and both stored timestamps refresh after five minutes.
- [x] Run the tests and observe current `resolve_repository_workspace` cannot satisfy the explicit-time contract.
- [x] Implement one transaction that returns a fresh registration read-only or conditionally touches and reloads a stale registration.
- [x] Run workspace registry and chat TUI reconnect tests to GREEN.

### Task 3: Remove scoped database path recovery

**Files:**
- Modify: `crates/orchestrator-state/src/database.rs`
- Modify: local test support and affected tests under `crates/orchestrator-state/tests`, `crates/orchestrator-daemon/src`, `crates/orchestrator-daemon/tests`, and `crates/orchestrator-cli`.

**Interfaces:**
- Removes: `WorkspaceDatabase::database_path` and its crate-private scoped path equivalent.
- Produces: helpers that receive an explicit `&Path` or owning `&Database` plus `WorkspaceId`.

- [x] Remove the scoped accessors and run all-target check to capture external compiler failures as RED evidence.
- [x] Retain database paths in fixture values and pass them explicitly to every raw test helper.
- [x] Search production/public state API and prove a `WorkspaceDatabase` holder cannot recover a database owner, connection, transaction, or path.

### Task 4: Configure direct fixture connections like production

**Files:**
- Modify: each new local `support.rs`/`test_support.rs` connection helper and cfg-test helper in CLI modules.
- Test: `crates/orchestrator-state/tests/global_workspace_state.rs`.

**Interfaces:**
- Produces: one local `open_test_connection(path)` per test crate applying `foreign_keys=ON`, `journal_mode=WAL`, `synchronous=FULL`, `temp_store=MEMORY`, and `busy_timeout=5000`.

- [x] Add a state integration test that inserts a workspace path referencing a nonexistent workspace through the helper and expects an FK error; run it RED against current helper settings.
- [x] Route every new helper-created direct connection through its local configured opener.
- [x] Run the FK test and affected integration suites to GREEN.

### Task 5: Verify, report, and commit

- [x] Run focused regressions, `cargo fmt --all -- --check`, `cargo check --workspace --all-targets --all-features`, full Clippy, full workspace tests, boundary searches, and `git diff --check`.
- [x] Append RED/GREEN evidence to `.superpowers/sdd/global-task-2-report.md`.
- [ ] Commit implementation without merging, pushing, or deleting the worktree.
