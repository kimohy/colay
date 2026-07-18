# Operations

## Initialize and diagnose

Run `colay init` once in the repository. It writes the local configuration, creates `.colay`, migrates a new SQLite database to the current schema, and creates/reconciles `events.jsonl`. Initialization does not invoke a provider.

`colay doctor` performs only non-inference checks: config validation, SQLite integrity/schema health when the database exists, event-log reconciliation when the log exists, and `<provider> --version` for configured CLIs. It does not prove sandbox behavior or start a model turn.

`colay compatibility` performs the deeper Codex-only capability probe. The probe allowlist is limited to version/help output and stable App Server schema generation. The possible startup classifications are:

- `Compatible`: an exact tested Codex version exposes the mandatory public contract.
- `CompatibleWithWarnings`: mandatory execution remains available but an optional capability is degraded.
- `Untested`: writable Codex work is blocked; read-only Codex is allowed only when its sandbox capability is present.
- `Incompatible`: Codex is disabled.

Codex incompatibility does not disable a usable Claude or Gemini adapter. An incompatible state, config, or handover schema blocks task execution rather than attempting an implicit downgrade.

When `.colay/config.toml` is absent, Colay can continue using a legacy `.codex/orchestrator/config.toml` in place and emits a warning. It never moves or copies live state automatically because persisted worktree and rollback paths may be absolute. If both config locations exist, startup fails closed and requires an explicit `--config` path; `colay init` also refuses to create a second state root over a legacy installation.

## Running and inspecting tasks

Use `run --plan-only` to persist an assessment and routing decision without creating a worktree or invoking a provider. A normal writable run creates a task branch/worktree, runs a bounded worker, checkpoints Git evidence, and independently verifies the result before completion.

`status`, `usage`, `providers`, `explain-routing`, and `compatibility` support global `--json`. `tui` renders the persisted five-panel snapshot and sends provider enable/disable, routing, handover, pause/resume/cancel, and usage-override actions through the same config/state APIs as the CLI.

## Control requests and recovery

`pause`, `cancel`, and `handover --to` append idempotent control records. A concurrently running orchestrator consumes them and reaches a safe checkpoint before acting.

`resume <task-id>` is the restart path for a paused, blocked, or interrupted non-terminal task. It validates the persisted worktree, sealed checkpoint/handover, task revision, and schema; converts an interrupted running/checkpoint/handover transition to an authoritative Git checkpoint when necessary; performs the persistence secret preflight; reroutes with current usage/health; and resumes through a vendor-neutral bundle. Inconsistent projections, missing worktrees, failed integrity, or unsafe persistence scans fail closed for administrator review.

SQLite and the hash-chained JSONL log retain tasks, attempts, checkpoints, handovers, leases, and worktree metadata across process restarts. Stale claimed pause/resume/cancel controls can be requeued safely; ambiguous handover/usage-override controls require manual reconciliation.

## Usage evidence

Usage collection priority is official structured output, an administrator-configured executable/argv probe, local execution ledger, manual override, then unknown. Interactive usage pages are never scraped. Missing values remain unknown.

The current manual command accepts `provider`, optional `--used`, `--limit`, and `--remaining`, plus the required audit label `--entered-by`. Period, scope, unit, and reset window come from provider configuration. Manual evidence is persisted with source `manual_override`; there is currently no expiration argument.

## Worktree retention

Worktrees and task branches are retained after completion, failure, cancellation, and rollback. The engine can produce a cleanup plan containing exact paths, but this release has no automatic worktree removal, merge, or push path.

## Provider prerequisites

An administrator installs and authenticates the approved Enterprise CLIs. Colay calls their public non-interactive interfaces and does not read credential stores. An empty model ID means “use the CLI's Enterprise default model.” Enable `effort_flag_enabled` only after the administrator has confirmed that the installed Claude contract accepts the configured effort flag.
