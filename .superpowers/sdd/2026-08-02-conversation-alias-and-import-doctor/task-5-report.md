# Task 5 report: validated live-daemon import completion

## Outcome

Schema-v1 `workspace.doctor.lookup` now carries additive legacy-import evidence without trusting client evidence as durable truth:

- The request retains `repository` and adds serde-defaulted optional `legacy_state_dir` and `legacy_source_fingerprint` fields.
- The response adds a serde-defaulted, omitted-when-absent `legacy_import: Option<LegacyImportDoctorStatus>`.
- The daemon reconstructs `RootConfig` and `RepositoryStatePaths`, independently inspects the current repository-local source, requires its fingerprint to match the client expectation, then calls `LegacyImporter::completed_import(&database, workspace_id, &plan, paths)`.
- Missing, changed-during-request, corrupt, or invalid completion evidence returns a static IPC validation failure. No client fingerprint or configured root can independently imply completion.
- The CLI performs Task 4's source inspection once, sends the inspected expectation, and reuses the same projection function for offline and live checks.
- Unregistered repositories and responses omitting `legacy_import` remain warnings with `pending:true, imported:false`; they never infer completion. No source remains `pending:false, imported:false`.

IPC schema version remains 1. No persisted schema or format changed.

## TDD evidence

### RED: completed live import

Before production edits:

```text
cargo test -p colay --test global_doctor --all-features live_doctor_reports_completed_legacy_import -- --nocapture

running 1 test
test live_doctor_reports_completed_legacy_import ... FAILED

assertion `left == right` failed:
left: String("warn")
right: "pass"

test result: FAILED. 0 passed; 1 failed; 27 filtered out
```

The real CLI output was the old bounded warning, `import readiness is unavailable through live-daemon IPC`, rather than `pending:false, imported:true`.

### RED: additive schema-v1 request

Before the request fields were added:

```text
cargo test -p orchestrator-daemon --all-features workspace_doctor_lookup_payload_preserves_schema_one_compatibility -- --nocapture

running 1 test
test ipc::tests::workspace_doctor_lookup_payload_preserves_schema_one_compatibility ... FAILED

schema-v1 payload must accept optional daemon-validated legacy evidence
test result: FAILED. 0 passed; 1 failed; 32 filtered out
```

The older repository-only payload decoded, while `deny_unknown_fields` correctly rejected the not-yet-implemented evidence fields.

### RED: full live truth table

Still before production edits:

```text
cargo test -p colay --test global_doctor --all-features live_doctor_ -- --nocapture

running 4 tests
test live_doctor_in_unregistered_legacy_workspace_is_read_only ... FAILED
test live_doctor_reports_completed_legacy_import ... FAILED
test live_doctor_reports_changed_legacy_import_as_pending ... FAILED
test live_doctor_fails_corrupt_legacy_import_completion_without_mutation ... FAILED

test result: FAILED. 0 passed; 4 failed; 26 filtered out
```

Completed, changed, and corrupt durable-evidence cases all received the old live warning. The unregistered warning omitted the mandatory `pending` and `imported` fields.

### GREEN

After the minimal implementation:

```text
cargo test -p colay --test global_doctor --all-features live_doctor_ -- --nocapture

running 4 tests
test live_doctor_in_unregistered_legacy_workspace_is_read_only ... ok
test live_doctor_reports_completed_legacy_import ... ok
test live_doctor_fails_corrupt_legacy_import_completion_without_mutation ... ok
test live_doctor_reports_changed_legacy_import_as_pending ... ok

test result: ok. 4 passed; 0 failed; 26 filtered out
```

```text
cargo test -p orchestrator-daemon --all-features doctor_lookup -- --nocapture

running 4 tests
test ipc::tests::workspace_doctor_lookup_payload_preserves_schema_one_compatibility ... ok
test ipc::tests::workspace_doctor_lookup_response_preserves_schema_one_compatibility ... ok
test ipc::tests::doctor_lookup_unregistered_repository_remains_read_only ... ok
test ipc::tests::doctor_lookup_never_trusts_a_client_fingerprint_without_current_source_evidence ... ok

test result: ok. 4 passed; 0 failed; 32 filtered out
```

The response compatibility test decodes both an older response with no `legacy_import` and an evidence-aware typed response.

## Regression and no-mutation coverage

The live integration tests snapshot global table row counts, source bytes/SHA-256, and published import file metadata before and after doctor. They cover:

- exact validated completion: `pending:false, imported:true`;
- independently inspected changed source with no exact ledger: `pending:true, imported:false`;
- structurally corrupt matching ledger: daemon IPC failure plus bounded/redacted failed `legacy_import` projection;
- unregistered valid source at an explicit non-default state directory: warning, never imported, no registration/binding or workspace-directory creation;
- no source through the pre-existing deep live doctor test: `pending:false, imported:false`.

Daemon unit coverage additionally proves a client-only fingerprint with no current source fails, and an unregistered lookup neither registers the repository nor creates its configured state directory.

Task 4's offline projection remained green after moving inspection into the shared single-inspection flow:

```text
cargo test -p colay --test global_doctor --all-features legacy_import_doctor -- --nocapture
test result: ok. 12 passed; 0 failed; 18 filtered out
```

The first broad offline run had one non-assertion Windows `icacls.exe` access-denied abort. The exact affected test passed alone, and the unchanged 12-test command above passed on immediate rerun, so no product or test workaround was introduced.

Additional focused checks:

```text
cargo test -p colay --test global_doctor --all-features doctor_deep_checks_a_workspace_through_the_live_daemon -- --nocapture
test result: ok. 1 passed; 0 failed

cargo test -p colay --test global_doctor --all-features live_doctor_fails_corrupt_legacy_import_completion_without_mutation -- --nocapture
test result: ok. 1 passed; 0 failed
```

## Static verification

```text
cargo fmt --all -- --check
cargo clippy -p orchestrator-daemon --all-targets --all-features -- -D warnings
cargo clippy -p colay --all-targets --all-features -- -D warnings
git diff --check
```

All commands exited 0. Initial clippy identified denied `expect`/`expect_err` calls only in the new tests; those were refactored to fallible test returns and explicit error branching before the clean rerun.

Full-workspace tests were not run because Task 7 owns them. The pre-existing untracked `.task4-target/` was neither used nor staged.

## Self-review

- The live daemon, not the client, reconstructs and inspects repository-local source paths.
- Fingerprint equality is required before querying completion, and completion is reported only from `LegacyImporter::completed_import` after its ledger, binding, audit, mapping, anchor, and published-file validation.
- All daemon-side source/completion failures cross IPC as one static redacted error; the CLI projects the existing bounded remediation text.
- `workspace.doctor.lookup` remains read-only with `workspace_id: None`; no registration, binding, migration, import, source mutation, or persisted-format change was added.
- The CLI source inspection occurs once per doctor invocation and feeds the common offline/live `legacy_import_check` projection.
- Missing optional response data cannot report imported and remains the existing warning for a present source.
- Tests use only local SQLite fixtures and `orchestrator-test-support` fake provider binaries.

No open Task 5 concern was found in the final diff.
