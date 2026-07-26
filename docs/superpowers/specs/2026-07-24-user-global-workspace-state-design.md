# User-Global Workspace State Design

**Status:** Approved design

**Date:** 2026-07-24
**Applies to:** Colay CLI, daemon, state, engine, TUI, and provider compatibility cache

## Context

Colay currently derives its SQLite database, JSONL event log, artifacts, checkpoints, handovers,
and managed worktrees from a repository-local state directory such as `.colay`. This makes a
different database appear in every working directory, prevents useful plan conversations outside
a committed Git repository, and lets the daemon and direct CLI commands compete as independent
SQLite writers. Nightly users have observed `SQLITE_BUSY`, raw Git readiness errors, missing local
config failures, provider-safe-mode migration deadlocks, and active-lease `resume` conflicts.

Colay will instead maintain one physical SQLite database per OS user environment. Every project or
ordinary directory is represented by a stable `workspace_id`, and all workspace-owned records are
partitioned by that identifier. Windows and WSL deliberately use separate databases; they never
open the same SQLite file through `/mnt/c`. A single user daemon owns normal database writes.

This design is also the storage prerequisite for
`2026-07-24-provider-minimum-version-capability-policy-design.md`. Provider compatibility evidence
is user-wide and therefore lives outside any individual workspace partition.

## Goals

- Use one SQLite database for every workspace belonging to one OS user environment.
- Partition workspace state and audit history by a stable `workspace_id`.
- Remove repository-local state files as a requirement while retaining an optional, shareable,
  non-secret `.colay/config.toml` project policy.
- Make plan conversations work in non-Git directories without creating tasks or worktrees.
- Materialize writable tasks only after interview, validation, exact-revision approval, and Git
  preflight.
- Make one daemon the only writer during normal operation and eliminate cross-process SQLite
  writer competition.
- Automatically and safely migrate the global schema before provider compatibility checks.
- Import an encountered legacy repository-local state store exactly once without modifying it.
- Preserve append-only audit semantics, redaction, approval gates, and isolated worktrees.
- Provide actionable diagnostics and Windows/WSL regression coverage without real provider
  inference.

## Non-goals

- Sharing or synchronizing one SQLite file between Windows and WSL.
- Inferring that two clones are the same workspace from a remote URL.
- Scanning a home directory for legacy `.colay` stores.
- Automatically committing user changes, initializing Git, taking over an uncertain worker, or
  deleting legacy state, backups, or worktrees.
- Storing credentials or user-specific absolute paths in project policy.
- Weakening provider compatibility, approval, redaction, or audit requirements.
- Adding external telemetry or unofficial provider interfaces.

## Selected Architecture

Colay uses a single global database and a single normal-operation writer daemon. Alternatives were
rejected as follows:

- A global catalog plus per-workspace databases preserves the fragmentation and repeated migration
  burden this design removes.
- A global database opened directly by every CLI process continues to depend on WAL timeouts and
  cannot structurally prevent `SQLITE_BUSY`.

The daemon owns a serialized write queue. CLI and TUI clients use authenticated, user-local IPC for
normal reads and writes. Provider processes and filesystem operations run outside SQLite
transactions. Exclusive maintenance commands are the only processes allowed to open the database
directly, and only after the daemon has stopped or entered maintenance mode.

## Global Paths and Platform Boundary

Default paths are resolved from the current OS environment:

| Purpose | Linux and WSL | Windows |
| --- | --- | --- |
| State database | `${XDG_STATE_HOME:-~/.local/state}/colay/state.db` | `%LOCALAPPDATA%\\Colay\\state\\state.db` |
| Workspace files | `${XDG_DATA_HOME:-~/.local/share}/colay/workspaces/<workspace_id>/` | `%LOCALAPPDATA%\\Colay\\data\\workspaces\\<workspace_id>\\` |
| User config | `${XDG_CONFIG_HOME:-~/.config}/colay/config.toml` | `%APPDATA%\\Colay\\config.toml` |
| IPC | `$XDG_RUNTIME_DIR/colay/` or a permission-checked user runtime directory | A named pipe restricted to the current user SID |

`COLAY_HOME` is an explicit override for tests and portable installations. The current directory
must never implicitly change the global state root. Windows and WSL resolve their own defaults and
must not share a database even when both can see the same checkout.

The global workspace file tree owns artifacts, checkpoints, handovers, backups associated with a
workspace, and managed Git worktrees. No state database, event log, lease, or workspace marker is
written into the project. The optional project policy file is the only supported local Colay file.

## Workspace Identity and Registry

`workspace_id` is a UUIDv7 assigned at first registration and retained until explicit archival.
Identity is stored in the global registry, not in `.git` or `.colay`.

The registry contains:

- `workspaces(workspace_id, kind, status, created_at, last_seen_at)` where `kind` is `git` or
  `directory` and `status` is `active`, `detached`, or `archived`;
- `workspace_paths(workspace_id, canonical_path, comparison_key, git_common_dir, is_current,
  first_seen_at, last_seen_at)` for current and historical paths;
- a unique active-path comparison key appropriate to the platform.

For a Git checkout, initial registration uses its canonical Git common directory. Colay-managed
worktrees inherit the source workspace identifier. Separately cloned repositories remain separate
workspaces even if their remotes match. For a non-Git directory, registration uses its canonical
directory path.

Windows path comparison is case-insensitive after platform-safe normalization. Linux and WSL path
comparison is case-sensitive. Symlink resolution must not escape path-safety checks.

A move or rename is never guessed. The operator uses
`colay workspace attach <workspace_id> <new-path>` (with `move` as a discoverable alias) to validate
and record a new current path. Historical paths remain available for audit. Attaching a path that
belongs to another active workspace fails closed.

## Data Partitioning and Audit Chains

Every workspace-owned row carries a non-null `workspace_id`. This includes sessions, conversation
turns, requirements, validation results, graph proposals, approvals, tasks, attempts, leases,
commands, checkpoints, handovers, integrations, artifacts, and events.

Cross-table relationships use composite uniqueness and foreign keys such as
`(workspace_id, task_id)` so a globally unique-looking entity identifier cannot be referenced from
another workspace accidentally. Repository-global queries become explicitly workspace-scoped;
user-global list views must opt into cross-workspace results and return the workspace identity with
each row.

Each workspace has its own event sequence and hash-chain head. Event append verifies and advances
that workspace's head in the same transaction as the corresponding state mutation. A damaged or
missing chain in one workspace does not rewrite or renumber another workspace's history.

Only genuinely user-global data omits `workspace_id`: schema metadata, daemon instance and
maintenance ownership, global configuration metadata, provider executable fingerprints, and
provider compatibility assessments.

## Daemon, IPC, and SQLite Ownership

There is one daemon per OS user environment. If no daemon is running, an ordinary CLI command may
start it and wait for a bounded readiness result. An OS-level singleton lock prevents duplicate
instances. Unix sockets use owner-only permissions; Windows named pipes admit only the current user
SID.

Normal operation follows these rules:

- CLI and TUI processes do not open the SQLite database directly.
- A single daemon writer queue serializes state-changing operations.
- Daemon read operations use bounded read connections or the writer connection as appropriate.
- WAL, foreign keys, and a bounded busy timeout remain defense in depth, not the concurrency model.
- Provider calls, Git commands, file copies, and content hashing occur outside write transactions.
- The scheduler is user-global, while task leases and configured limits are workspace-scoped.

Daemon startup order is:

1. acquire the singleton or maintenance lock;
2. locate or create the global database;
3. inspect schema and integrity;
4. create a backup and apply bundled forward migrations when required;
5. expose diagnostic-only service if the database is newer than the binary;
6. publish IPC readiness;
7. refresh required runtime state, inspect providers, and recover interrupted work.

Provider safe mode is evaluated after state maintenance and cannot block database creation,
backup, forward migration, restore inspection, or `doctor state`.

## Lease and Resume Semantics

Leases include `workspace_id`, entity identity, daemon instance identity, heartbeat, and expiry.
`colay resume <task-id>` against a task owned by the healthy current daemon attaches to the existing
status and output stream instead of returning a lease conflict.

After daemon restart, leases owned by another daemon instance are inspected before becoming
recoverable. Expiry alone is not proof that a provider or worker stopped; Colay must not launch a
duplicate writable attempt until process and worktree ownership are reconciled. Forced takeover is
a separate explicit operation that records an audit event and is never the default `resume`
behavior.

## Conversation-First Command Semantics

`colay run <prompt>` creates or advances a plan-first conversation, not a writable task. The state
flow is:

`conversation -> interview -> plan revision -> validation -> awaiting exact approval -> writable preflight -> worktree task`

Before approval:

- Git is optional, including for the current user home directory;
- the orchestrator provider runs with read-only authority;
- workspace files, task rows, task attempts, worktrees, coordinator leases, and worker leases are
  not created;
- durable session messages, immutable requirement revisions, validation evidence, and proposed
  graph revisions may be stored in the global database.

`colay run --plan-only <prompt>` adds a hard fence: the session can interview, validate, and retain
a plan, but that invocation cannot promote the plan to a task. A later explicit promotion starts
from the persisted exact plan revision.

Approval binds the workspace, plan revision hash, validation evidence, provider, and base revision.
Any new requirement message, validation change, provider authority change, or Git base drift
invalidates the old approval. Non-interactive approval must name the exact plan revision; Colay
does not accept blanket advance approval.

Writable preflight checks, in order:

1. the current workspace is a Git repository;
2. `HEAD` resolves to a commit;
3. repository identity and the validated base revision have not changed;
4. the selected provider is eligible for writable work;
5. approval policy and workspace concurrency limits permit materialization.

Failure leaves the conversation intact and reports a product-level next action. Raw, expected Git
errors such as `not a git repository` and `Needed a single revision` are not shown. Only after all
checks pass does Colay create a task and isolated worktree under
`workspaces/<workspace_id>/worktrees/<task_id>/`. Colay never commits or cleans user changes.

A runtime error that suggests provider compatibility drift fails only the current attempt, retains
the resumable task, and directs the user to `colay doctor`. It does not automatically downgrade or
route to another provider.

## Configuration Model

Ordinary configuration precedence is:

`explicit CLI option -> .colay/config.toml -> user-global config.toml -> compiled default`

Approval and security constraints compose by selecting the strictest effective policy; project
configuration and ordinary CLI flags cannot weaken a user-global safety requirement.

The optional `.colay/config.toml` is a shareable project-policy file. It may select providers and
declare task, validation, or approval rules. It may not contain credentials, tokens, workspace
identifiers, mutable task state, or user-specific absolute paths. Secrets remain in official
provider authentication stores or the user's environment. Absence of either local or global config
uses validated defaults and never prevents database migration.

## Global Schema Migration and Backups

Before a forward migration, Colay uses the SQLite backup API to create a verified global backup.
Migration history stores ordered versions and checksums. Migrations run transactionally when
SQLite permits. Failure prevents normal daemon readiness and reports the backup and
`colay doctor state` path. Colay never auto-downgrades a database that is newer than its binary.

`colay migrate apply` remains as an idempotent compatibility command. With no pending migrations it
succeeds. When work is required it uses exclusive maintenance mode or asks the daemon to shut down
cleanly. Provider compatibility status is not an authorization gate for state maintenance.

Initial releases retain migration and import backups until an explicit, confirmed cleanup command.
They do not delete worktrees as part of backup cleanup.

## Legacy Repository-State Import

When entering a workspace for the first time, Colay checks only that workspace for a legacy local
state store. It never scans parent homes or unrelated repositories.

Import is read-only with respect to the legacy source:

1. inspect its schema, integrity, WAL state, and lock ownership;
2. create a source backup and a pre-import global backup;
3. allocate or select the target workspace;
4. copy artifacts, checkpoints, and handovers into a global staging directory;
5. verify content hashes;
6. import records in one global transaction with `workspace_id`;
7. atomically publish staged files;
8. persist the source fingerprint, manifest hash, mappings, and result.

The fingerprint combines stable SQLite identity and content evidence rather than only the source
path. Re-import of the same fingerprint is a successful no-op. Entity identifiers are retained
unless they collide, in which case deterministic replacement identifiers and their mappings are
audited.

For an empty target workspace, a valid legacy event chain is retained as its initial chain. When
the target already has events, the legacy chain is stored as an immutable segment and the current
workspace chain appends an import-anchor event containing the legacy root and manifest hashes.
Colay never rewrites two chains into a fabricated sequence. Missing or corrupt audit evidence
fails import instead of being silently discarded.

If the source is owned by a live legacy daemon, import stops with instructions to terminate that
daemon. A failed import removes only its global staging data and preserves both source state and
the pre-import global state. New binaries ignore a legacy local database after successful import
but continue to read the optional project policy.

## Doctor and Compatibility Cache

Default `colay doctor` checks global path permissions and space, database schema and integrity,
backup state, daemon singleton and IPC, workspace path and Git health, orphaned leases, artifact
references, workspace event chains, and every configured provider.

Provider assessments are global and keyed by provider kind plus canonical executable path, file
size, modification time, and reported version. Normal startup performs only a bounded version
probe and reuses a full fingerprint match. Doctor always performs fresh safe probes and stores the
new assessment through the daemon. `colay compatibility` remains an alias for
`colay doctor providers`.

Probes use only documented public version, help, or schema surfaces. They never start inference,
inspect credentials, scrape usage, or contact unofficial endpoints. If a newer database prevents
cache writes, doctor still reports fresh in-memory provider evidence and identifies that the result
could not be persisted.

## Error Contract

Expected errors include three parts: the failed condition, its scope, and a concrete next command.
Internal command stderr may be retained only after redaction and must not replace product-level
messages for known Git, SQLite, migration, provider, or lease states.

Representative outcomes are:

- non-Git planning: continue the session without a Git error;
- non-Git promotion: retain the plan and recommend initializing or selecting a committed
  repository;
- active lease resume: attach to the active daemon stream;
- provider writable incompatibility: retain plan access and recommend `colay doctor providers`;
- migration failure: retain the backup and recommend `colay doctor state`;
- newer schema: serve diagnostics, refuse writes, and identify the required binary version.

All diagnostics and audit payloads use existing redaction rules.

## Verification Strategy

Tests use `orchestrator-test-support` fake provider binaries. Tests and CI must never invoke real
Codex, Claude, Gemini, or Agy inference.

Required regression coverage includes:

- Windows home and non-Git WSL home start `colay run hello` as a plan conversation;
- `run --plan-only` interviews without Git, task rows, leases, or worktrees;
- old global schemas migrate before provider safe-mode decisions;
- missing local config does not prevent migration;
- concurrent CLI clients cannot produce `database is locked`;
- resume attaches to a healthy active task instead of conflicting;
- legacy state imports once and remains untouched;
- Windows and WSL resolve independent stores;
- Windows paths with spaces, Unicode, and case variation resolve consistently;
- crashes during migration or import preserve verified backups;
- composite foreign keys reject cross-workspace references;
- one corrupt workspace event chain does not alter another;
- read-only provider compatibility permits planning but blocks writable promotion;
- daemon and maintenance ownership reject a second writer;
- provider probes use separated executable arguments and no shell interpolation.

The repository-wide completion gate is:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Windows-native integration tests and WSL Linux smoke tests must both run with isolated temporary
`COLAY_HOME` values and fake providers. Any platform-only limitation is recorded in the nightly QA
tracker rather than hidden by conditional success.

## Rollout and Compatibility

The rollout is staged but each shipped stage must be internally usable:

1. add global path resolution, workspace registry, partitioned schema, and legacy import support;
2. move ordinary CLI/database traffic behind the user daemon and exclusive maintenance boundary;
3. switch run semantics to plan-first global sessions and exact approved promotion;
4. store and diagnose global provider assessments under the approved compatibility policy;
5. remove repository-local state writes after import coverage and Windows/WSL QA pass.

During transition, readers may recognize legacy state for import, but no new task or event is
written there. Release documentation identifies the retained source and global backup locations.
Rollback uses the source backup or a verified global backup; it never attempts schema downgrade in
place.

## Approved Decisions

- One physical SQLite database per user per OS environment.
- Windows and WSL databases are separate.
- One user-global daemon is the normal-operation writer.
- Workspace identity is a registry UUID and path moves require explicit attach/move.
- Audit chains are workspace-scoped.
- DB, artifacts, checkpoints, handovers, and managed worktrees use global storage.
- Non-Git directories support planning; Git is required only at writable promotion.
- Current-workspace legacy state is imported automatically, idempotently, and without deletion.
- Safe forward schema migrations run automatically before provider checks.
- Resume attaches to a healthy active lease.
- User-global config may be overridden by a shareable, non-secret local project policy.
- Default doctor performs deep state and provider checks.

