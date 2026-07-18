# Rollback

Release rollback and database-migration rollback are separate operations. Neither operation deletes task worktrees, checkpoints, handovers, or results.

## Release rollback

An administrator first provisions an immutable manifest at:

```text
.colay/backups/releases/<version>/manifest.json
```

The manifest names approved backup sources and destinations. Destinations must exactly resolve to the running Colay binary, the configured file, or the configured Codex binary; a release manifest cannot replace the live task database. `rollback plan --to <version>` validates allowed roots, rejects symlink/broad-root/overlapping targets, hashes every source and current destination, excludes task/checkpoint/handover/worktree roots from replacement, and stores the sealed plan under `backups/rollback-plans`. When the database is available, the plan hash/artifact is also recorded as `rollback_planned`.

The JSON manifest starts with `"schema_version": 1`; unknown fields are rejected and manifest/plan metadata is capped at 1 MiB.

`rollback apply --to <version> --plan-hash <hash> --approved-by <audit-identity>` loads that exact immutable plan, revalidates its hash and current trusted destinations, records a plan-bound approval, stages and hashes each replacement, preserves every previous destination as a recovery backup, and writes an fsynced JSONL recovery journal. A later-step failure restores destinations already changed and retains failed-install evidence.

Colay never overwrites a provider CLI merely because it is detected on `PATH`. The configured Codex binary is replaceable only when the administrator explicitly includes the `codex` component and its exact resolved destination in the selected release manifest/plan. New manifests should name the Colay executable component `colay` or `colay_binary`; the legacy `orchestrator` aliases remain readable for rollback compatibility.

`rollback apply` re-reads the selected release manifest and requires its normalized component/source/destination set to exactly match the sealed plan. It also rejects the operation while any execution-state task, worker lease, or coordinator lease is active. Restart the Colay binary after a successful apply and run `doctor` before resuming work.

## Database migration rollback

`migrate rollback plan [--backup <path>]` verifies a trusted local SQLite image and stores an immutable plan under `backups/migration-rollback-plans`. The plan seals the canonical backup path, backup SHA-256, live and target schema versions, live and backup event sequences, creation time, and its own integrity hash. The backup and live event sequences must be identical so restoring it can never orphan an existing append-only JSONL event.

`migrate rollback apply --plan-hash <hash> --approved-by <audit-identity>` is the only database replacement path. It is disabled by startup safe mode, rejects a changed plan/backup/live sequence and active tasks or leases, persists immutable approval evidence, creates a separate recovery backup, and restores under the database connection lock. Integrity, foreign-key, migration-sequence, and exact event-sequence checks run after restore. Failure triggers automatic restoration of the recovery image; success retains both recovery and result artifacts and requires restart/`doctor` before new work.

The `rollback_planned` audit event is deferred until successful restore because appending it before apply would advance and invalidate the sealed event sequence. On schema v3 or newer, the restored outbox receives both that event and `migration_completed`, and normal reconciliation appends them to `events.jsonl`. Older schemas retain the immutable plan/approval/result artifacts and must be upgraded before current task execution.

SQLite down-migrations are not provided. A prior-schema image is restorable only when its event sequence is exactly compatible; otherwise the operation fails closed. Release manifests intentionally cannot target the live task database.
