# Operations

## Initialize and diagnose

Run `colay init` once in the repository. It writes a minimal versioned override (rather than a materialized copy of every default), creates `.colay`, migrates a new SQLite database to the current schema, and creates/reconciles `events.jsonl`. Initialization does not invoke a provider. Other read-only commands do not create repository state; the first `colay run`, including `run --plan-only`, creates and persists repository state safely if it is absent.

## Configuration resolution

Configuration is resolved as versioned partial overrides in this precedence order:

```text
compiled defaults
< $COLAY_HOME/config.toml
< <repository>/.colay/config.toml
< $COLAY_CONFIG
< --config
```

`COLAY_HOME` defaults to `~/.colay` on Unix and `%USERPROFILE%\.colay` on Windows. Layers merge table values by key; arrays replace the lower-precedence value and never concatenate. Each loaded layer must carry the current supported `config_version`. Missing automatic layers are allowed, but normal runtime commands require a path selected through `$COLAY_CONFIG` or `--config` to exist. `init` instead treats a missing explicit selector as the destination for its new minimal override. A malformed or unsupported loaded layer fails startup rather than being skipped.

The personal `$COLAY_HOME` layer and `$COLAY_CONFIG` provide configuration inputs only. The effective `state_dir` is constrained beneath the repository, so task state remains repository-local. When neither explicit selector is used, Colay discovers either `.colay/config.toml` or the legacy `.codex/orchestrator/config.toml` without moving live state. If both are present, automatic resolution fails closed; use `--config` to select one explicitly.

`colay doctor` performs only non-inference checks: config validation, SQLite integrity/schema health when the database exists, event-log reconciliation when the log exists, and `<provider> --version` for configured CLIs. Only a successful provider version check includes structured configured-executable, resolved-executable, and executable-kind evidence. Failed resolution, spawn, or nonzero version checks report their detail without that structured evidence. Doctor does not prove sandbox behavior or start a model turn.

Executable resolution is platform-specific but shared by diagnostics and execution. On Windows, a bare executable name is searched through the effective `PATH` using only `.exe`, `.com`, `.cmd`, and `.bat` entries from `PATHEXT`, in `PATHEXT` order; matching is case-insensitive and `.cmd`/`.bat` are reported as command scripts. A bare Unix name must be a regular file with an executable permission bit. An explicit path is resolved from the working directory when relative, and a missing explicit path does not fall back to `PATH`.

`colay compatibility` performs the deeper Codex-only capability probe. The probe allowlist is limited to version/help output and stable App Server schema generation. The possible startup classifications are:

- `Compatible`: an exact tested Codex version exposes the mandatory public contract.
- `CompatibleWithWarnings`: mandatory execution remains available but an optional capability is degraded.
- `Untested`: writable Codex work is blocked; read-only Codex is allowed only when its sandbox capability is present.
- `Incompatible`: Codex is disabled.

Codex incompatibility does not disable a usable Claude or Gemini adapter. An incompatible state, config, or handover schema blocks task execution rather than attempting an implicit downgrade.

When `.colay/config.toml` is absent, Colay can continue using a legacy `.codex/orchestrator/config.toml` in place and emits a warning. It never moves or copies live state automatically because persisted worktree and rollback paths may be absolute. If both config locations exist, startup fails closed and requires an explicit `--config` path; `colay init` also refuses to create a second state root over a legacy installation.

## Running and inspecting tasks

Use `run --plan-only` to persist an assessment and routing decision without creating a worktree or invoking a provider. A normal writable run creates a task branch/worktree, runs a bounded worker, checkpoints Git evidence, and independently verifies the result before completion.

`status`, `usage`, `providers`, `explain-routing`, and `compatibility` support
global `--json`. `colay tui [task-id]` opens the durable chat workspace and
starts the daemon when needed. The header reports `online`, `stale`, or
`offline`; stale/offline workspaces remain readable but messages and task
controls are rejected. Run `colay daemon restart` from another terminal, then
the open workspace reconnects on its 200ms refresh cycle.

The text layout and bindings are:

```text
wide (>=110):  tasks | conversation | inspector
medium (80-109): tasks | conversation, inspector overlay
narrow (60-79): one primary view selected by focus/overview
compact (<60): status and resize guidance, no mutation

Tab / Shift+Tab   traverse panes
Ctrl+P, /tasks    task switcher
Ctrl+O            overview
Ctrl+L            full log
Ctrl+T            explicit composer target
?                 help
/plan             plan the newest session-level user goal (read-only)
/integrate        build a read-only sealed result preview
/approve          confirm the exact current graph or integration hash
/resolve          create one task for a resolvable integration conflict
/admin            five-panel administration compatibility view
```

Task selection never changes the composer target. `@task-<id>` is a one-message
override that atomically records an ordered instruction for that graph task.
`@all` fans the same redacted instruction out into separate durable rows for
every current non-terminal graph task, preserving per-task audit identity.
`/plan` selects only the newest final, session-level user message and
submits a durable read-only planning request. The resulting plan card shows its
revision, SHA-256 proposal hash, ordered nodes, dependencies, scopes,
providers/profiles, risks, and parallelism. `/approve` is enabled only for a
validated current revision while the daemon is online. Only `y` confirms;
`n`/`Esc` cancels, and a refresh with a different hash closes the overlay.
Typing "yes" in chat remains an ordinary message.

Before approval there are no writable tasks, worktrees, or worker leases. Exact
approval materializes queued tasks and dependency rows once. The daemon claims
dependency-ready tasks subject to `max_parallel_workers`, optional
`provider_parallel_limits`, and non-overlapping normalized write scopes. Each
claim creates one isolated worktree and one provider attempt. A task completes
only after checkpoint sealing and verification; failure releases its claim and
does not cancel an independent sibling. An invalid plan is retained with a
redacted attention error and no approvable hash. Re-run `/plan` after correcting
the goal to create a new revision; earlier revisions remain historical.
`/retry` still fails visibly as unavailable.

After intended graph tasks complete, `/integrate` recomputes managed Git
snapshots and sealed checkpoint/verification evidence. The preview card shows
the base, exact hash, ordered sources and changed files, blockers, and retained
destination. Previewing never creates that destination. Only `y` in the
integration approval overlay submits typed authority for the displayed hash.
Any source or base change invalidates it. Missing evidence, failed verification,
overlap, stale base, or patch failure stops closed. Evidence and verification
failures require remediation in the source task. For a path overlap or failed
application, `/resolve` materializes one idempotent task bound to the batch;
completing it grants no authority, and `/integrate` plus `/approve` must run
again.

## Repository daemon

Use `colay daemon start` to initialize missing repository state and launch the
single local background service. A repeated start returns the existing healthy
instance. `colay daemon status` is read-only and reports `stopped`, `online`, or
`stale`; it does not create state when the database is absent. `stop` requests a
graceful release and is idempotent when no daemon exists. `restart` waits for the
previous lease to be released or expire before starting a replacement.

The hidden `daemon serve` action is an internal child-process entry point. The
service heartbeats once per second with a five-second lease, processes durable
session/message/planning/approval commands every 100ms, and schedules approved
tasks without blocking heartbeat or stop handling. Read-only planning and task
execution run in owned cancellable children. Task claims renew while an attempt
is active; a crashed daemon leaves expiring claims and restart reconciliation
does not create a second attempt for already completed work. A replacement may
take daemon ownership only at or after lease expiry. There is no network
endpoint.

## Control requests and recovery

`pause`, `cancel`, and `handover --to` append idempotent control records. A concurrently running orchestrator consumes them and reaches a safe checkpoint before acting.

`resume <task-id>` is the restart path for a paused, blocked, or interrupted non-terminal task. It validates the persisted worktree, sealed checkpoint/handover, task revision, and schema; converts an interrupted running/checkpoint/handover transition to an authoritative Git checkpoint when necessary; performs the persistence secret preflight; reroutes with current usage/health; and resumes through a vendor-neutral bundle. Inconsistent projections, missing worktrees, failed integrity, or unsafe persistence scans fail closed for administrator review.

SQLite and the hash-chained JSONL log retain tasks, attempts, checkpoints, handovers, leases, and worktree metadata across process restarts. Stale claimed pause/resume/cancel controls can be requeued safely; ambiguous handover/usage-override controls require manual reconciliation.

Client commands use unique idempotency keys. Stale claimed session creation,
message append, planning, graph approval, and preview commands are reconciled or
requeued on recovery. Integration application is never blindly replayed: an
ambiguous `applying` record becomes `interrupted`, and its batch/session become
`needs_attention`. A stale
`stop_daemon` command remains claimed for manual reconciliation because blind
replay could stop a replacement instance.

The TUI redacts message text before it enters `client_commands`; the daemon
redacts again before writing `conversation_messages`. Exact projection matching
allows a crash after insertion to finish on replay without duplicating a
message. A mismatched projection fails closed. Closing the TUI does not stop the
daemon; use the explicit daemon command for lifecycle changes.

## Usage evidence

Usage collection priority is official structured output, an administrator-configured executable/argv probe, local execution ledger, manual override, then unknown. Interactive usage pages are never scraped. Missing values remain unknown.

The current manual command accepts `provider`, optional `--used`, `--limit`, and `--remaining`, plus the required audit label `--entered-by`. Period, scope, unit, and reset window come from provider configuration. Manual evidence is persisted with source `manual_override`; there is currently no expiration argument.

## Worktree retention

Worktrees and task branches are retained after completion, failure,
cancellation, daemon restart, and rollback. Approved results may be copied only
to a separately retained integration worktree. Colay has no automatic worktree
removal, merge to the user's branch, push, or publication path.

## Provider prerequisites

An administrator installs and authenticates the approved Enterprise CLIs. Colay calls their public non-interactive interfaces and does not read credential stores. An empty model ID means “use the CLI's Enterprise default model.” Enable `effort_flag_enabled` only after the administrator has confirmed that the installed Claude contract accepts the configured effort flag.
