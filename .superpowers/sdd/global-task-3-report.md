# Global Task 3 Report: Artifact Layout and Legacy Import

## Outcome

Implemented a read-only, idempotent repository-state importer that publishes legacy state under the global workspace artifact layout. The importer validates the complete source before mutation, preserves the original database as evidence, imports supported rows transactionally, records deterministic ID rewrites, and fails closed when either source or already-published import evidence is inconsistent.

An independent read-only review completed with the verdict: **READY: no release-blocking correctness or audit issues found.**

## Implementation

- Added migration 14 with the `legacy_imports` ledger and durable `legacy_import_id_mappings`.
- Added `LegacyImporter::inspect` and `LegacyImporter::apply`.
- Opened legacy sources read-only and rejected failed SQLite integrity, foreign-key, migration-checksum, event-chain, JSONL, artifact-manifest, or live-daemon checks.
- Used SQLite online backup for a stable private snapshot and never migrated or modified the source database.
- Derived a stable logical source fingerprint that survives relocation and `VACUUM` while still including referenced artifact content.
- Staged files under the target workspace, verified their physical manifest, renamed them atomically, and committed imported rows and ledger evidence together.
- Added a pre-import target backup and scoped stale-staging recovery.
- Imported all supported workspace tables in foreign-key order after exact schema validation.
- Preserved vendor-neutral domain state and rewrote only typed identity fields when deterministic UUID collisions occur; arbitrary UUID-looking user text is not rewritten.
- Revalidated original sealed records before rewriting and propagated dependent hashes for checkpoints, handovers, verifications, graphs, integrations, task envelopes, worker results, and supported client commands.
- Preserved an exact read-only `legacy.db` evidence copy and namespaced imported artifact paths under `imports/<fingerprint>/...`.
- Added an auditable compatibility anchor for merged chains or ID mappings, with source root/tip/count, mapping hash, and copied-chain semantics.
- Hardened no-op replay by revalidating the published manifest, mapping ledger, imported event prefix, full current target chain, and anchor uniqueness/content.
- Updated artifact lookup to use the exact workspace-owned path instead of heuristic scanning.
- Updated CLI startup migration expectations to use `STATE_SCHEMA_VERSION`.

## TDD Evidence

- Initial focused test was RED because `LegacyImporter` did not exist.
- Added RED regressions for artifact mismatches, live daemons, collisions, typed payload rewrites, missing JSONL exports, stale staging, target backups, namespace lookup, relocation/`VACUUM` fingerprint stability, unaudited targets, published-manifest tampering, invalid source seals, mapping-ledger tampering, and imported-chain corruption.
- Implemented each behavior until the focused suite was GREEN: 14 passed, 0 failed.
- Migration contract suite was GREEN: 5 passed, 0 failed.

## Verification

All required gates passed on the final code:

```text
cargo fmt --all -- --check
PASS

cargo clippy --workspace --all-targets --all-features -- -D warnings
PASS

cargo test --workspace --all-features
PASS (exit 0; 498 seconds)
```

One earlier full-suite attempt hit a pre-existing Windows test-environment flake while launching `icacls.exe` (`PermissionDenied`) in `records::tests::checkpoint_diff_is_registered_and_missing_file_is_rejected`. The isolated test passed immediately afterward, and the exact full workspace command above subsequently passed without the flake.

Additional checks:

```text
cargo test -p orchestrator-state --test legacy_import -- --nocapture
PASS (14 passed, 0 failed)

cargo test -p orchestrator-state --test migration_contract -- --nocapture
PASS (5 passed, 0 failed)

git diff --check
PASS
```

## Files

- `.superpowers/sdd/global-task-3-report.md`
- `crates/orchestrator-cli/tests/default_startup.rs`
- `crates/orchestrator-state/src/artifacts.rs`
- `crates/orchestrator-state/src/legacy_import.rs`
- `crates/orchestrator-state/src/lib.rs`
- `crates/orchestrator-state/src/migrations.rs`
- `crates/orchestrator-state/tests/legacy_import.rs`
- `crates/orchestrator-state/tests/migration_contract.rs`
- `migrations/0014_legacy_imports.sql`
