# Operations

## Initialize and diagnose

`colay init` writes a minimal versioned repository policy override when an editable local policy is wanted. Durable orchestration state is not repository-local: one user daemon owns one SQLite database per native OS user environment, and each encountered directory receives a non-null workspace UUID. Windows and WSL therefore use separate native state roots and never share a SQLite file. Initialization and state maintenance do not invoke provider inference. `doctor`, `status`, `compatibility`, and `run --plan-only` do not create `.colay` state unless an explicit local policy edit is requested.

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

The personal `$COLAY_HOME` layer and `$COLAY_CONFIG` provide configuration inputs. Missing local and global layers use the same validated compiled defaults and cannot block global state creation, doctor, or forward migration. The legacy `state_dir` setting is consulted only while inspecting the current workspace for a repository-local import source; new durable records always use the user-global database and workspace data root. When neither explicit selector is used, Colay may discover `.colay/config.toml` or the legacy `.codex/orchestrator/config.toml` as a project policy. If both are present, automatic resolution fails closed; use `--config` to select one explicitly.

`colay doctor` performs only non-inference checks. Under exclusive maintenance ownership it opens only an existing, current-schema database read-only; it never creates or migrates the database, creates a backup or artifact directory, or registers or refreshes a workspace. A missing database, a pending migration, or an unregistered repository is reported as a warning with the explicit writable command needed to repair it. When the user daemon is live, doctor preserves an explicit `--config` selection while connecting and consumes one versioned, typed workspace-diagnostics response. A missing, malformed, mismatched, or unsupported response fails every dependent integrity category closed.

Doctor reports global path/database integrity and schema, daemon runtime identity, the current workspace UUID/path, that workspace's event-chain head, every persisted artifact reference and its hash, Git readiness, the running CLI build/target, and every configured provider's safe public compatibility assessment. A CLI/daemon identity mismatch is a warning that requires restarting the user daemon with the intended binary. Successful executable resolution includes configured path, canonical resolved path, and executable kind. Doctor never starts a model turn, inspects credentials, or scrapes usage.

Executable resolution is platform-specific but shared by diagnostics and execution. On Windows, a bare executable name is searched through the effective `PATH` using only `.exe`, `.com`, `.cmd`, and `.bat` entries from `PATHEXT`, in `PATHEXT` order; matching is case-insensitive and `.cmd`/`.bat` are reported as command scripts. A bare Unix name must be a regular file with an executable permission bit. An explicit path is resolved from the working directory when relative, and a missing explicit path does not fall back to `PATH`.

`colay doctor providers` refreshes all configured providers through the existing safe assessment APIs. `colay compatibility` is a behavioral alias with the same provider report; only the JSON envelope command label differs. Because this supersedes the older Codex-only compatibility payload, both commands identify the multi-provider envelope and data contract as schema v2 rather than silently reusing v1. Probe allowlists remain limited to documented version/help/schema surfaces. The possible Codex startup classifications are:

- `Compatible`: an exact tested Codex version exposes the mandatory public contract.
- `CompatibleWithWarnings`: mandatory execution remains available but an optional capability is degraded.
- `Untested`: writable Codex work is blocked; read-only Codex is allowed only when its sandbox capability is present.
- `Incompatible`: Codex is disabled.

Codex incompatibility does not disable a usable Claude, Agy, or Gemini adapter. Agy and Gemini are configured and routed independently. An incompatible state, config, or handover schema blocks task execution rather than attempting an implicit downgrade.

When `.colay/config.toml` is absent, Colay can continue using a legacy `.codex/orchestrator/config.toml` in place and emits a warning. It never moves or copies live state automatically because persisted worktree and rollback paths may be absolute. If both config locations exist, startup fails closed and requires an explicit `--config` path; `colay init` also refuses to create a second state root over a legacy installation.

## Running and inspecting tasks

Use `run --plan-only` to persist an assessment and routing decision without creating a worktree or invoking a provider. It is a static compatibility command, not the conversation-first provider interview. A normal writable run first resolves the repository root, verifies `HEAD^{commit}`, and rejects unresolved Git operations before it creates `.colay` state or a task. It then creates a task branch/worktree, runs a bounded worker, checkpoints Git evidence, and independently verifies the result before completion.

If a normal run reports that the directory is not a Git repository, move to the intended project
repository. If it reports that the repository has no base commit, review the intended initial file
set and create an initial commit. Do not use a broad `git add .` in an arbitrary parent workspace:
it can capture credentials, dependency directories, or nested repositories.

On WSL, prefer a Linux-native clone under the distribution filesystem (for example
`~/workspace/project`) for Linux Colay. `doctor` warns when the active checkout is under
`/mnt/<drive>/...` because Windows Git and WSL Git can apply different line-ending and permission
rules to the same files. Use Windows Colay with Windows Git for a Windows checkout; do not alternate
Windows and WSL Git against one working tree. Review `git status --short` before writable approval.

`status`, `usage`, `providers`, `doctor`, `explain-routing`, and `compatibility` support
global `--json`. `colay tui [task-id]` opens the durable chat workspace and
starts the daemon when needed. The header reports `online`, `stale`, or
`offline`; stale/offline workspaces remain readable but messages and task
controls are rejected. Run `colay daemon restart` from another terminal, then
the open workspace reconnects on its 200ms refresh cycle.

A session-level message automatically queues a read-only official-CLI
conversation turn. A normal answer ends with only the durable timeline; an
incomplete implementation request records an immutable requirement revision and
asks a follow-up question. Only a complete `worktree_task_candidate` queues graph
planning. Git is checked at that promotion boundary, so non-Git directories and
repositories without an initial commit remain usable for conversation and show
preparation guidance without creating tasks or leases.

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
/plan             revalidate the newest complete requirement (read-only)
/integrate        build a read-only sealed result preview
/approve          confirm the exact current graph or integration hash
/resolve          create one task for a resolvable integration conflict
/admin            five-panel administration compatibility view
```

Task selection never changes the composer target. `@task-<id>` is a one-message
override that atomically records an ordered instruction for that graph task.
`@all` fans the same redacted instruction out into separate durable rows for
every current non-terminal graph task, preserving per-task audit identity.
`/plan` is an explicit compatibility trigger for the newest final session-level
user message, but it cannot bypass the provider interview or manufacture approval
authority. It succeeds only when the latest immutable requirement revision is complete;
complete conversation candidates queue the same durable read-only planning request
automatically. The plan card shows its requirement revision,
validation hash, base commit, validation checks, revision, SHA-256 proposal
hash, ordered nodes, dependencies, scopes, providers/profiles, risks, and
parallelism. `/approve` is enabled only for the validated current requirement
and repository revision while the daemon is online. Only `y` confirms;
`n`/`Esc` cancels, a newer user message hides the stale card, and a changed hash
or Git `HEAD` rejects approval. Typing "yes" in chat remains an ordinary message.

Before approval there are no writable tasks, worktrees, or worker leases. Exact
approval materializes queued tasks and dependency rows once. The daemon claims
dependency-ready tasks subject to `max_parallel_workers`, optional
`provider_parallel_limits`, and non-overlapping normalized write scopes. Each
claim creates one isolated worktree and one provider attempt. A task completes
only after checkpoint sealing and verification; failure releases its claim and
does not cancel an independent sibling. An invalid plan is retained with a
redacted attention error and no approvable hash. Send a correcting message so the
interview creates a new requirement revision, then allow automatic planning or re-run
`/plan`; earlier revisions remain historical. Chat `/retry` still fails visibly as
unavailable. A persisted non-terminal task that stopped after reaching `planned` is
restarted explicitly with `colay resume <task-id>`; this reuses its task identity and
creates at most one managed worktree instead of materializing a duplicate task.

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

## User daemon and maintenance ownership

Use `colay daemon start` to initialize missing user-global state, register the
current workspace, and launch the single per-user background service. Repeating
the command from any workspace attaches to that same daemon and database.
`colay daemon status` reports `stopped`, `online`, or `stale`; `stop` requests a
graceful release and is idempotent when no daemon exists. `restart` waits for the
singleton owner and lease to be released before starting a replacement.

Ordinary CLI and TUI mutations go through the daemon writer queue. Explicit
`doctor`, `migrate`, migration rollback, and release rollback maintenance may open SQLite directly
only after acquiring the same OS singleton lock. A live daemon therefore makes
offline migration or rollback fail with an ownership diagnostic instead of admitting a
second writer. Release rollback plans, approvals, audit, quiescence checks, and artifacts use the
registered global workspace rather than repository-local state. Stop the daemon and repeat the maintenance command. Provider safe
mode is never authority for database creation, backup, forward migration, import
inspection, or state doctor; future-schema writes still fail closed.

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

The global SQLite database retains tasks, attempts, checkpoints, handovers, leases, artifact references, and workspace-scoped hash chains across process restarts. Workspace files, including artifacts and managed worktrees, live below the matching UUID directory. Stale claimed pause/resume/cancel controls can be requeued safely; ambiguous handover/usage-override controls require manual reconciliation.

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
