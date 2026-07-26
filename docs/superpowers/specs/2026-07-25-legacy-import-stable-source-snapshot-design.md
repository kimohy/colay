# Legacy Import Stable Source Snapshot Design

## Scope

This review round removes the last mutable-path dependency from legacy SQLite import. The importer will never pass a repository source path, canonical source path, or `/proc/self/fd/<parent>/<name>` path to rusqlite after source guards are constructed. It will first capture an identity- and content-stable SQLite file family through retained handles into the existing private, locked, recoverable importer scratch. SQLite validation, online backup, migration, and rewrite then operate only on that scratch.

The change preserves source bytes, schemas, manifest and fingerprint semantics, append-only audit behavior, approval gates, publication ordering, and the current scratch scavenger. It adds no unsafe code, anonymous temporary storage, source-side lock, or source mutation.

## Guarded SQLite family

A focused `sqlite_snapshot` module owns SQLite-family capture. `GuardedSqliteFamily::open(root, database)` first creates a `SourceOpenGuard` for the main database, retaining its final file handle and every root-to-parent boundary. It then discovers only the exact sibling names formed by appending `-wal`, `-shm`, and `-journal` to the database filename. Every present sidecar receives its own `SourceOpenGuard`; a symbolic link, reparse point, non-regular file, containment escape, identity change, or open race is rejected. The family records the exact present sidecar suffix set and rejects any later appearance or disappearance.

`SourceOpenGuard` exposes its canonical source location for deterministic scratch namespacing and a crate-private retained-handle byte read used only by the family capture. Its existing `read_all` path continues to revalidate before and after retained-handle reads for JSONL and artifact evidence. The Linux and non-Linux `sqlite_path` API and the connection-owning `GuardedSourceConnection` are removed.

## Stable capture protocol

`GuardedSqliteFamily::capture(destination_main)` applies this sequence:

1. Revalidate the main database, all guarded sidecars, and the exact sidecar existence set.
2. Invoke the test-only `BeforeSnapshotCopy` hook after every guard is constructed and the pre-copy validation succeeds.
3. Read the entire main database and each recorded sidecar from their retained final handles into capture pass A. No pathname is reopened.
4. Invoke the test-only `AfterSnapshotCopy` hook. The hook is attempted even if pass A fails, so a deterministic substitution fixture can restore the source.
5. Revalidate every guarded identity and the exact sidecar existence set.
6. Read the entire retained family again as capture pass B, then revalidate identities and the sidecar set again.
7. Require the two ordered suffix/byte collections to be byte-for-byte identical. Any observed identity, set, or content difference fails closed.
8. Create new private files in locked recoverable scratch, using the exact sibling names `source.db`, `source.db-wal`, `source.db-shm`, and `source.db-journal` for the accepted members. Flush each file before returning.

The two full-family passes establish a quiescent interval across the whole captured family rather than validating each file in isolation. A final-name substitution cannot influence captured bytes: reads use the retained original handle. Production Windows handles continue to omit delete sharing, while Unix may allow a rename but still reads the retained object. If any path identity or set differs after the hook window, capture is rejected.

## Scratch-only SQLite lifecycle

Initial inspection derives a deterministic 64-hex scratch namespace from the guarded canonical source root and database location because the semantic source identity and final source fingerprint are not available until SQLite opens the capture. The namespace is only a lock/scavenger key; it does not replace or alter the sealed plan fingerprint. `LegacyImportScratch` remains rooted beside the selected global database, exclusively locked per namespace, and recoverable through its existing validated attempt scavenger.

Inspection captures the family as `source.db{,-wal,-shm,-journal}` inside that attempt and opens only `source.db` read-only. Schema status, integrity and foreign-key checks, live-daemon rejection, and source identity validation run on the private capture. SQLite online backup then produces a sidecar-independent `validated.db`; its bytes and length retain the existing `LegacyImportPlan.database_sha256` and `database_length` meaning. Migration creates `migrated.db` from `validated.db`, after which event, document, JSONL, artifact, manifest, logical-content, and fingerprint validation proceed as today.

Apply performs the sealed-plan reinspection before acquiring the plan-fingerprint scratch lock, avoiding recursive acquisition of the same non-reentrant lock. It then captures a fresh private `source.db` family under the plan fingerprint, opens and validates only that scratch image, and uses SQLite online backup to create staged immutable `legacy.db`. The staged database must match the inspected hash and length before any artifact staging or target transaction. Migration and rewrite continue from that staged stable backup.

## Failure and cleanup behavior

- Source SQLite main and sidecar files are opened only through retained guards and are never modified.
- A missing, added, replaced, linked, escaped, non-regular, or byte-changing family member rejects the attempt before staging publication or target transaction work.
- A partially written scratch family is confined to the owned attempt and removed on normal drop; abrupt termination leaves only validated recoverable scratch for the existing scavenger.
- No rejected capture changes workspace rows, import ledger rows, audit events, published files, or the sealed source evidence.
- Existing JSONL/artifact retained-handle validation and staging rollback behavior remain unchanged.

## Deterministic regressions

The first TDD regression installs a hook exactly between complete family-guard construction and retained snapshot copying. `BeforeSnapshotCopy` renames source database A aside and substitutes database B at the final name; pass A is allowed to proceed; `AfterSnapshotCopy` restores A. The assertion proves inspection/apply either consumes retained A or fails closed, never consumes B. On rejection it verifies original A and alternate B bytes, target rows, import ledger, audit rows, and published files are unchanged.

The second regression begins with no WAL sidecar and creates `database-wal` at `BeforeSnapshotCopy`. Post-copy exact-set validation must reject the new sidecar before SQLite opens scratch or target state changes. The fixture removes its injected sidecar after the assertion.

Focused source-guard, legacy-import, migration-contract, and orchestrator-state tests run before the repository gates. Final verification is `cargo fmt --all -- --check`, `cargo check --workspace --all-targets --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, and `git diff --check`.
