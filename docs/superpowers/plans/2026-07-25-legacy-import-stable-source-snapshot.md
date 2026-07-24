# Legacy Import Stable Source Snapshot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Do not delegate this plan; the parent assigned this final review round to the current worker.

**Goal:** Ensure legacy import can consume only a stable retained-handle snapshot of the source SQLite main database and its exact sidecar family.

**Architecture:** Add a focused `GuardedSqliteFamily` beside the generic `SourceOpenGuard`. It captures two byte-identical passes from retained main/sidecar handles into locked recoverable scratch, after which rusqlite opens only the private capture and online backup produces the preserved validation/migration image. Inspection uses a deterministic canonical-location scratch key; apply reinspects first and then captures under the sealed plan fingerprint.

**Tech Stack:** Rust 2024/stable 1.95, `std::fs`, safe `same-file` handles, `rusqlite` online backup, SHA-256, existing `fs2`-locked `LegacyImportScratch`.

## Global Constraints

- Add no unsafe code, anonymous temporary storage, source mutation, source-side locks, or mutable source path passed to rusqlite.
- Guard exactly the main database plus present `-wal`, `-shm`, and `-journal` siblings; reject links, reparse points, escapes, identity changes, set changes, and byte changes.
- Copy only from retained handles and preserve exact SQLite sibling names below the current owned scratch attempt.
- Preserve schemas, source bytes, manifest/fingerprint semantics, append-only audit semantics, explicit approval gates, staging rollback, and publication ordering.
- Do not invoke real provider inference in tests; this state-only plan needs no provider process.
- Run the repository-required format, clippy, and full workspace test gates before completion.

---

### Task 1: Record final-name and WAL-set regressions

**Files:**
- Modify: `crates/orchestrator-state/src/legacy_import.rs` (the existing `final_hardening_tests` module)

**Interfaces:**
- Consumes: existing `LegacyImporter::apply`, `set_source_open_hook`, `target_import_counts`, and source/published hash helpers.
- Anticipates: `set_snapshot_capture_hook`, `clear_snapshot_capture_hook`, and `SnapshotCaptureHookPhase::{BeforeSnapshotCopy,AfterSnapshotCopy}` from Task 2.

- [ ] **Step 1: Add a deterministic final-file substitution regression**

Replace neither existing ABA test. Add a second high-level test that creates SQLite A at `source.database`, garbage/non-SQLite B at a sibling, and a dummy sealed plan. Install a no-op `set_source_open_hook` so Windows test handles permit the deliberate rename, then use the new capture hook:

```rust
set_snapshot_capture_hook(move |phase, database| {
    if database != source_database_for_hook {
        return Ok(());
    }
    match phase {
        SnapshotCaptureHookPhase::BeforeSnapshotCopy => {
            fs::rename(&source_database_for_hook, &saved_for_hook)
                .map_err(|error| StateError::io(&source_database_for_hook, error))?;
            fs::rename(&alternate_for_hook, &source_database_for_hook)
                .map_err(|error| StateError::io(&source_database_for_hook, error))?;
        }
        SnapshotCaptureHookPhase::AfterSnapshotCopy => {
            fs::rename(&source_database_for_hook, &alternate_for_hook)
                .map_err(|error| StateError::io(&source_database_for_hook, error))?;
            fs::rename(&saved_for_hook, &source_database_for_hook)
                .map_err(|error| StateError::io(&source_database_for_hook, error))?;
        }
    }
    Ok(())
});
```

Call `LegacyImporter::apply`, clear both hooks even on the expected error, and assert the result reports A's valid empty-SQLite outcome (`legacy database has no migration history`) rather than B's `file is not a database`. Assert A and B hashes, `target_import_counts`, the `imports` directory, and source sibling layout are unchanged.

- [ ] **Step 2: Add a deterministic WAL appearance regression**

Create an empty SQLite main database with no `-wal`. At `BeforeSnapshotCopy`, write an injected `source.db-wal`; leave it present through `AfterSnapshotCopy`. Call `LegacyImporter::apply` and assert the error reports a changed SQLite sidecar set. Clear the hook, remove only the injected WAL, and assert the main hash, target counts, ledger/mapping counts, and absence of published imports remain unchanged.

- [ ] **Step 3: Run the new regressions RED**

Run:

```text
cargo test -p orchestrator-state sqlite_final_name_substitution -- --nocapture
cargo test -p orchestrator-state sqlite_wal_sidecar_set_change -- --nocapture
```

Expected: compilation fails because the snapshot-capture hook/family interfaces do not exist. Record that precise RED cause in `.superpowers/sdd/global-task-3-report.md` only after the implementation is complete.

### Task 2: Capture an exact SQLite family through retained handles

**Files:**
- Modify: `crates/orchestrator-state/src/source_guard.rs`
- Create: `crates/orchestrator-state/src/sqlite_snapshot.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`

**Interfaces:**
- Produces: `SourceOpenGuard::canonical_path(&self) -> &Path`.
- Produces: `SourceOpenGuard::read_retained(&self) -> StateResult<Vec<u8>>` for the family capturer; `read_all` remains pre/post revalidated.
- Removes: `SourceOpenGuard::sqlite_path` on every platform.
- Produces: `GuardedSqliteFamily::open(root: &Path, database: &Path) -> StateResult<Self>`.
- Produces: `GuardedSqliteFamily::{canonical_root,canonical_database,capture}` where `capture(&self, destination_main: &Path) -> StateResult<()>`.

- [ ] **Step 1: Separate retained reads from pathname revalidation**

Refactor `SourceOpenGuard::read_all` without changing its behavior:

```rust
pub(crate) fn read_all(&self) -> StateResult<Vec<u8>> {
    self.revalidate()?;
    let bytes = self.read_retained()?;
    self.revalidate()?;
    Ok(bytes)
}

pub(crate) fn read_retained(&self) -> StateResult<Vec<u8>> {
    let mut file = self.final_component().handle.as_file().try_clone()
        .map_err(|error| StateError::io(&self.requested_path, error))?;
    file.seek(SeekFrom::Start(0))
        .map_err(|error| StateError::io(&self.requested_path, error))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| StateError::io(&self.requested_path, error))?;
    Ok(bytes)
}
```

Add `canonical_path`; delete both `sqlite_path` implementations and their Linux raw-fd import.

- [ ] **Step 2: Implement exact sidecar discovery and guards**

Create `sqlite_snapshot.rs` with `SIDECAR_SUFFIXES: [&str; 3] = ["-wal", "-shm", "-journal"]`, a `GuardedMember { suffix: &'static str, guard: SourceOpenGuard }`, and `GuardedSqliteFamily`. Build sibling paths by appending to the main `OsStr`, not by string-formatting the whole path. `fs::symlink_metadata` returning `NotFound` means absent; any present link/reparse/non-file is an error; every present member is opened with `SourceOpenGuard::open`. Re-scan the three exact names after construction and in every `revalidate` call, comparing the ordered suffix set exactly.

- [ ] **Step 3: Implement two-pass retained capture and private writes**

Implement this capture skeleton:

```rust
pub(crate) fn capture(&self, destination_main: &Path) -> StateResult<()> {
    self.revalidate()?;
    run_snapshot_capture_hook(SnapshotCaptureHookPhase::BeforeSnapshotCopy, &self.database)?;
    let first = self.read_pass();
    let restore = run_snapshot_capture_hook(
        SnapshotCaptureHookPhase::AfterSnapshotCopy,
        &self.database,
    );
    let first = first?;
    restore?;
    self.revalidate()?;
    let second = self.read_pass()?;
    self.revalidate()?;
    if first != second {
        return Err(StateError::RollbackGuard(
            "legacy SQLite source family changed while its retained snapshot was captured".to_owned(),
        ));
    }
    write_captured_family(destination_main, &first)
}
```

`read_pass` returns ordered `(suffix, Vec<u8>)` entries with `""` for the main file. `write_captured_family` uses `OpenOptions::new().write(true).create_new(true)`, `ensure_private_file`, `write_all`, and `sync_all`; destination sidecars are formed by appending the captured suffix to `destination_main.file_name()`. Any partial files remain only inside the owned scratch attempt.

- [ ] **Step 4: Add the thread-local two-phase test hook**

Under `cfg(test)`, define a thread-local boxed `FnMut(SnapshotCaptureHookPhase, &Path) -> StateResult<()>`, plus setters/clearers matching the interfaces in Task 1. Production `run_snapshot_capture_hook` is a no-op. The `AfterSnapshotCopy` invocation must occur after the retained first-pass attempt even when that attempt returns an error.

- [ ] **Step 5: Register the focused module and run unit formatting/checks**

Add `mod sqlite_snapshot;` to `lib.rs`, then run:

```text
cargo fmt --all
cargo check -p orchestrator-state --all-targets --all-features
```

Expected: the new capture module compiles; the Task 1 regressions still fail behaviorally until legacy import uses it.

### Task 3: Move inspection and apply to scratch-only SQLite

**Files:**
- Modify: `crates/orchestrator-state/src/legacy_import.rs`

**Interfaces:**
- Consumes: `GuardedSqliteFamily` from Task 2 and existing `LegacyImportScratch`.
- Produces: `open_snapshot_read_only(path: &Path) -> StateResult<Connection>` that is called only with owned scratch paths.
- Changes: `inspection_scratch_fingerprint(canonical_root: &Path, canonical_database: &Path) -> String`.
- Removes: `GuardedSourceConnection`, its `Deref` implementation, and `open_source_read_only`.

- [ ] **Step 1: Make inspection scratch available before SQLite opens**

Construct `GuardedSqliteFamily` immediately after the existing missing-database check. Derive the deterministic lock/scavenger key with length-delimited normalized canonical root and database strings:

```rust
fn inspection_scratch_fingerprint(root: &Path, database: &Path) -> String {
    let mut digest = Sha256::new();
    digest.update(b"colay/legacy-import-inspection-scratch/v2\0");
    for path in [root, database] {
        let normalized = path.to_string_lossy().replace('\\', "/");
        update_digest_length(&mut digest, normalized.len());
        digest.update(normalized.as_bytes());
    }
    hex::encode(digest.finalize())
}
```

Acquire `LegacyImportScratch`, capture to `scratch/source.db`, and only then call `open_snapshot_read_only` on that private path.

- [ ] **Step 2: Validate scratch and back up to a stable validated image**

Replace guard-boundary revalidations with scratch connection validation. Run migration status, integrity/FK, live-daemon, and source-identity checks on captured `source.db`. Online-back up that connection to `scratch/validated.db`, hash and size `validated.db`, and migrate `validated.db` to `migrated.db`. Compute `source_root_hash` from the family canonical root saved before the family is dropped. Keep event/document/JSONL/artifact and logical fingerprint validation unchanged.

- [ ] **Step 3: Reorder apply scratch ownership and capture a fresh family**

Move `LegacyImportScratch::acquire(paths, &plan.source_fingerprint)` after `Self::inspect` and `ensure_plan_unchanged` so the non-reentrant plan lock cannot recursively conflict. Capture a fresh guarded family to `scratch/source.db`, open only that scratch path, repeat status/integrity/FK/daemon/source-identity validation, and online-back up into staged `legacy.db`. Verify staged hash/length against the plan before artifact staging. Continue migration from staged `legacy.db` to scratch `migrated.db`, then rewrite as today.

- [ ] **Step 4: Remove every mutable source SQLite open**

Delete `open_source_read_only`, `GuardedSourceConnection`, the `Deref` import, and all `connection.revalidate`/`connection.guard` uses. `rg -n "sqlite_path|open_source_read_only|GuardedSourceConnection|Connection::open.*source\.database" crates/orchestrator-state/src` must return no source-opening path. Keep `OpenFlags::SQLITE_OPEN_READ_ONLY | SQLITE_OPEN_NO_MUTEX`, zero busy timeout, and `query_only = true` in `open_snapshot_read_only`.

- [ ] **Step 5: Run the new regressions GREEN**

Run:

```text
cargo test -p orchestrator-state sqlite_final_name_substitution -- --nocapture
cargo test -p orchestrator-state sqlite_wal_sidecar_set_change -- --nocapture
```

Expected: both pass. The substitution test reports A's no-history validation rather than B's invalid-database error; the WAL test rejects the changed exact set.

### Task 4: Focused regression and lifecycle verification

**Files:**
- Modify only as failures demonstrate within scope: `crates/orchestrator-state/src/{source_guard,sqlite_snapshot,legacy_import}.rs`
- Modify only as fixture failures demonstrate within scope: `crates/orchestrator-state/tests/legacy_import.rs`

**Interfaces:**
- Verifies all Task 2 and Task 3 interfaces together with migration and state behavior.

- [ ] **Step 1: Run source-guard and capture tests**

```text
cargo test -p orchestrator-state source_parent_aba -- --nocapture
cargo test -p orchestrator-state sqlite_ -- --nocapture
```

- [ ] **Step 2: Run legacy import and migration contracts**

```text
cargo test -p orchestrator-state --test legacy_import -- --nocapture
cargo test -p orchestrator-state --test migration_contract -- --nocapture
```

- [ ] **Step 3: Run all state tests**

```text
cargo test -p orchestrator-state --all-features
```

Expected for every command: exit 0. Fix only failures caused by the stable-source snapshot change, rerunning the failing focused command before continuing.

### Task 5: Full gates, report, diff review, and commit

**Files:**
- Modify: `.superpowers/sdd/global-task-3-report.md`
- Verify: all files changed since commit `86380fc`

**Interfaces:**
- Produces: final review evidence and one scoped implementation commit.

- [ ] **Step 1: Run format and workspace check gates**

```text
cargo fmt --all -- --check
cargo check --workspace --all-targets --all-features
```

- [ ] **Step 2: Run required lint and test gates**

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

- [ ] **Step 3: Append exact evidence to the report**

Record the RED compile causes, focused test command/count results, full gate exit results, stable main-file substitution behavior, WAL set-change rejection, absence of source/target/ledger/file mutation, removal of `sqlite_path`, and the intended implementation commit subject. The final handoff will supply the resulting commit hash. Do not overstate detection of unobservable content changes that return to identical retained bytes between both full-family passes.

- [ ] **Step 4: Inspect the complete scoped diff**

Run:

```text
git diff --check 86380fc..HEAD
git diff --stat 86380fc
git status --short
```

Before the final commit, use `git diff --check` and `git diff --stat`; inspect `git diff` for every changed source/test/report file. Confirm no schema, provider, telemetry, external command, or unrelated file changes.

- [ ] **Step 5: Commit the complete implementation**

Stage only the scoped source, test, plan, and report files, then commit:

```text
git commit -m "fix: snapshot guarded legacy SQLite families"
```

Re-run `git status --short` and require no output.
