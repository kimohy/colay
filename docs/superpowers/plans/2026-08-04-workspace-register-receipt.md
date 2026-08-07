# Workspace registration receipt implementation plan

**Goal:** Remove the duplicate legacy inspection after a fresh daemon bootstrap without increasing
the IPC timeout or weakening incumbent/new-workspace registration.

**Design:** `docs/superpowers/specs/2026-08-04-workspace-register-receipt-design.md`

### Task 1: Record the Windows CI defect and add RED observability contracts

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Modify: `crates/orchestrator-state/src/legacy_import.rs`
- Modify: focused test-support/CLI integration tests as required

- Add `WIN-005` as `fix-in-progress`, with Release run `30866284840`, failed main CI run
  `30866284842`, Windows job `91858756051`, the two exact failures, and the duplicate-inspection
  diagnosis. Do not close WSL-022/023.
- Add a `test-fixtures`-only, content-free inspection marker/counter.
- Add a cold-start RED test proving the current path performs three inspections where the approved
  contract requires two.
- Commit the tracker/RED contract separately.

### Task 2: Add the additive daemon bootstrap receipt

**Files:**
- Modify: `crates/orchestrator-daemon/src/ipc.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: daemon IPC unit/integration tests

- Add optional startup workspace identity to `IpcServer` and `daemon.ping` data.
- Set it only after bootstrap registration/import succeeded and before serving IPC.
- Keep schema v1 and the old response shape when unset.
- Test opaque-field behavior and absence from unrelated responses.
- Commit the daemon side separately.

### Task 3: Reuse the receipt only for the exact spawned owner

**Files:**
- Modify: `crates/orchestrator-cli/src/ipc_client.rs`
- Modify: focused CLI lifecycle/global daemon tests

- Parse the optional workspace ID and reject malformed values.
- Propagate the receipt through readiness only when the spawned child PID is the live owner.
- Skip `workspace.register` only for that exact owner; preserve fallback for incumbent, contender,
  legacy, and old-peer paths.
- Make the cold legacy marker test GREEN at exactly two inspections.
- Verify existing daemon plus a second workspace still imports before command/activation.
- Commit the client handshake separately.

### Task 4: Stress the Windows lifecycle and document source-fixed evidence

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Modify: `docs/testing.md` if the new marker/receipt contract needs durable guidance

- Run the two original exact failing tests repeatedly, the live-doctor group in parallel and
  sequential modes, full `global_doctor`, daemon lifecycle/global daemon tests, and relevant state
  import tests.
- Record inspect counts, elapsed times, zero residual processes, source hashes, and the unchanged
  ten-second timeout.
- Mark WIN-005 `fix-in-progress (source checks passed; published CI/nightly verification pending)`.
- Commit documentation evidence.

### Task 5: Verify, review, publish, and resume nightly QA

- Run `cargo fmt --all -- --check`.
- Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- Run `cargo test --workspace --all-features` on Windows with a resource-safe process-scoped Cargo
  job limit if needed; timeouts are not passes.
- Run full WSL Clippy from a fresh Linux-native `/tmp` target and retain its log.
- Request an independent final review and fix all Critical/Important findings.
- Push a PR, require Ubuntu/macOS/Windows CI green, merge normally, and preserve worktrees.
- Require the merge-triggered Release and main CI to succeed.
- Only then resolve the newly published nightly once and resume the isolated WSL clean-install,
  daemon/schema, all-provider probe, bounded account-ready real-provider, persistence, and cleanup
  QA from the prior Task 6.
- Close WSL-022, WSL-023, and WIN-005 only through a reviewed follow-up documentation PR with exact
  published evidence.
