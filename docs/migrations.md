# Schema and migration policy

State, config, handover, worker result, checkpoint, routing decision, and usage documents carry explicit schema versions. Writers emit the current schema; readers reject unknown future versions rather than guessing.

## SQLite

SQLite migrations are embedded, ordered `v1 -> v2 -> v3 -> v4 -> v5`, and checksum-verified. The runner refuses gaps and never skips an intermediate version. Each pending migration executes in its own transaction and advances `PRAGMA user_version`; a failed step rolls back that step and leaves later versions unapplied.

State schema v4 adds `sessions`, ordered `conversation_messages`, idempotent
`client_commands`, and repository `daemon_instances`. It also adds nullable
`session_id` correlation to the event outbox. Historical v1-v3 event JSON and
hashes are not rewritten: absent `session_id` remains omitted during
serialization, and migration audit events use the pre-v4 insert shape until the
new column exists.

State schema v5 adds `session_workspace_state`, containing only the optional
selected task and update timestamp for each durable session. It restores task
navigation after a TUI reconnect without mixing presentation fields into the
provider-neutral task/session contracts. The foreign keys prevent a selection
from naming a missing session or task.

For an existing nonzero schema, `migrate apply` creates an online SQLite backup under `.colay/backups/orchestrator.db.backup.<timestamp>` before applying pending versions. A legacy config keeps using its explicitly selected state root. A brand-new empty database has no prior state to back up. After migration, `doctor` reports SQLite integrity and foreign-key health.

`migrate apply --dry-run` copies the live database to a temporary directory, applies the same catalog to the copy, and runs integrity/foreign-key checks without modifying the source. The integration contract test starts from a real v1 database, verifies the v2/v3/v4/v5 plan, proves dry-run non-mutation, checks the v1 backup, preserves historical event hashes, and rejects checksum tampering/future schemas.

## Configuration

The current config schema is v4. Config migration uses a separate raw-document reader so normal startup remains strict: `ConfigDocument` accepts only v4, while `MigratableConfigDocument` accepts the supported v1-v4 range and rejects future or pre-v1 versions without guessing.

The migration catalog is explicit and sequential:

- v1 -> v2 adds `orchestrator.automatic_routing = true` only when the field is absent.
- v2 -> v3 adds `orchestrator.redaction.patterns = []` only when the field is absent.
- v3 -> v4 materializes the legacy `.codex/orchestrator` state path only when an older config omitted `orchestrator.state_dir`; explicit paths are never changed. New v4 configs default to `.colay`.

Each step advances `config_version` exactly once. A v1 document therefore always executes v1 -> v2 -> v3 -> v4; no caller can request a skipped intermediate version. Transformations use `toml_edit`, preserving comments, ordering, existing values, and unknown fields. The v4 result is then parsed and validated through the strict current-schema reader.

The state API exposes a non-mutating plan and dry-run result. A live config apply rechecks that the source has not changed since planning, creates and verifies the required sibling `config.toml.backup.<timestamp>`, and only then uses the existing atomic-save path. An already-current config is not rewritten and does not create a backup. CLI `migrate status`, `migrate plan`, and `migrate apply [--dry-run]` should present the config plan/result alongside the SQLite plan/result.

## Rollback

`migrate rollback plan [--backup <path>]` selects only a regular, non-symlink file below the local backup root, verifies SQLite integrity, foreign keys, the sequential migration catalog, backup SHA-256, and exact append-only event sequence, then writes an immutable integrity-sealed plan artifact. If `--backup` is omitted, the newest `orchestrator.db.backup.*` is selected. Planning never swaps the live database.

`migrate rollback apply --plan-hash <sha256> --approved-by <identity>` loads that exact artifact and requires a non-empty plan-bound administrator identity. It revalidates schema, hash, event sequence, task/lease quiescence, and safe mode; creates and verifies a full online recovery backup; restores through the locked SQLite connection; then checks integrity, foreign keys, schema, and event sequence again. A failed restore or post-restore check automatically restores the recovery image and retains it for repair. A successful current-schema restore appends the deferred `rollback_planned` and `migration_completed` events without rewriting `events.jsonl`; all outcomes retain immutable approval/result artifacts.

There is no destructive down-SQL. A backup whose event outbox is behind the live append-only JSONL chain is rejected, even when its schema would otherwise be readable. See [`rollback.md`](rollback.md) for the distinction between database recovery and release binary/config rollback.

Restoring a pre-v4 backup removes durable sessions and daemon lease history along
with every other post-backup record. The normal rollback guards therefore still
require task/lease quiescence, exact event sequence, an approved sealed plan, and
a verified recovery backup; daemon lifecycle does not bypass those controls.
Restoring a v4 backup also removes v5 task-selection preferences; chat content
and task records remain governed by the selected backup image.
