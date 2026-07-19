# Testing

## Fake-only rule

Tests and CI must never invoke real Codex, Claude, or Gemini inference. Provider integration tests use the compiled `fake-provider-cli` or `FakeAdapterRuntime`. The fake runtime canonicalizes its configured executable and rejects any basename other than `fake-provider-cli`, so accidentally passing `codex`, `claude`, or `gemini` is a test failure.

CI clears common provider API-key variables and sets `COLAY_TEST_FAKE_PROVIDERS_ONLY=1` at job scope. Compatibility workflows may build an exact official Codex source revision and run only the explicit version/help/schema probe allowlist; they never pass a prompt.

## Required local verification

```text
npm test
node --test scripts/release/test/workflow-contract.test.mjs
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

No provider credentials are needed. If an integration test asks for a provider login or consumes Enterprise quota, stop: that test violates the repository contract.

## Contract coverage

- `codex-compat/tests/contracts.rs` validates exact N/N-1 help/schema/event fixtures, unknown optional preservation, fail-closed lifecycle events, quota classification, resume events, and the committed compatibility matrix.
- `orchestrator-test-support/tests/provider_e2e.rs` runs each adapter through fake structured streams, malformed/error/quota paths, cancellation, redaction, bounded process execution, and executable/argv usage probes.
- `orchestrator-test-support/tests/multi_provider_handover_e2e.rs` drives the vendor-neutral lifecycle from a fake Gemini daily quota event through a sealed checkpoint, Codex implementation, a monthly-headroom warning carried into the Claude handover, Claude read-only review, and the independent completion gate.
- `orchestrator-cli/tests/fake_cli_handover_e2e.rs` launches the compiled `colay` and gated `colay-e2e-fake-provider` binaries. It proves a Codex quota failure preserves a partial Git diff, Claude exactly acknowledges the sealed bundle before writing, local fmt/clippy/check/test evidence reaches `Completed`, the original branch remains untouched, and no merge/push/cleanup occurs. A second scenario exercises sealed, explicitly approved SQLite restore, recovery backup retention, and the post-restore JSONL hash chain.
- `orchestrator-state/tests/migration_contract.rs` starts at SQLite schema v1, verifies the sequential v2/v3 plan, proves dry-run non-mutation, inspects the pre-apply backup, and rejects checksum/future-schema tampering. `orchestrator-state/tests/config_migration.rs` separately verifies config v1 -> v2 -> v3 -> v4, legacy state-path materialization, explicit-path preservation, and the `.colay` v4 default.

The multi-provider test deliberately uses synthetic Git evidence, the persistence secret preflight, and an in-memory fake runtime; it validates orchestration contracts without executing a real model or mutating a user repository. Engine worktree tests separately exercise actual temporary Git repositories.

## Adding a Codex release

1. Run the non-inference compatibility workflow for the exact release tag.
2. Review command/option/schema changes in the uploaded report.
3. Add a version directory with manifest, public metadata, and reviewed redacted JSONL contract fixtures.
4. Add the exact version to `codex-version.toml`, run `python scripts/generate_codex_matrix.py`, and commit the regenerated `codex-matrix.json` in the same change.
5. Run all required verification commands.
6. Merge only after human review; CI never auto-enables or auto-merges a new writable adapter.
