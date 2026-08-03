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

## Fix Round 1

Round 1 closed both Important review findings without changing IPC schema version 1.

### Additive older-daemon negotiation

The CLI now sends a repository-only `workspace.doctor.lookup` probe first. A current daemon advertises `legacy_import_evidence_supported: true`; only a registered lookup with that explicit capability and a locally inspected source receives a second evidence-bearing request. An older daemon therefore never sees fields rejected by its schema-v1 `deny_unknown_fields` request type. Once a daemon advertises support, transport, protocol, and evidence-validation failures from the second request propagate unchanged and are never downgraded.

RED evidence before negotiation:

```text
older_daemon_receives_only_repository_doctor_probe
assertion failed: the first payload contained legacy_state_dir and legacy_source_fingerprint

advertised_doctor_evidence_failure_is_not_downgraded
advertised evidence validation failure was downgraded (only one request was sent)
```

The controlled older-daemon regression uses the real lookup client and live legacy-import projector. It proves exactly one `{"repository": ...}` request, an absent capability/status decoded additively, and a CLI warning containing `pending:true, imported:false`.

### Daemon-authoritative source configuration

The daemon now loads effective configuration itself, resolves the configured repository-local source with `RepositoryStatePaths`, and treats `legacy_state_dir` only as a normalized, config-safe expectation. The client cannot select an alternate valid legacy database. A configured non-default repository-local directory continues to work.

Clean RED evidence before the authority fix:

```text
doctor_lookup_rejects_spoofed_alternate_repository_state_dir
Error: client-selected alternate state directory was accepted
```

The spoof regression also snapshots the alternate source database and proves rejection is read-only. The positive authority regression seeds a non-default configured directory and verifies the daemon reports the current fingerprint as `pending:true, imported:false`.

### Round 1 verification

```text
cargo test -p colay --all-features older_daemon_probe_projects_pending_legacy_import_warning -- --nocapture
test result: ok. 1 passed; 0 failed

cargo test -p colay --all-features older_daemon_receives_only_repository_doctor_probe -- --nocapture
cargo test -p colay --all-features advertised_doctor_evidence_failure_is_not_downgraded -- --nocapture
test result: ok. 1 passed; 0 failed (each)

cargo test -p orchestrator-daemon --all-features doctor_lookup -- --nocapture
test result: ok. 6 passed; 0 failed

cargo test -p colay --test global_doctor --all-features live_doctor_ -- --nocapture --test-threads=1
test result: ok. 4 passed; 0 failed

cargo test -p colay --test global_doctor --all-features legacy_import_doctor_ -- --nocapture --test-threads=1
test result: ok. 12 passed; 0 failed

cargo test -p colay --test global_doctor --all-features doctor_deep_checks_a_workspace_through_the_live_daemon -- --nocapture --test-threads=1
test result: ok. 1 passed; 0 failed

cargo fmt --all -- --check
cargo clippy -p orchestrator-daemon -p colay --all-targets --all-features -- -D warnings
git diff --check
```

The final static commands exited 0. The first clippy pass found only a repeated test-fixture field prefix and a 104-line acceptance test; both were refactored without lint allowances. No post-test daemon process referenced this worktree. Full-workspace tests remain owned by Task 7, and the pre-existing untracked `.task4-target/` remains untouched.

## Fix Round 2

Round 2 restores forward compatibility for fix-base schema-v1 clients. `WorkspaceDoctorLookup` no longer carries an unconditional capability field. A repository-only lookup with no evidence request now serializes exactly the legacy lookup field set, so a local legacy reader using `deny_unknown_fields` accepts a new-daemon response.

Capability discovery moved to the separate read-only `workspace.doctor.capabilities` action. With a locally inspected source and a registered lookup, the new client performs:

1. legacy-shaped repository-only lookup;
2. strict capability request with `{}` payload;
3. evidence-bearing lookup only after an exact `legacy_import_evidence_supported: true` response.

Only the exact legacy response `{"status":"error","error":"unsupported IPC action"}` returns the original lookup and warning path. Capability transport errors, any other protocol error, malformed/missing/false capability data, and evidence-validation errors all propagate without downgrade.

### RED: old reader rejects the unconditional lookup field

Before removing the lookup capability field:

```text
cargo test -p orchestrator-daemon --all-features repository_lookup_response_remains_readable_by_legacy_schema_one_client -- --nocapture

repository_lookup_response_remains_readable_by_legacy_schema_one_client ... FAILED
Error: unknown field `legacy_import_evidence_supported`, expected one of
`registered`, `database`, `daemon`, `diagnostics`

test result: FAILED. 0 passed; 1 failed
```

### RED: separate capability negotiation is absent

Before the separate action and client sequence:

```text
cargo test -p colay --all-features capability -- --nocapture

running 5 tests
capability_protocol_failure_is_not_downgraded ... FAILED
capability_transport_failure_is_not_downgraded ... FAILED
malformed_capability_response_is_not_downgraded ... FAILED
older_daemon_exact_unsupported_capability_returns_repository_lookup ... FAILED
supported_capability_sends_evidence_lookup ... FAILED

left: 1, right: 2/3
capability transport/protocol/malformed failure was downgraded
test result: FAILED. 0 passed; 5 failed
```

```text
cargo test -p orchestrator-daemon --all-features workspace_doctor_capability_is_discovered_outside_legacy_lookup_response -- --nocapture

workspace_doctor_capability_is_discovered_outside_legacy_lookup_response ... FAILED
Error: capability action was not dispatched
```

The evidence-validation propagation and controlled older-daemon warning tests also failed at RED because the client returned after one request instead of performing the separate capability request.

### GREEN and regression verification

```text
cargo test -p orchestrator-daemon --all-features repository_lookup_response_remains_readable_by_legacy_schema_one_client -- --nocapture
test result: ok. 1 passed; 0 failed

cargo test -p orchestrator-daemon --all-features workspace_doctor_capability_is_discovered_outside_legacy_lookup_response -- --nocapture
test result: ok. 1 passed; 0 failed

cargo test -p colay --all-features capability -- --nocapture
test result: ok. 5 passed; 0 failed

cargo test -p colay --all-features advertised_doctor_evidence_failure_is_not_downgraded -- --nocapture
cargo test -p colay --all-features older_daemon_probe_projects_pending_legacy_import_warning -- --nocapture
test result: ok. 1 passed; 0 failed (each)

cargo test -p orchestrator-daemon --all-features doctor_ -- --nocapture
test result: ok. 7 passed; 0 failed

cargo test -p colay --test global_doctor --all-features live_doctor_ -- --nocapture --test-threads=1
test result: ok. 4 passed; 0 failed

cargo test -p colay --test global_doctor --all-features legacy_import_doctor_ -- --nocapture --test-threads=1
test result: ok. 12 passed; 0 failed

cargo test -p colay --test global_doctor --all-features doctor_deep_checks_a_workspace_through_the_live_daemon -- --nocapture --test-threads=1
test result: ok. 1 passed; 0 failed

cargo fmt --all -- --check
cargo clippy -p orchestrator-daemon -p colay --all-targets --all-features -- -D warnings
git diff --check
```

All final commands exited 0. The Round 1 daemon-authoritative source resolution and spoof rejection remain green. No post-test daemon process referenced this worktree. Full-workspace tests remain Task 7-owned; `.task4-target/` is still untouched. The tracked-report Minor remains deferred as directed.
