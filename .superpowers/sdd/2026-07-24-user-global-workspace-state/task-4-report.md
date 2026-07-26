# Task 4 Report: User-daemon bootstrap, migration, and local IPC

## Status

Complete. Normal daemon lifecycle bootstrap now resolves one user-global state root, acquires a singleton owner lock, migrates the global database before loading repository/provider configuration, imports the encountered configured legacy state without modifying it, and publishes a versioned local IPC endpoint. Root `status` and daemon lifecycle commands establish or use that global daemon boundary. The remaining operational command conversion is intentionally assigned to Tasks 5 and 6.

## Implementation

- Added schema-v1 newline-delimited `IpcRequest`/`IpcResponse` contracts and a bounded local IPC server in `orchestrator-daemon`.
- Added an owner-private filesystem lock around daemon startup. Unix uses a mode-`0600` socket; Windows uses a local named pipe with remote clients rejected and an explicit protected DACL containing only the current-user SID.
- Serialized mutations through one bounded daemon writer queue. Request handlers do not retain SQLite transactions across `await` points.
- Added `DaemonClient::{connect_or_start, connect, request, stream}` with bounded readiness/reply waits, request UUIDs, separated executable/argument spawning, and singleton-contender retries.
- Reordered startup to resolve global paths, lock, open, back up/migrate, load validated configuration, register/import the configured legacy workspace, bind IPC, then probe providers.
- Kept provider failures from blocking migrated state/IPC readiness and fixed post-readiness service-setup failures to release an online startup lease without attempting an invalid phase transition.
- Changed `init` to create only the optional repository configuration override and report the resolved global state root; it no longer creates repository-local SQLite state.
- Updated transitional integration fixtures that still exercise pre-Task-5/6 direct local command paths so the full existing suite remains explicit and deterministic.

## TDD evidence

- Initial ownership RED: `two_repositories_share_one_global_daemon_and_database` observed zero global database files instead of one.
- Protocol RED: the focused IPC contract test failed to compile before `IPC_SCHEMA_VERSION` and `IpcRequest` existed.
- Configured-import RED: `configured_legacy_state_dir_is_resolved_after_global_migration` failed because bootstrap inspected corrupt default `.colay` state before applying the configured legacy path.
- Online-cleanup RED: `failure_after_ipc_readiness_releases_an_online_startup_lease` returned an optimistic phase-transition conflict and left the lease online.
- Bounded-frame GREEN: `oversized_request_is_rejected_and_connection_is_closed` proves the reader stops at one MiB plus one byte, returns a protocol error, and closes rather than buffering the remainder.
- Focused GREEN: `cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture` — 4 passed.
- Adjacent GREEN: `cargo test -p colay --test daemon_lifecycle --features test-fixtures -- --nocapture` — passed.
- WIN-003 focused rerun: `cargo test -p orchestrator-state --all-features scheduling::tests::two_connections_never_claim_the_same_task_and_release_is_idempotent -- --nocapture` — 1 passed. The prior full-run temporary-directory `icacls` ENOENT was non-reproducible; the final full run also passed this test.

## Required verification

- `cargo fmt --all -- --check` — passed.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed.
- `cargo test --workspace --all-features` — final clean-build run passed in 648.8 seconds; all unit, integration, fake-provider, and doc tests passed.
- `git diff --check` — passed (Git emitted only the repository's existing LF-to-CRLF checkout notices).

## Self-review and follow-up boundary

- One database and one daemon owner are shared by distinct repositories, and automatic migration completes before an untrusted provider can be evaluated.
- Legacy import remains source-read-only and idempotent; configured state directories are honored only after the global migration boundary.
- IPC currently exposes lifecycle, readiness, status, and workspace registration. Task 5 adds conversation-first `run`; Task 6 converts the remaining ordinary state mutations and removes the transitional direct local writers.
- The later cross-platform rollout task remains responsible for the 32-client stress matrix; the explicit Windows current-SID named-pipe ACL smoke assertion is covered here.

## Review fix round 1

### Contract after the fix

- Root `status` now performs workspace registration and the status read through the user daemon's `workspace.status` IPC action. It reports the global state root and does not open or create repository-local SQLite state.
- The IPC server task and orchestration runtime are supervised together. A named-pipe/socket create or accept failure cancels orchestration, drops its lease guard, releases the online lease, and returns the IPC failure instead of leaving an unreachable online daemon.
- Windows pipe creation uses a small audited `orchestrator-windows-ipc` FFI boundary to pass a protected, current-user-SID-only `SECURITY_DESCRIPTOR` to Tokio. Remote clients remain rejected.
- Windows sharing/lock violations (`ERROR_SHARING_VIOLATION` and `ERROR_LOCK_VIOLATION`) are normalized to the singleton `AlreadyOwned` result, so a second owner is rejected consistently.
- The migration acceptance fixture now requires the fake provider process itself to open the global database read-only and record the schema version it observed. The test cannot pass unless provider evaluation actually happens after migration.
- Task 5's conversation-first `run` conversion and Task 6's remaining TUI/ordinary mutation conversion remain unchanged and intentionally out of this fix round.

### Finding 1: normal CLI status bypassed the daemon database

- Test: `crates/orchestrator-cli/tests/global_daemon.rs` — `status_reads_global_state_without_opening_repository_sqlite`.
- RED command: `cargo test -p colay --features test-fixtures --test global_daemon status_reads_global_state_without_opening_repository_sqlite -- --exact --nocapture`.
- RED output: failed with `left: ...\\first\\.colay` and `right: ...\\home...`, proving the command still reported repository-local state after global daemon bootstrap.
- GREEN command: `cargo test -p colay --features test-fixtures --test global_daemon status_reads_global_state_without_opening_repository_sqlite -- --exact --nocapture`.
- GREEN output: `1 passed; 0 failed`; the response reports `COLAY_HOME` and `.colay/orchestrator.db` is absent.

### Finding 2: IPC task failure was unsupervised

- Test: `crates/orchestrator-cli/src/daemon.rs` — `daemon::tests::ipc_failure_cancels_runtime_and_releases_online_lease`.
- RED command: `cargo test -p colay --features test-fixtures daemon::tests::ipc_failure_cancels_runtime_and_releases_online_lease -- --exact --nocapture`.
- RED output: compile failed with `no supervise_daemon_runtime in daemon` before the joint supervisor existed.
- GREEN command: `cargo test -p colay --features test-fixtures daemon::tests::ipc_failure_cancels_runtime_and_releases_online_lease -- --exact --nocapture`.
- GREEN output: `1 passed; 0 failed`; the injected IPC accept failure is returned, cancellation is set, and daemon status is `Stopped`.

### Finding 3: Windows named pipe lacked an explicit current-user descriptor

- Test: `crates/orchestrator-cli/tests/global_daemon.rs` — `windows_pipe_dacl_grants_access_only_to_the_current_user_sid`.
- RED command: `cargo test -p colay --features test-fixtures --test global_daemon windows_pipe_dacl_grants_access_only_to_the_current_user_sid -- --exact --nocapture`.
- RED output: compile failed because `current_windows_user_sid` and `windows_named_pipe_security_descriptor` did not exist; the prior implementation exposed no descriptor assertion and used Tokio's default DACL.
- GREEN command: `cargo test -p colay --features test-fixtures --test global_daemon windows_pipe_dacl_grants_access_only_to_the_current_user_sid -- --exact --nocapture`.
- GREEN output: `1 passed; 0 failed`; the actual live pipe SDDL contains one allow ACE for the current SID and no `WD`, `AN`, `AU`, or `BU` trustee.

### Finding 4a: second daemon ownership was not acceptance-tested

- Test: `crates/orchestrator-cli/tests/global_daemon.rs` — `second_daemon_owner_is_rejected_without_displacing_the_live_owner`.
- Initial RED command: `cargo test -p colay --features test-fixtures --test global_daemon second_daemon_owner_is_rejected_without_displacing_the_live_owner -- --exact --nocapture`.
- Initial RED output on Windows: the contender returned raw OS error 33 (`ERROR_LOCK_VIOLATION`) instead of the stable singleton rejection.
- Mutation RED: removing `try_lock_exclusive` made the same test fail with `optimistic update conflict for repository daemon lease`, proving the acceptance test detects removal of the owner lock. The mutation was immediately restored.
- GREEN command: `cargo test -p colay --features test-fixtures --test global_daemon second_daemon_owner_is_rejected_without_displacing_the_live_owner -- --exact --nocapture`.
- GREEN output: `1 passed; 0 failed`; the contender is rejected with `daemon singleton is already owned by another process`, the original daemon remains reachable, and exactly one unreleased instance remains.

### Finding 4b: migration test did not observe provider invocation order

- Test: `crates/orchestrator-cli/tests/global_daemon.rs` — `old_schema_migrates_before_untrusted_provider_is_evaluated`.
- RED command: `cargo test -p colay --features test-fixtures --test global_daemon old_schema_migrates_before_untrusted_provider_is_evaluated -- --exact --nocapture`.
- RED output: `fake provider did not record its schema observation (os error 2)` before the fake-provider schema guard existed.
- GREEN command: `cargo test -p colay --features test-fixtures --test global_daemon old_schema_migrates_before_untrusted_provider_is_evaluated -- --exact --nocapture`.
- GREEN output: `1 passed; 0 failed`; the spawned fake provider observed `STATE_SCHEMA_VERSION` from the real global database before returning capability output.

### Fix-round verification

- Focused acceptance: `cargo test -p colay --features test-fixtures --test global_daemon -- --nocapture` — `7 passed; 0 failed`.
- Required formatting: `cargo fmt --all -- --check` — passed.
- Required linting: `cargo clippy --workspace --all-targets --all-features -- -D warnings` — passed.
- Required tests: `cargo test --workspace --all-features` — passed in 471.1 seconds; all unit, integration, fake-provider, Windows permission, and doc tests passed.
- The first full-suite attempt reached its 360-second command bound without a test failure; no child process remained. The final verification used the repository's documented Windows runtime envelope and completed cleanly.
