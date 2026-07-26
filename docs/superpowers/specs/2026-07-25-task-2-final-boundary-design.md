# Task 2 Final Boundary Design

## Objective

Close the remaining Task 2 review findings without reopening raw state access or adding a write to every workspace lookup. Already-owned daemon leases must have continuous RAII coverage, workspace-scoped handles must not reveal the database file, repository liveness must refresh at a bounded cadence, and new direct test connections must match production SQLite settings.

## Lease ownership

`serve_with_full_orchestration_on_owned_lease` will arm `OwnedLeaseGuard` before validating execution services, runtime settings, or workspace reconciliation. The already-owned path and the self-acquiring path will enter a common runtime loop only after they hold an armed guard. The self-acquiring path may validate before acquisition, then creates its guard immediately after acquisition.

The CLI startup guard remains armed while awaiting the already-owned daemon entry point. The daemon guard is therefore established while the startup guard still covers the lease; after the awaited call returns, the startup guard is disarmed. Invalid execution services and invalid daemon settings must release the pre-existing lease and leave daemon status stopped or failed.

## Scoped state boundary

`WorkspaceDatabase` will expose only `workspace_id` and typed workspace operations. Its public database-path accessor and any equivalent scoped accessor will be removed. Test fixtures will retain the path or the owning `Database` created by the fixture and pass that explicit global context to test-only connection helpers.

No production API will give a caller holding only `WorkspaceDatabase` access to `Database`, `rusqlite::Connection`, `rusqlite::Transaction`, or the database path. The pre-existing `Database::path` remains available to explicit global maintenance owners.

## Repository liveness

Repository resolution will use a named five-minute stale-touch interval. A private explicit-time resolver will make behavior deterministic in unit tests. Within one transaction, an existing registration newer than the threshold is returned without a write; a stale registration updates both `workspaces.last_seen_at` and the current `workspace_paths.last_seen_at`, then reloads and returns the actual persisted registration. New and reserved-workspace adoption behavior remains unchanged.

## Test connections

Each affected test crate will have one local connection-opening helper. Every connection opened by these new helpers will apply:

- `PRAGMA foreign_keys = ON`
- `PRAGMA journal_mode = WAL`
- `PRAGMA synchronous = FULL`
- `PRAGMA temp_store = MEMORY`
- `PRAGMA busy_timeout = 5000`

A state integration test will prove enforcement by attempting an invalid foreign-key insert through the helper. Test code may open the retained path directly; no production raw connection API will be added.

## Verification

Each behavior change follows a RED/GREEN cycle. Final verification is `cargo fmt --all -- --check`, `cargo check --workspace --all-targets --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, boundary searches, and `git diff --check`. Tests continue to use only fake provider binaries.
