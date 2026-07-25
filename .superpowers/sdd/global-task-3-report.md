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

## Reviewer-Gate Follow-up

The reviewer gate identified four additional audit-boundary findings. The follow-up is limited to those findings.

### Immutable transform scratch

- The inspected source, staged `legacy.db`, and fully validated migrated image are never rewritten.
- Apply copies the validated migrated image to a private temporary `rewrite.db`.
- Before transformation, the importer compares normalized SQLite definitions for exactly six migration-13 immutable UPDATE triggers: graph payload, graph authority, graph approval, integration batch, integration source, and integration approval.
- Only those six exact triggers are removed from `rewrite.db`. A missing or changed definition fails closed, and every other trigger remains active.
- Collision remapping, typed JSON rewrites, resealing, and graph/integration dependent-hash propagation operate only against the attached scratch image.

TDD evidence:

```text
cargo test -p orchestrator-state --test legacy_import approved_graph_collision -- --nocapture
RED: SQLite `graph revision payload is immutable`

cargo test -p orchestrator-state --test legacy_import integration_collision -- --nocapture
RED: SQLite `integration preview payload is immutable`

cargo test -p orchestrator-state --test legacy_import collision -- --nocapture
GREEN: 2 passed, 0 failed
```

The GREEN fixtures verify an approved non-planning graph collision and a colliding integration batch/source/approval/application. They recompute the imported proposal/preview seals, verify propagated approval/application hashes, hash source evidence files before and after apply, and query published `legacy.db` to verify the original proposal/preview IDs and sealed hashes. Published-manifest sealing is covered separately by the import and replay regressions.

### Transaction-local target audit gate

- The import transaction validates the complete target workspace chain before creating a backup or mutating rows, ledger data, anchors, or published files.
- Validation covers sequence continuity, row/JSON agreement, predecessor hashes, canonical event hashes, and an optional `event_log_state` cursor that must identify an exact chain prefix.
- The validated event count is passed into import logic; untrusted rows are not recounted after the gate.

TDD evidence:

```text
cargo test -p orchestrator-state --test legacy_import before_any_target_mutation -- --nocapture
RED: gapped target chain and inconsistent cursor were both accepted
GREEN: 2 passed, 0 failed
```

Both GREEN fixtures compare task/event/ledger/mapping/backup counts before and after refusal and verify neither staging nor final fingerprint directories remain.

### Requirement revision validation

- Every source requirement row now parses exact revision, session, and source-message domain IDs.
- Schema version and positive ordinal are required.
- `snapshot_json` must decode as the deny-unknown-fields `RequirementSnapshot` contract.
- The stored completeness bit must equal `RequirementSnapshot::is_complete`.
- The stored hash must equal the provider-neutral canonical SHA-256 of the typed snapshot.

TDD evidence:

```text
cargo test -p orchestrator-state --test legacy_import requirement_ -- --nocapture
RED: canonical hash mismatch and row/snapshot completeness mismatch were both accepted
GREEN: 2 passed, 0 failed
```

### Source-open containment

- Database, JSONL, manifest, directory traversal, and final staging-copy reads now rerun component-wise symbolic-link/reparse-point checks immediately before open.
- Canonical source paths must remain below the canonical source root.
- Paths are canonicalized again after open and after file reads where feasible, while hashing bytes from the already-opened handle.

TDD evidence on Windows:

```text
cargo test -p orchestrator-state --test legacy_import nested_link -- --nocapture
RED: nested directory junction to matching external artifact bytes was accepted
GREEN: 1 passed, 0 failed
```

### Final follow-up verification

All requested final commands passed on the follow-up code:

```text
cargo test -p orchestrator-state --test legacy_import -- --nocapture
PASS: 21 passed, 0 failed

cargo test -p orchestrator-state --test migration_contract -- --nocapture
PASS: 5 passed, 0 failed

cargo test -p orchestrator-state --all-features
PASS

cargo fmt --all -- --check
PASS

cargo check --workspace --all-targets --all-features
PASS

cargo clippy --workspace --all-targets --all-features -- -D warnings
PASS

cargo test --workspace --all-features
PASS: exit 0; 504.8 seconds
```

No environmental flake occurred in the final follow-up state or workspace suites.

## Final Source-Lifecycle Hardening

The final review round closed the remaining source ABA and crash-recovery findings without changing schemas, evidence publication, provider behavior, or audit ordering.

### Stable source-open identity

- Added a narrowly scoped `same-file` dependency to `orchestrator-state`. Stable Rust 1.95 does not expose Windows `MetadataExt::volume_serial_number` or `file_index`; `same-file::Handle::from_file` provides the required safe volume/file-index comparison without adding unsafe code to this repository.
- Added `SourceOpenGuard`, which compares pre-open, retained-handle, and post-open identities for the source root, every parent boundary, and the final file.
- Windows parent and file handles omit delete sharing; directory handles use backup semantics. Linux SQLite opens through `/proc/self/fd/<parent-fd>/<database-name>` so the retained parent roots the database and its sidecar naming.
- The guarded read-only SQLite connection is retained and revalidated through schema status, integrity, daemon ownership, source identity, online backup, document validation, and manifest construction.
- JSONL and artifacts are read from their retained guarded file handles. Artifact staging now accepts those verified bytes and does not reopen the source path.

TDD evidence:

```text
cargo test -p orchestrator-state source_parent_aba -- --nocapture
RED: the new regression initially failed to compile because SourceOpenGuard and its deterministic two-phase hook did not exist.
GREEN: 2 passed, 0 failed
```

The direct regression replaces a parent after the pre-open identity observation, opens the alternate, restores the original before the post-open check, and verifies refusal. The high-level apply regression performs the same ABA cycle and verifies both the original source database hash and target import-ledger count remain unchanged.

### Recoverable locked scratch

- Replaced anonymous legacy-import inspection/migration temporary storage with private scratch below the selected global database parent: `import-scratch/<fingerprint>/attempt-<uuid>`.
- `LegacyImporter::inspect` now receives the caller-selected `GlobalStatePaths`; it never guesses or writes an ambient global directory.
- A per-fingerprint `import.lock` and per-attempt `owner.lock` are held for the attempt. Scavenging considers only valid, exactly contained attempt directories and removes one only after obtaining its owner lock.
- Normal drop removes the current attempt. Abrupt termination releases locks and leaves recoverable database, `-wal`, and `-shm` files for the next attempt.
- A locked active attempt and a sentinel outside the exact scratch root are preserved.

TDD evidence:

```text
cargo test -p orchestrator-state --test legacy_import scratch_crash -- --nocapture
RED: stale migrated.db, migrated.db-wal, and migrated.db-shm residue remained after retry.
GREEN: 1 passed, 0 failed
```

### Fixture and documentation alignment

- The approved-graph collision fixture now seeds a sealed requirement revision and full `GraphValidationAuthority` in both the graph revision and approval. The imported validation JSON and approval columns are checked against that authority.
- Corrected the documented `prepare_rewrite_scratch` return type to `StateResult<()>`.
- Narrowed evidence wording to the actual checks: unchanged source evidence hashes, original sealed proposal/preview identities and hashes queried from published `legacy.db`, and separate existing manifest-seal/replay coverage.

### Final verification

```text
cargo test -p orchestrator-state --test legacy_import -- --nocapture
PASS: 22 passed, 0 failed

cargo test -p orchestrator-state --test migration_contract -- --nocapture
PASS: 5 passed, 0 failed

cargo test -p orchestrator-state --all-features
PASS: all unit, integration, and doc tests

cargo fmt --all -- --check
PASS

cargo check --workspace --all-targets --all-features
PASS

cargo clippy --workspace --all-targets --all-features -- -D warnings
PASS

cargo test --workspace --all-features
PASS: exit 0; 544.4 seconds on the final-code rerun
```

The first full workspace-test attempt stopped during compilation, before tests ran, because the worktree drive had only 4.6 MB free (`os error 112`, followed by a PDB filesystem error). After verifying that the exact target was this worktree's `target` directory, `cargo clean` removed 14.4 GiB of recoverable build artifacts only. The unchanged full workspace command then rebuilt cleanly and passed in 651.6 seconds. After the high-level ABA assertion was strengthened to cover every relevant target row family and absence of an import directory, the final-code workspace rerun also passed in 544.4 seconds. This was not a test or code failure.

## Stable Retained SQLite-Family Snapshot

The final P1 review found that Linux SQLite still resolved the final database name through `/proc/self/fd/<parent>/<name>` after `SourceOpenGuard` construction. A final-component ABA could therefore bind rusqlite to replacement B even though the retained guard and later path revalidation identified original A.

### Retained family capture

- Added `GuardedSqliteFamily`, which retains guards for the main database and the exact present set of `-wal`, `-shm`, and `-journal` siblings.
- Sidecar discovery accepts only exact regular-file siblings below the guarded source root. Links, reparse points, escapes, unexpected file types, identity changes, and set changes fail closed.
- Capture validates the family, reads every member from retained handles, revalidates the complete identity/set boundary, reads every retained handle a second time, and requires ordered byte-for-byte equality before writing scratch.
- The private recoverable attempt receives exact `source.db`, `source.db-wal`, `source.db-shm`, and `source.db-journal` sibling names for present members. Unix creation requests mode `0600` and verifies it. Windows create-new files inherit the already-hardened attempt directory's protected trusted-only file ACEs; no redundant per-file ACL subprocess is needed.
- Removed `SourceOpenGuard::sqlite_path`, `GuardedSourceConnection`, and `open_source_read_only`. No production rusqlite open receives the mutable source database path or `/proc/self/fd` path.

Inspection now acquires deterministic location-keyed recoverable scratch before SQLite opens, captures the raw family there, validates only scratch `source.db`, and online-backs it up to sidecar-independent `validated.db`. Hashing, migration, event/document validation, and plan sealing use that stable backup. Apply reinspects before taking the non-reentrant plan scratch lock, captures a fresh family under that lock, validates only scratch, and online-backs it up to staged immutable `legacy.db` before hash/manifest verification and the existing migration/rewrite flow.

The two full-family retained reads detect observed identity, set, and byte changes during capture. They intentionally do not claim detection of an unobservable content ABA that returns to identical retained bytes between both complete passes; in that case the accepted evidence bytes are still the retained original content.

### TDD race evidence

```text
cargo test -p orchestrator-state sqlite_final_name_substitution -- --nocapture
RED: E0432, `crate::sqlite_snapshot` did not exist.
GREEN: 1 passed, 0 failed.

cargo test -p orchestrator-state sqlite_wal_sidecar_set_change -- --nocapture
RED: E0432, the snapshot capture boundary/hook did not exist.
GREEN: 1 passed, 0 failed.

cargo test -p orchestrator-state sqlite_ -- --nocapture
FINAL GREEN: 2 passed, 0 failed.
```

The final-name fixture installs its hook only after the complete family guard is constructed. It renames A aside, substitutes non-SQLite B, lets retained pass A proceed, and restores A before post-copy validation. The importer reaches A's expected `legacy database has no migration history` result rather than B's invalid-database result. Both hook phases ran; A and B hashes, target task/event/import/mapping counts, saved-name absence, and published-import absence were unchanged.

The WAL fixture starts with no sidecar, creates the exact `-wal` sibling at `BeforeSnapshotCopy`, and leaves it through post-copy validation. Capture rejects `legacy SQLite sidecar set changed` before scratch SQLite or target mutation; the main hash, target counts, ledger/mappings, and published files remain unchanged after fixture cleanup.

### Focused and final verification

```text
cargo test -p orchestrator-state source_parent_aba -- --nocapture
PASS: 2 passed, 0 failed

cargo test -p orchestrator-state --all-features --test legacy_import -- --nocapture
PASS: 22 passed, 0 failed

cargo test -p orchestrator-state --test migration_contract -- --nocapture
PASS: 5 passed, 0 failed

cargo test -p orchestrator-state --all-features
PASS: 87 unit tests and every state integration/doc-test suite

cargo fmt --all -- --check
PASS

cargo check --workspace --all-targets --all-features
PASS

cargo clippy --workspace --all-targets --all-features -- -D warnings
PASS

cargo test --workspace --all-features
PASS: exit 0; 454.8 seconds on the final unchanged rerun
```

An intermediate capture implementation invoked the full Windows `ensure_private_file` ACL rewrite for every new raw scratch member. Parallel import fixtures exposed `icacls.exe` spawn denials. The final implementation removes only those redundant child rewrites while retaining the already-verified private attempt-directory inheritance boundary; the affected parallel import binary then passed 22/22. A later full workspace attempt encountered the same transient OS denial in the untouched CLI resume fixture. Its exact all-features test immediately passed 1/1 in isolation, and the final exact unchanged workspace command passed completely. No unrelated production or fixture behavior was changed.

Planned implementation commit subject: `fix: snapshot guarded legacy SQLite families`.
