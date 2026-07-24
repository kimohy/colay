# Legacy Import Final Hardening Design

## Scope

This final review round closes the remaining source time-of-check/time-of-use gap, makes migration and rewrite scratch recoverable after abrupt termination, and aligns the previous review documents with the tests they actually perform. It does not change schemas, source evidence, provider behavior, or publication semantics. `LegacyImporter::inspect` gains an explicit `GlobalStatePaths` argument because private recoverable scratch must be rooted in the caller-selected global state rather than an ambient or process-wide directory.

## Stable source-open guard

A new internal `SourceOpenGuard` centralizes contained source opens. The guard validates a path below the supplied repository-state root, rejects symbolic-link and Windows reparse-point components, opens and retains every root-to-file parent plus the final file, and records a stable identity for each object.

The workspace remains on stable Rust 1.95. Windows `std::os::windows::fs::MetadataExt::volume_serial_number` and `file_index` are still nightly-only on that toolchain, so `orchestrator-state` uses the safe `same-file` API as a narrow dependency. `same-file::Handle::from_file` owns the already-open file and compares Unix device/inode identity or Windows volume/file-index identity without adding unsafe code to this repository. Windows opens use stable `OpenOptionsExt` controls: parent directories use `FILE_FLAG_BACKUP_SEMANTICS`, and parent/file handles permit read/write sharing but omit delete sharing so rename, junction replacement, and deletion remain blocked while the guard lives.

For every component and the final file, the guard compares three observations: the pre-open path handle, the retained opened handle, and a post-open path handle. A mismatch fails closed. Revalidation repeats the path-to-retained-handle comparison at each trust boundary. JSONL and artifact bytes are read from the retained final handle; staging receives already-verified bytes rather than reopening source paths.

The SQLite wrapper owns both the read-only `rusqlite::Connection` and its `SourceOpenGuard`. It retains them through schema-status checks, integrity and foreign-key validation, daemon-lease rejection, source identity calculation, online backup, source-document validation, and the final boundary revalidation. On Linux, SQLite opens `/proc/self/fd/<immediate-parent-fd>/<database-name>`, which roots the pathname in the retained parent while preserving normal `-wal` and `-shm` sidecar resolution. Other platforms use the contained pathname while the guard's platform protections and boundary identity checks remain active.

A test-only, thread-local two-phase hook fires after the pre-open identity observation and after the retained handle opens but before the post-open comparison. The regression swaps a source parent to an alternate directory for the open, restores the original parent, and proves the ABA cycle is rejected. The high-level import assertion also proves the source evidence and target database are unchanged.

## Recoverable importer scratch

Anonymous temporary directories no longer own inspection or apply migration databases. A new internal `LegacyImportScratch` derives its exact root from the global database parent:

```text
<global-database-parent>/import-scratch/
  <source-fingerprint>/
    import.lock
    attempt-<uuid>/
      owner.lock
      source.db
      migrated.db
      rewrite.db
      ... SQLite -wal/-shm sidecars
```

Apply uses the existing 64-character lowercase hexadecimal source fingerprint. Initial inspection has to calculate that content fingerprint from a migrated guarded backup, so its scratch namespace uses a deterministic 64-hex source-identity fingerprint derived from the guarded database's schema version and sealed `legacy_source_identity`; apply reinspection and transformation use the plan's final source fingerprint. Each acquisition validates exact containment, private permissions, child naming, and absence of link/reparse components.

`import.lock` is held exclusively for the entire attempt. Each attempt also owns an exclusive `owner.lock`. After the fingerprint lock is acquired, the importer enumerates only validated `attempt-<uuid>` children. It attempts each owner lock and removes that exact contained attempt directory, including SQLite `-wal` and `-shm` files, only when the owner lock is obtainable. A locked active attempt is preserved. Normal drop removes only the current attempt; abrupt termination releases OS locks and leaves residue that the next acquisition safely scavenges and rebuilds. No cleanup traverses above the exact importer scratch root or follows links.

Inspection and apply keep their scratch owner alive until all connections and backups that refer to it have closed. Scratch acquisition therefore also serializes concurrent inspection/apply work for one source fingerprint while leaving other sources independent.

## Documentation and fixture alignment

The approved-graph collision fixture creates a sealed requirement revision and supplies a real `GraphValidationAuthority` in both the graph revision and approval. The regression validates the imported authority and its references after collision rewriting.

The previous implementation plan incorrectly documented `prepare_rewrite_scratch` as returning a connection; the actual and retained signature is `StateResult<()>`. Earlier evidence wording also implied a byte-for-byte published SQLite database hash comparison that the fixture did not perform. The corrected claim is narrower: the regressions hash source evidence files before and after import, and independently open published `legacy.db` to verify the original sealed proposal/preview identities and hashes. Manifest sealing remains covered by the existing import and replay tests.

## Failure and recovery invariants

- A source identity or containment mismatch returns before source bytes are trusted or target state is mutated.
- Source database, JSONL, artifacts, and published `legacy.db` remain read-only evidence.
- Scratch cleanup requires both exact-root containment and an obtainable owner lock.
- An active attempt is never deleted; a crashed attempt is recoverable on the next acquisition.
- Target transaction, staging publication, audit ledger, and explicit approval gates retain their existing ordering and rollback behavior.

## Verification

TDD first records RED results for the ABA swap and scratch residue/active-lock cases. Focused source-import, migration-contract, and state tests must pass, followed by `cargo fmt --all -- --check`, `cargo check --workspace --all-targets --all-features`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, `cargo test --workspace --all-features`, and `git diff --check`.
