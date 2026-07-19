# Layered Configuration, Windows Executable Resolution, and Codex 0.144.6 Design

## Objective

Colay must work safely before a repository `config.toml` exists, while allowing
users to override only the settings they need. Configuration must support a
cross-platform user-wide home, repository-specific overrides, explicit
environment and command-line overrides, deterministic Windows executable
resolution, and an exact tested contract for Codex CLI 0.144.6.

The change must preserve schema versions, append-only audit semantics,
redaction, explicit approval gates, provider-neutral domain boundaries, and
repository-local worker isolation.

## Configuration Model

Colay resolves effective configuration from these layers, from lowest to
highest precedence:

1. Compiled safe defaults.
2. `$COLAY_HOME/config.toml`.
3. `<repository>/.colay/config.toml`.
4. The file named by `$COLAY_CONFIG`.
5. The file named by `--config`.

`COLAY_HOME` defaults to `~/.colay` on Linux and macOS and to
`%USERPROFILE%\.colay` on Windows. `COLAY_HOME` controls user-wide Colay
configuration only. Repository databases, JSONL audit logs, checkpoints,
handover artifacts, backups, and worktrees remain under the repository-local
state root.

The standard global and repository configuration files are optional. A path
explicitly supplied through `COLAY_CONFIG` or `--config` is required to exist;
a missing explicit path is an error. `--config` has higher precedence than
`COLAY_CONFIG`.

Configuration files are versioned partial overrides. Each persisted file must
declare `config_version`; the declared version is validated before its values
are merged. Tables merge recursively. Scalars replace lower-precedence values.
Arrays replace the complete lower-precedence array rather than concatenating.
Unknown fields and comments remain preserved in their source document for
administrative edits and migrations.

Existing complete version-4 configuration files remain valid. Existing legacy
`.codex/orchestrator/config.toml` discovery remains supported. If both legacy
and current repository files exist, Colay continues to fail closed until the
administrator selects the intended file explicitly.

## Compiled Defaults

Compiled defaults form a complete valid `RootConfig` at schema version 4. They
use portable values such as UTC for time zones, `.colay` for repository state,
bounded worker concurrency, existing conservative routing thresholds, unknown
quota limits, provider-defined quota units, no external telemetry, and no
organization-specific redaction patterns.

Provider defaults contain the existing public executable names and safe quota
period metadata. Missing provider binaries remain unavailable rather than
causing values or quota data to be invented. A provider is eligible only after
its configured executable and required safe interfaces are detected.

Colay-specific discovery variables are consumed by Colay and are not added to
provider subprocess environments. Existing subprocess environment allowlists
remain authoritative.

## Initialization and State Creation

Read-only commands such as `doctor`, `compatibility`, and `status` resolve the
effective configuration without creating files. When repository state is
absent, they report that fact without mutating the repository.

State-changing commands may initialize the repository database and append-only
event log at their existing safe boundary before recording the requested
operation. Initialization runs the current sequential database migrations and
does not invoke provider inference.

`colay init` remains the explicit administrative initialization command. It
creates repository state and writes a minimal repository override containing
the current `config_version` and commented examples instead of copying every
compiled default. This prevents generated configuration from pinning values
that the application can safely evolve. Initialization refuses to overwrite an
existing repository or legacy configuration.

## Configuration Components and Data Flow

Configuration discovery is separated from parsing and merging:

- A discovery component receives the repository path, command-line path, and
  an injectable environment/home view. It returns ordered named layers.
- A layer parser validates each source version and returns a document plus its
  typed partial values.
- A deterministic merger applies the layer rules and validates the final
  complete `RootConfig` once.
- Callers receive the effective typed configuration and source metadata for
  diagnostics. Persisted administrative edits continue to target one explicit
  source document rather than the synthetic merged document.

Environment and home discovery are injectable inputs so tests do not mutate
process-global environment variables and can run concurrently.

## Error Handling

An absent optional standard layer is ignored. A present layer with invalid
TOML, an unsupported schema version, unsafe paths, invalid thresholds, invalid
time zones, NUL bytes, or invalid redaction expressions fails closed. Errors
name the layer and its path while continuing to apply existing redaction rules.

An explicit `COLAY_CONFIG` or `--config` path that is absent fails immediately.
A future schema version is never silently interpreted as the current schema.
Migration applies to a concrete source document with the existing backup-first
and atomic-write behavior; synthetic merged configuration is never persisted
as a migration side effect.

## Windows Executable Resolution

All provider probing, provider execution, configured usage probes, and
rollback destination lookup use one executable resolver before constructing a
`Command`. The resolver accepts the executable value and an injectable PATH and
platform view and returns an absolute selected path plus diagnostic evidence.

Paths that are absolute or contain path components are explicit. They are
validated as the exact requested target and are not replaced by another PATH
candidate.

On Windows, a bare executable name is resolved by walking PATH directories in
order and considering only supported Windows command suffixes: `.exe`, `.com`,
`.cmd`, and `.bat`. The effective PATHEXT order is honored after filtering it
to that allowlist. Extensionless shell scripts and ELF binaries are not
selected. The selected executable and arguments remain separate values passed
through Rust `Command`; Colay does not construct interpolated shell command
strings.

On Unix, the resolver preserves normal PATH ordering and requires an executable
regular-file candidate. Missing, invalid, and access-denied targets produce
distinct diagnostics. Resolver output is included in `doctor` data so an
administrator can see which path was selected.

## Codex 0.144.6 Compatibility

Codex CLI 0.144.6 becomes the recommended exact contract. The tested N/N-1
set becomes 0.144.6 and 0.144.5. Unknown versions continue to use the generic
untested adapter and keep writable execution disabled.

The 0.144.6 fixture set records only non-inference public interfaces:
`--version`, root help, `exec --help`, `exec resume --help`, App Server help,
App Server generated schema, and representative committed protocol fixtures.
The version catalog, pinned upstream revision, generated JSON matrix, release
documentation, and hard-coded compatibility registry remain synchronized.

Fixture collection may use a locally installed Codex binary for those
non-inference commands. Tests and CI consume committed fixtures and
`orchestrator-test-support` fake binaries; they never invoke real Codex,
Claude, or Gemini inference.

## Test Strategy

Configuration unit tests cover an empty layer set, every precedence boundary,
recursive table merging, complete array replacement, explicit missing paths,
invalid lower layers, future schemas, legacy conflicts, existing full configs,
and preservation of source comments and unknown fields.

Executable resolver tests use injected PATH values and test-controlled fake
files. Windows-specific cases place an extensionless Unix script or ELF file
before a valid fake `.cmd` or `.exe`, verify explicit paths are not substituted,
and verify provider and rollback call sites select the same result. Tests do
not call a real provider CLI.

CLI integration tests use temporary repositories and temporary `COLAY_HOME`
directories. They verify that read-only commands make no files, state-changing
commands initialize state safely, `init` writes only a minimal override, and
diagnostics report configuration sources and the resolved fake executable.

Compatibility contract tests verify that 0.144.6 selects the exact supported
adapter, 0.144.5 remains supported, unknown versions remain untested, committed
fixtures satisfy parsers, and the generated matrix matches the catalog and
manifests.

Final verification is:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Non-Goals

This change does not add credential discovery, identity rotation, quota
scraping, raw cross-provider quota comparison, external telemetry, automatic
push or merge, worktree deletion, or provider-specific types to
`orchestrator-domain`.
