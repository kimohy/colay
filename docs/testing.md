# Testing

## Fake-only rule

Tests and CI must never invoke real Codex, Claude, Gemini, or Agy inference. Provider integration tests use the compiled `fake-provider-cli` or `FakeAdapterRuntime`. The fake runtime canonicalizes its configured executable and rejects any basename other than `fake-provider-cli`, so accidentally passing `codex`, `claude`, `gemini`, or `agy` is a test failure.

CI clears common provider API-key variables and sets `COLAY_TEST_FAKE_PROVIDERS_ONLY=1` at job scope. Compatibility workflows may build an exact official Codex source revision and run only the explicit version/help/schema probe allowlist; they never pass a prompt.

Configuration, resolver, and rollback tests also use local fixtures and fake binaries only. They do not invoke provider inference: Windows and Unix executable-resolution cases exercise fixture files, and rollback cases validate persisted execution evidence without resolving a live provider binary from the current `PATH`.

Daemon tests are also inference-free. State tests race independent SQLite
connections for command claims and lease acquisition, while runtime tests use
short Tokio intervals to cover heartbeat, cancellation, stop, and release. The
CLI lifecycle test launches only the compiled `colay daemon serve` child in a
temporary repository, verifies start/status/restart/stop, and checks that no
child remains. Crash recovery is exercised through expired leases and stale
claimed commands; no provider binary or network listener is involved.

Chat TUI tests use Ratatui's in-memory backend and scripted key events. They
cover exact wide/medium/narrow/compact thresholds, pane traversal, command and
target pickers, the no-silent-retarget invariant, administration round trips,
terminal restoration, and a bounded tail from 1,000 messages. CLI reconnect
tests launch only `colay daemon serve` in a temporary repository, require
session/message completion within 500ms, reopen SQLite, verify double redaction,
confirm the daemon survives the client, and stop the child during cleanup.

Phase 3 graph tests cover deterministic DAG validation, cycles, dependency and
scope errors, provider/profile eligibility, stable proposal hashes, immutable
valid/invalid revisions, exact-hash idempotent approval, session-isolated graph
projection, and stale approval overlays. `chat_plan_approval.rs` launches the
real local daemon plus only the compiled fake official CLI. It proves a single
read-only planner invocation, heartbeat-backed command completion, zero
tasks/worktrees/worker leases before approval, wrong-hash rejection, exact
queued-task/dependency materialization, SQLite reconnect, and child cleanup.

Phase 4 scheduling tests race two independent SQLite connections, enforce exact
global/provider capacity, dependency verification, component-aware scope
ownership, repository-wide exclusion, idempotent release, and ordered
instruction recovery. The daemon test runs two slow disjoint executor futures
concurrently while continuing to renew and release claims. The
`parallel_task_execution.rs` integration test creates a real temporary Git
repository and launches only real fake official-CLI subprocesses. It proves
overlapping invocation intervals, distinct retained worktrees, target-exact
instruction application, sealed completion evidence, and restart without a
duplicate attempt or mutation of the user's branch.

Phase 5 domain/engine/state tests cover stable preview seals, exact approval,
overlap and source mutation, immutable persistence, idempotent resolution-task
creation, and interrupted-application reconciliation. The daemon
`result_integration.rs` test uses a real temporary Git repository: typed preview
remains read-only, exact approval applies two verified sources only to the
dedicated integration worktree, session state completes, and the user plus task
worktrees remain unchanged.

## Required local verification

```text
npm test
node --test scripts/release/test/workflow-contract.test.mjs
python scripts/generate_codex_matrix.py --check
git diff --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

`npm test` uses only the dependency-free Node.js built-in test runner. It
checks the npm package templates, launcher behavior, release version/channel
classification, staging allowlists and checksums, retry-safe publication logic,
and workflow contracts without contacting npm or GitHub.

## Release package smoke tests

The release workflow packs all four staged tarballs locally and, on each native
runner, installs the root tarball and its selected platform tarball into an
isolated npm prefix with `--offline --ignore-scripts`. It then runs only
`colay --version` from that isolated installation. On Windows, the smoke
invokes npm's generated `colay.ps1` global command shim through the known
Windows PowerShell executable with separated arguments and `shell: false`.
This proves package versions, exact optional dependencies, and the embedded
Rust version agree without a registry publish or provider process. Linux x64
uses a musl-linked binary and
has no npm `libc` selector, so the package remains installable on both musl and
glibc hosts.

No provider credentials are needed. If an integration test asks for a provider login or consumes Enterprise quota, stop: that test violates the repository contract. For lower disk pressure, the Rust verification suite may be run with `CARGO_INCREMENTAL=0`.

## Contract coverage

- `codex-compat/tests/contracts.rs` validates exact N/N-1 help/schema/event fixtures, unknown optional preservation, fail-closed lifecycle events, quota classification, resume events, and the committed compatibility matrix.
- `orchestrator-test-support/tests/provider_e2e.rs` runs each adapter through fake structured streams or Agy's bounded plain-text bridge, malformed/error/quota paths, cancellation, redaction, bounded process execution, and executable/argv usage probes.
- `orchestrator-test-support/tests/multi_provider_handover_e2e.rs` drives the vendor-neutral lifecycle from a fake Gemini daily quota event through a sealed checkpoint, Codex implementation, a monthly-headroom warning carried into the Claude handover, Claude read-only review, and the independent completion gate.
- `orchestrator-cli/tests/fake_cli_handover_e2e.rs` launches the compiled `colay` and gated `colay-e2e-fake-provider` binaries. It proves a Codex quota failure preserves a partial Git diff, Claude exactly acknowledges the sealed bundle before writing, local fmt/clippy/check/test evidence reaches `Completed`, the original branch remains untouched, and no merge/push/cleanup occurs. A second scenario exercises sealed, explicitly approved SQLite restore, recovery backup retention, and the post-restore JSONL hash chain.
- `orchestrator-state/tests/migration_contract.rs` starts at SQLite schema v1, verifies the sequential v2/v3/v4/v5/v6/v7/v8 plan and historical event hashes, rebuilds constrained command tables without losing rows, proves dry-run non-mutation, inspects backups, and rejects checksum/future-schema tampering. `orchestrator-state/tests/config_migration.rs` separately verifies config v1 -> v2 -> v3 -> v4, legacy state-path materialization, explicit-path preservation, and the `.colay` v4 default.
- `orchestrator-cli/tests/daemon_lifecycle.rs` proves public help, hidden internal serve, absent-state status, single-instance start, idempotent start, restart ownership transfer, graceful stop, and child cleanup.
- `orchestrator-cli/tests/chat_tui_reconnect.rs` proves chat help/docs, durable daemon command processing, redacted persistence, a second SQLite connection restoring the session, and daemon survival/cleanup.
- `orchestrator-cli/tests/chat_plan_approval.rs` proves the full goal -> read-only fake planner -> validated revision -> exact typed approval path through a real daemon process, with no pre-approval writable artifact and no real provider.
- `orchestrator-cli/tests/parallel_task_execution.rs` proves approved disjoint tasks overlap through fake official CLI processes in isolated Git worktrees, task instructions stay target-exact, claims are released, restart is idempotent, and no integration/merge/push/cleanup occurs.
- `orchestrator-daemon/tests/result_integration.rs` proves typed exact-hash integration through the daemon with real Git worktrees and no mutation of the user or source task worktrees.

The multi-provider test deliberately uses synthetic Git evidence, the persistence secret preflight, and an in-memory fake runtime; it validates orchestration contracts without executing a real model or mutating a user repository. Engine worktree tests separately exercise actual temporary Git repositories.

## Adding a Codex release

1. Run the non-inference compatibility workflow for the exact release tag.
2. Review command/option/schema changes in the uploaded report.
3. Add a version directory with manifest, public metadata, and reviewed redacted JSONL contract fixtures.
4. Add the exact version to `codex-version.toml`, run `python scripts/generate_codex_matrix.py`, and commit the regenerated `codex-matrix.json` in the same change.
5. Run all required verification commands.
6. Merge only after human review; CI never auto-enables or auto-merges a new writable adapter.
