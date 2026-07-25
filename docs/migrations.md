# Schema and migration policy

State, config, handover, worker result, checkpoint, routing decision, and usage documents carry explicit schema versions. Writers emit the current schema; readers reject unknown future versions rather than guessing.

## SQLite

SQLite migrations are embedded, ordered through v15, and checksum-verified. The runner refuses gaps and never skips an intermediate version. Each pending migration executes in its own transaction and advances `PRAGMA user_version`; a failed step rolls back that step and leaves later versions unapplied. Every workspace-owned v15 row has a non-null `workspace_id`; daemon ownership and other user-global metadata remain outside workspace partitions.

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

State schema v6 adds immutable graph revisions, planning attempts, session graph
heads, exact approvals, ordered session-task membership, and relational task
dependencies. It rebuilds the v5 client-command table so only typed graph
actions are accepted while preserving existing rows and idempotency. Planning
attempts support a durable in-flight state and one terminal completion; graph
triggers allow only the intended planning-to-valid/invalid and
awaiting-approval-to-approved/superseded transitions. Existing databases are
backed up before the rebuild, and historical event JSON/hashes are not rewritten.

State schema v7 adds `task_schedule_claims`, `resource_claims`, and
`task_instructions`. Schedule claims bind one daemon, approved graph revision,
task, provider, lease window, and explicit release reason. Partial unique
indexes prevent two active claims for one task. Resource rows store normalized
path components or repository-wide ownership and are released with the parent
claim. Instructions retain per-task order and the
`queued -> applying -> applied|rejected|interrupted` lifecycle across restarts.

State schema v8 adds immutable integration batches, ordered source evidence,
exact approvals, application journals, resolution-task links, and the three
typed integration command actions. Existing command rows are preserved through
the constrained-table rebuild.

State schema v9 adds durable daemon startup phases and redacted diagnostics so
booting and provider probing remain observable and timeout cleanup can release
only the exact spawned instance.

State schema v10 adds read-only conversation attempts, immutable requirement
revisions and session heads, typed conversation commands, and validation
authority on graph revisions. The authority binds an approvable graph to the
exact requirement revision, validation hash, and Git base commit. Existing
command rows are preserved through the constrained-table rebuild.

State schema v11 adds compatibility projections for the explicit `validating`
session state and the Agy provider without rewriting legacy constrained columns.
It also persists exact graph-approval authority (session, requirement revision,
validation hash, and base commit) and active daemon executable/build/target identity.
Triggers keep legacy readers safe while current readers use the v11 projections.

State schema v12 adds the user-global workspace registry and canonical path history. State schema
v13 rebuilds workspace-owned tables with composite workspace keys, independent event sequences,
and a reserved compatibility partition only when pre-workspace rows exist. State schema v14 adds
idempotent legacy-import evidence and mappings. State schema v15 adds durable command invocation
fences used by the single daemon writer.

The live database is the native user-global state file (`$COLAY_HOME/state/state.db` when
`COLAY_HOME` is set), never a repository-local SQLite file. Before opening it, `migrate status`,
`plan`, `apply`, and migration rollback acquire the same OS singleton lock as the user daemon. If
the daemon is live, stop it and repeat the maintenance command; a second writer is never admitted.

For an existing nonzero schema, `migrate apply` creates a verified SQLite backup under the global
state backup directory before applying pending versions. A brand-new empty database has no prior
state to back up. Missing local or global configuration uses validated defaults and does not create
a repository config. Forward apply is idempotent: no pending versions succeeds without another
backup. Provider compatibility and safe mode are never migration authority. Every normal database
open first reads the existing database header directly. When recovery sidecars are present, it copies
the database and WAL or rollback journal into private temporary scratch and asks SQLite for the
effective schema only from that copy. Future-schema refusal therefore completes before SQLite opens
the source or changes its journal/shared-memory state, and before any migration audit append, so an
unknown future database and its sidecars are not modified. After an explicit migration, `doctor`
reports global integrity, foreign keys, current workspace audit head, and artifact-reference
integrity. Doctor itself never applies a migration or creates a backup.

`migrate apply --dry-run` copies the live database to a temporary directory, applies the same catalog to the copy, and runs integrity/foreign-key checks without modifying the source. Integration contracts verify the complete plan through v15, prove dry-run non-mutation, preserve historical event hashes, reject checksum tampering/future schemas, and exercise the workspace-partition and command-fence migrations. Separate fixtures prove later migrations preserve completed command rows and create a verified pre-apply backup.

## Configuration

The current config schema is v4. The optional
`orchestrator.provider_parallel_limits` map is an additive v4 field: an absent
map means every provider inherits `max_parallel_workers`, while present values
must be positive and name known providers. No persisted document rewrite is
required. Config migration uses a separate raw-document reader so normal
startup remains strict: `ConfigDocument` accepts only v4, while
`MigratableConfigDocument` accepts the supported v1-v4 range and rejects future
or pre-v1 versions without guessing.

The migration catalog is explicit and sequential:

- v1 -> v2 adds `orchestrator.automatic_routing = true` only when the field is absent.
- v2 -> v3 adds `orchestrator.redaction.patterns = []` only when the field is absent.
- v3 -> v4 materializes the legacy `.codex/orchestrator` state path only when an older config omitted `orchestrator.state_dir`; explicit paths are never changed. New v4 configs default to `.colay`.

Each step advances `config_version` exactly once. A v1 document therefore always executes v1 -> v2 -> v3 -> v4; no caller can request a skipped intermediate version. Transformations use `toml_edit`, preserving comments, ordering, existing values, and unknown fields. The v4 result is then parsed and validated through the strict current-schema reader.

The state API exposes a non-mutating plan and dry-run result. A live config apply rechecks that the source has not changed since planning, creates and verifies the required sibling `config.toml.backup.<timestamp>`, and only then uses the existing atomic-save path. An already-current config is not rewritten and does not create a backup. CLI `migrate status`, `migrate plan`, and `migrate apply [--dry-run]` should present the config plan/result alongside the SQLite plan/result.

## Rollback

`migrate rollback plan [--backup <path>]` selects only a regular, non-symlink file below the global backup root, verifies SQLite integrity, foreign keys, the sequential migration catalog, backup SHA-256, and exact append-only event sequence, then writes an immutable integrity-sealed plan artifact. If `--backup` is omitted, the newest migration backup is selected. Planning never swaps the live database and owns the singleton maintenance lock for the entire direct database operation.

`migrate rollback apply --plan-hash <sha256> --approved-by <identity>` loads that exact artifact and requires a non-empty plan-bound administrator identity. It revalidates schema, hash, event sequence, task/lease quiescence, and safe mode; creates and verifies a full online recovery backup; restores through the locked SQLite connection; then checks integrity, foreign keys, schema, and event sequence again. A failed restore or post-restore check automatically restores the recovery image and retains it for repair. A successful current-schema restore appends the deferred `rollback_planned` and `migration_completed` events without rewriting `events.jsonl`; all outcomes retain immutable approval/result artifacts.

There is no destructive down-SQL. A backup whose event outbox is behind the live append-only JSONL chain is rejected, even when its schema would otherwise be readable. See [`rollback.md`](rollback.md) for the distinction between database recovery and release binary/config rollback.

Restoring a pre-v4 backup removes durable sessions and daemon lease history along
with every other post-backup record. The normal rollback guards therefore still
require task/lease quiescence, exact event sequence, an approved sealed plan, and
a verified recovery backup; daemon lifecycle does not bypass those controls.
Restoring a v4 backup also removes v5 task-selection preferences; chat content
and task records remain governed by the selected backup image.
Restoring a pre-v6 backup also removes graph revisions, planning attempts,
approvals, session graph membership, and dependency rows. The normal quiescence,
sealed-plan, backup, and exact event-sequence guards still apply.
