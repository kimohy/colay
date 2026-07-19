# Colay

Colay is a local-first enterprise orchestrator for approved Codex CLI, Claude Code, and Gemini CLI installations. It selects a provider and logical model profile, records why it made that decision, preserves work in isolated Git worktrees, and can resume from a vendor-neutral checkpoint after a provider becomes unavailable.

The orchestrator never rotates identities, bypasses quotas, scrapes usage pages, extracts credentials, purchases credits, or calls unofficial provider endpoints. Provider inference is performed only by the official CLIs with their existing authenticated state. Tests use fake binaries and consume no provider credit.

## Commands

```text
colay init
colay run "<task>"
colay run --task-file task.json
colay run --plan-only "<task>"
colay status [--json]
colay providers [--json]
colay providers {enable|disable} <provider>
colay usage [--json]
colay usage override <provider> --entered-by <audit-identity> [--used N] [--limit N] [--remaining N]
colay handover <task-id> --to <provider>
colay pause <task-id>
colay resume <task-id>
colay cancel <task-id>
colay explain-routing <task-id> [--json]
colay checkpoint <task-id> [--json]
colay doctor [--json]
colay compatibility [--json]
colay migrate {status|plan}
colay migrate apply [--dry-run]
colay migrate rollback plan [--backup <local-backup>]
colay migrate rollback apply --plan-hash <sha256> --approved-by <identity>
colay rollback plan --to <version>
colay rollback apply --to <version> --plan-hash <sha256> --approved-by <identity>
colay tui [task-id]
```

## Configuration

Colay resolves versioned TOML configuration in this order, with later layers overriding earlier ones:

```text
compiled defaults
< $COLAY_HOME/config.toml
< <repository>/.colay/config.toml
< $COLAY_CONFIG
< --config
```

`COLAY_HOME` defaults to `~/.colay` on Unix and `%USERPROFILE%\.colay` on Windows. Configuration files are partial overrides: tables merge by key, while arrays replace the lower-precedence array rather than concatenate. Every loaded file must declare the supported `config_version`. Absent automatic layers are ignored, but normal runtime commands fail when an explicitly selected `$COLAY_CONFIG` or `--config` file is missing. `init` is the creation-path exception: it treats a missing explicit selector as the destination for its new minimal override.

Repository state remains local to the repository (by default, `.colay`); personal defaults and environment-selected configuration are global inputs, not a global state directory. A legacy `.codex/orchestrator/config.toml` is discovered without moving its state. If automatic discovery finds both legacy and current repository configuration, Colay fails closed until `--config` explicitly selects one. `init` writes a minimal configuration override and initializes state safely. Other read-only commands do not create repository state, but the first `run`—including `run --plan-only`—initializes and persists repository state. Start from [`config.example.toml`](config.example.toml).

See [`docs/architecture.md`](docs/architecture.md), [`docs/security.md`](docs/security.md), [`docs/operations.md`](docs/operations.md), [`docs/compatibility.md`](docs/compatibility.md), [`docs/testing.md`](docs/testing.md), and [`docs/release.md`](docs/release.md) for the implemented boundary and current limitations.
