# Task 4 Report: User-daemon bootstrap, migration, and local IPC

## Status

Complete. Normal daemon lifecycle bootstrap now resolves one user-global state root, acquires a singleton owner lock, migrates the global database before loading repository/provider configuration, imports the encountered configured legacy state without modifying it, and publishes a versioned local IPC endpoint. Root `status` and daemon lifecycle commands establish or use that global daemon boundary. The remaining operational command conversion is intentionally assigned to Tasks 5 and 6.

## Implementation

- Added schema-v1 newline-delimited `IpcRequest`/`IpcResponse` contracts and a bounded local IPC server in `orchestrator-daemon`.
- Added an owner-private filesystem lock around daemon startup. Unix uses a mode-`0600` socket; Windows uses a local named pipe with remote clients rejected and the process token's default local DACL.
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
- The later cross-platform rollout task remains responsible for the explicit Windows current-SID named-pipe ACL smoke assertion and the 32-client stress matrix.
