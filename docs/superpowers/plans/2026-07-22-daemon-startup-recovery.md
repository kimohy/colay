# Daemon startup recovery implementation plan

> Execute each code change with test-driven development and keep all provider tests on the bundled
> fake provider binaries.

**Goal:** Make daemon startup phase-aware, prevent false slow-probe failures, and guarantee timeout
cleanup of the spawned process tree.

**Architecture:** Persist startup phase on the daemon lease, renew the lease while services are being
prepared, enter the main loop with the already-owned lease, and keep the parent as a bounded process
supervisor until the child becomes online.

**Tech stack:** Rust, Tokio, rusqlite, Clap integration tests, orchestrator-test-support fake provider.

---

### Task 1: Persist daemon startup phases

**Files:**
- Create: `migrations/0009_daemon_startup_phase.sql`
- Modify: `crates/orchestrator-state/src/migrations.rs`
- Modify: `crates/orchestrator-state/src/daemon_instances.rs`
- Modify: `crates/orchestrator-state/tests/migration_contract.rs`

1. Add failing state tests for phase-aware status, owner-only monotonic transitions, redacted failure
   storage, and migration 8 to 9 compatibility.
2. Run the focused state tests and confirm they fail for missing phase APIs/schema.
3. Add migration 9 and the minimum record/API changes needed by the tests.
4. Run the focused state tests and migration contract until green.
5. Commit the state model separately.

### Task 2: Acquire and renew the lease during startup

**Files:**
- Modify: `crates/orchestrator-daemon/src/lib.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`

1. Add failing daemon tests that exercise an already-acquired lease and startup stop handling.
2. Confirm the focused tests fail because the current daemon loop always acquires a new lease.
3. Add an entry point that runs the normal loop with exact existing ownership.
4. In the CLI server, acquire `booting`, run a bounded startup heartbeat, transition to `probing`,
   construct services, transition to `online`, then enter the pre-acquired loop.
5. On setup failure, persist a redacted `failed` diagnostic and release exact ownership.
6. Run focused daemon and CLI unit tests until green.

### Task 3: Supervise and clean up the child process

**Files:**
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Modify: `crates/orchestrator-cli/tests/daemon_lifecycle.rs`

1. Extend the fake provider with a test-only capability-probe delay.
2. Add a failing integration test proving a probe longer than five seconds is progress rather than a
   false failure.
3. Add a failing integration test with a test-fixture-only short deadline proving the timed-out child
   cannot later acquire a lease and a subsequent start succeeds.
4. Confirm both tests fail against the current implementation.
5. Capture bounded child stderr, poll phase-aware status, and derive the production deadline from the
   configured enabled-provider probe budget.
6. Add exact process-tree termination and confirmed child reaping for Windows and Unix without shell
   interpolation.
7. Run the lifecycle tests repeatedly on Windows.

### Task 4: Document and verify

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

1. Record root cause, implementation commits, phase behavior, diagnostics, and Windows evidence.
2. Run `cargo fmt --all -- --check`.
3. Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
4. Run `cargo test --workspace --all-features`.
5. Run the npm suite used by the repository and record its count.
6. Confirm the worktree is clean except for intentional committed changes.
