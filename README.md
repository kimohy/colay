# Colay

[![License: Apache-2.0](https://img.shields.io/badge/License-Apache--2.0-blue.svg)](LICENSE)

## Install

Requires Node.js 22 or newer. Install the stable channel (the default) with:

```text
npm install --global @kimohy/colay
colay --version
```

Beta and nightly channels are available when you explicitly opt in:

```text
npm install --global @kimohy/colay@beta
npm install --global @kimohy/colay@nightly
```

Colay currently supports Windows x64, macOS Apple Silicon (ARM64), and Linux
x64. The Linux x64 package contains a musl-linked binary and deliberately has
no npm `libc` selector, so it can install on both musl and glibc Linux hosts.
For beta and stable builds without Node.js, download the matching archive from
[GitHub Releases](https://github.com/kimohy/colay/releases). Nightly workflow
artifacts expire after 14 days; npm is the normal way to install a nightly.

Colay is a local-first enterprise orchestrator for approved Codex CLI, Claude Code, and Gemini CLI installations. It selects a provider and logical model profile, records why it made that decision, preserves work in isolated Git worktrees, and can resume from a vendor-neutral checkpoint after a provider becomes unavailable.

The orchestrator never rotates identities, bypasses quotas, scrapes usage pages, extracts credentials, purchases credits, or calls unofficial provider endpoints. Provider inference is performed only by the official CLIs with their existing authenticated state. Tests use fake binaries and consume no provider credit.

## Commands

```text
colay init
colay daemon {start|status|stop|restart}
colay run "<task>"
colay run --task-file task.json
colay run --plan-only "<task>"
colay status [--json]
colay providers [--json]
colay providers {enable|disable} <provider>
colay profiles [--json]
colay profiles set <provider> <profile> --model <id> [--effort low|medium|high]
colay profiles reset <provider> <profile>
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

`colay daemon start` launches one repository-local background service and is
idempotent while that service has a healthy lease. Use `daemon status`,
`daemon stop`, or `daemon restart` to manage it; `--json` emits the same stable
command envelope used by the rest of the CLI. The daemon stores control state
only in the repository SQLite database and does not open a network listener.
This durable-session phase does not yet schedule task graphs or replace the
current TUI; those capabilities build on this lifecycle foundation.

## Model profiles

Colay analyzes each task and automatically selects `economy`, `standard`, or `premium`; users do not choose a model per run. The built-in mappings below are current as of 2026-07-20:

| Provider | Economy (`low`) | Standard (`medium`) | Premium (`high`) |
| --- | --- | --- | --- |
| Codex | `gpt-5.6-luna` | `gpt-5.6-terra` | `gpt-5.6-sol` |
| Claude | `claude-haiku-4-5` | `claude-sonnet-5` | `claude-fable-5` |
| Gemini | `gemini-3.1-flash-lite` | `gemini-3.5-flash` | `gemini-3.1-pro-preview` |

Inspect effective settings with `colay profiles` or `colay profiles --json`. Administrators can override one entry with `colay profiles set <provider> <profile> --model <id> [--effort low|medium|high]`, and remove the writable-layer override with `colay profiles reset <provider> <profile>`. Reset reveals the compiled or lower-precedence value; it does not delete it. The TUI exposes the same matrix and editor under `f:profiles`.

Layered TOML remains supported for managed deployments. A configured model must also be available to the installed official provider CLI and the authenticated account; a preset does not grant model access.

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
