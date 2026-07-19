# CI Permissions Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make every Linux, macOS, and Windows CI job pass without broadening trusted principals or hiding verification failures.

**Architecture:** Keep platform-specific permission logic in `orchestrator-state`. Represent the current Windows identity as an exact SID plus a locally verified SDDL alias, canonicalize security-sensitive test roots before persistence, and expose redacted nested-command diagnostics from the fake CLI E2E fixture when its verification gate fails.

**Tech Stack:** Rust 1.95, standard library `Command`, `icacls.exe`, `whoami.exe`, `hostname.exe`, Cargo tests and Clippy.

## Global Constraints

- Use separated executable and argument values; no shell interpolation.
- Preserve fail-closed DACL verification and existing redaction/audit behavior.
- Tests and CI must not invoke real provider inference.
- Do not merge or delete the worktree. Push only because the user explicitly requested branch publication and CI monitoring.

---

### Task 1: Windows current-identity SDDL alias

**Files:**
- Modify and test: `crates/orchestrator-state/src/permissions.rs`

**Interfaces:**
- Produces: private `WindowsIdentity { sid: String, alias: Option<&'static str> }` used by `set_windows_permissions`, `verify_file_permissions`, and `verify_private_dacl`.

- [x] **Step 1: Write failing regression tests**

Add Windows-only tests for the desired private interfaces:

```rust
struct WindowsIdentity {
    sid: String,
    alias: Option<&'static str>,
}

fn local_administrator_alias(
    sid: &str,
    account_authority: &[u8],
    hostname: &[u8],
) -> Option<&'static str>;
```

Assert that `local_administrator_alias` returns `Some("LA")` only for an `S-1-5-21-...-500` SID whose account authority equals the ASCII-trimmed hostname, and returns `None` for RID 1001 or a domain/hostname mismatch. Pass `D:P(A;;FA;;;LA)(A;;FA;;;SY)(A;;FA;;;BA)` to `verify_private_dacl` and require success only when `WindowsIdentity.alias == Some("LA")`.

- [x] **Step 2: Verify the tests fail**

Run: `cargo test -p orchestrator-state --all-features windows_local_administrator_alias -- --nocapture`

Expected: compilation failure because `WindowsIdentity` and `local_administrator_alias` do not exist yet. This is the RED state for the desired API.

- [x] **Step 3: Implement the minimal identity mapping**

Add `WindowsIdentity` and implement `local_administrator_alias` with all three conditions:

```rust
let local_rid_500 = sid
    .strip_prefix("S-1-5-21-")
    .and_then(|suffix| suffix.rsplit_once('-'))
    .is_some_and(|(_, rid)| rid == "500");
(local_rid_500
    && account_authority.eq_ignore_ascii_case(hostname.trim_ascii()))
.then_some("LA")
```

Replace `current_user_sid` with `current_windows_identity`. Reuse the existing trusted `whoami.exe /user /fo csv /nh` result for the SID and first CSV field's authority. Only for an RID 500 SID, invoke trusted `hostname.exe` with an empty argument slice and derive the alias using the helper. Pass `&WindowsIdentity` through `set_windows_permissions`, `verify_file_permissions`, and `verify_private_dacl`. Use `(identity.sid.as_str(), identity.alias)` for the current-principal entry in the existing `required` array.

- [x] **Step 4: Verify the tests pass**

Run: `cargo test -p orchestrator-state --all-features windows_local_administrator_alias -- --nocapture`

Expected: all matching regression tests pass.

### Task 2: Unix Rust 1.95 Clippy compatibility

**Files:**
- Modify: `crates/orchestrator-state/src/permissions.rs:124`

- [x] **Step 1: Apply the semantic-equivalent expression**

Replace `metadata.permissions().mode() & 0o077 == 0` with `metadata.permissions().mode().trailing_zeros() >= 6`.

- [x] **Step 2: Verify formatting and local Clippy**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: both commands exit successfully.

### Task 3: Full verification

**Files:**
- Review all changed files.

- [x] **Step 1: Run required tests**

Run: `cargo test --workspace --all-features`

Expected: all tests pass; if the known transient Windows `git` process Access Denied recurs, rerun only that failing fake-provider integration test once and report both results.

- [x] **Step 2: Inspect the final diff**

Run: `git diff --check` and `git status --short`.

Expected: no whitespace errors and only the planned permission/test/documentation changes.

### Task 4: Canonical macOS test root

**Files:**
- Modify and test: `crates/orchestrator-cli/src/app.rs:6178`

**Interfaces:**
- Consumes: `std::fs::canonicalize(&Path) -> io::Result<PathBuf>`.
- Produces: a canonical temporary root shared by `StatePaths` and `WorkerRequest::workspace_root`.

- [x] **Step 1: Confirm the existing regression is red on macOS CI**

Use CI run `29666672339` as the RED result: `app::tests::unconfirmed_process_tree_blocks_task_and_redacts_audit_detail` fails with `SymlinkEscape("/var")` because macOS exposes the temporary directory through the `/var` symlink.

- [x] **Step 2: Canonicalize the temporary root before deriving state paths**

Replace direct uses of `temporary.path()` in the test with:

```rust
let temporary_root = fs::canonicalize(temporary.path())?;
let root = temporary_root.join("state");
```

Set `WorkerRequest::workspace_root` to `temporary_root` so the state root and workspace root use the same canonical path.

- [x] **Step 3: Run the focused test locally**

Run: `cargo test -p colay --all-features unconfirmed_process_tree_blocks_task_and_redacts_audit_detail -- --nocapture`

Expected: the focused test passes. The pushed macOS CI job supplies the platform-specific GREEN verification.

### Task 5: Windows fake CLI failure diagnostics

**Files:**
- Modify and test: `crates/orchestrator-cli/tests/fake_cli_handover_e2e.rs`

**Interfaces:**
- Produces: `verification_stderr(repository: &Path, output: &Value) -> Result<String>`, which reads only the already-redacted `.stderr.log` artifacts for the reported task ID.

- [x] **Step 1: Establish the failure classification**

Run the full `fake_cli_handover_e2e` test binary three times with `COLAY_TEST_FAKE_PROVIDERS_ONLY=1` and empty provider API keys.

Expected: if all three runs pass, record the Windows CI failure as non-reproducible and avoid changing production verification behavior. If a run fails, use its saved stderr to form and test a concrete root-cause hypothesis before modifying production code.

- [x] **Step 2: Add a focused diagnostics test**

Create a temporary `.colay/results/task-1/commands/` tree containing two `.stderr.log` files and one `.stdout.log`. Assert that `verification_stderr` returns the two stderr logs in sorted path order and excludes stdout.

- [x] **Step 3: Verify the diagnostics test is red**

Run: `cargo test -p colay --test fake_cli_handover_e2e --all-features verification_stderr_reports_redacted_command_logs -- --nocapture`

Expected: compilation fails because `verification_stderr` does not exist.

- [x] **Step 4: Implement the minimal diagnostic reader**

Extract `/data/task_id`, construct `.colay/results/<task_id>/commands`, read only regular files ending in `.stderr.log`, sort their paths, and return a bounded diagnostic string. Do not read raw provider output or bypass redaction.

- [x] **Step 5: Attach diagnostics to the completion assertion**

Before asserting `run_completed`, call `verification_stderr(&repository, &output)?` and include it in the assertion message after the JSON envelope.

- [x] **Step 6: Verify the focused and full E2E tests**

Run: `cargo test -p colay --test fake_cli_handover_e2e --all-features -- --nocapture`

Expected: both integration tests pass, and the diagnostic helper test passes.

### Task 6: Required verification, commit, push, and CI

**Files:**
- Review all changed files.

- [x] **Step 1: Run repository-required verification**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Run: `cargo test --workspace --all-features`

Expected: all three commands exit successfully.

- [x] **Step 2: Inspect and commit the final diff**

Run: `git diff --check`, `git status --short`, then commit only the plan and two test files with message `test: stabilize cross-platform CI diagnostics`.

Expected: no whitespace errors and no unrelated files in the commit.

- [ ] **Step 3: Push and monitor the new CI run**

Push `codex/fix-ci-permissions` to `origin`, find the new workflow run for the pushed SHA, and wait for all matrix jobs to complete.

Expected: Ubuntu, macOS, and Windows jobs all succeed. If Windows recurs, its assertion includes the nested redacted `cargo test` stderr required for a root-cause fix.

### Task 7: Cross-platform latent test failures exposed by CI diagnostics

**Files:**
- Modify and test: `crates/orchestrator-cli/tests/fake_cli_handover_e2e.rs`
- Modify and test: `crates/orchestrator-process/src/runner.rs`

**Interfaces:**
- Produces: canonical repository roots for both fake CLI E2E tests.
- Produces: `EnvironmentPolicy::default()` entries required for Rust/MSVC tool discovery on hosted Windows runners.
- Produces: cancellation semantics that ignore only the expected stdin `BrokenPipe` caused by terminating the child process tree.

- [x] **Step 1: Canonicalize both fake CLI E2E repository roots**

In each E2E test, replace `temporary.path().join("repository")` with:

```rust
let repository = fs::canonicalize(temporary.path())?.join("repository");
```

Use macOS CI run `29667425877` as RED evidence: both tests fail during `colay init` with `symbolic-link traversal is forbidden: /var`.

- [x] **Step 2: Add a Windows MSVC environment regression test**

Add a Windows-only unit test that constructs `EnvironmentPolicy::default()` and asserts its private `inherited` set contains `ProgramFiles`, `ProgramFiles(x86)`, `ProgramW6432`, `VCINSTALLDIR`, `VSINSTALLDIR`, `VSCMD_ARG_TGT_ARCH`, `WindowsSdkDir`, and `WindowsSDKVersion`.

- [x] **Step 3: Verify the Windows environment test is red**

Run: `cargo test -p orchestrator-process --all-features default_environment_preserves_msvc_tool_discovery -- --nocapture`

Expected: the assertion fails because the existing allowlist omits the MSVC discovery variables. Windows CI run `29667425877` additionally proves the user-visible failure: Rust resolves GNU `link.exe`, which reports `missing operand` while linking the fixture test.

- [x] **Step 4: Add the minimal MSVC discovery allowlist**

Add only the eight named installation/discovery variables to `EnvironmentPolicy::default()`. Keep credential-name rejection and `env_clear()` unchanged.

- [x] **Step 5: Add a deterministic cancellation/input race regression test**

Start the existing sleeping fixture with a multi-megabyte initial stdin payload, cancel after 100 milliseconds, and require `ProcessRunner::run` to return a `ProcessResult` whose termination is `Cancelled` rather than a `ProcessError::Io(BrokenPipe)`.

- [x] **Step 6: Verify the cancellation test is red**

Run: `cargo test -p orchestrator-process --all-features cancellation_ignores_stdin_broken_pipe_from_terminated_child -- --nocapture`

Expected: the current monitor returns `subprocess I/O failed: Broken pipe` after killing the child, matching Ubuntu CI run `29667425877`.

- [x] **Step 7: Preserve cancellation while keeping unrelated stdin errors fail-closed**

After joining the input task, suppress `io::ErrorKind::BrokenPipe` only when termination is `Cancelled` or `TimedOut`. Continue propagating every stdin error when the child exited normally and every non-`BrokenPipe` error for all termination reasons.

- [x] **Step 8: Run focused cross-platform regression tests**

Run: `cargo test -p orchestrator-process --all-features -- --nocapture`

Run: `cargo test -p colay --test fake_cli_handover_e2e --all-features -- --nocapture`

Expected: all focused tests pass locally; pushed CI supplies macOS and hosted Windows GREEN evidence.

### Task 8: Reverify, commit, push, and monitor CI

**Files:**
- Review all changed files from Task 7 and this plan.

- [x] **Step 1: Run required verification**

Run: `cargo fmt --all -- --check`

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Run: `cargo test --workspace --all-features`

Expected: every command exits successfully.

- [ ] **Step 2: Commit and push**

Commit only the planned files, push `codex/fix-ci-permissions`, and identify the workflow run for the new SHA.

Expected: the branch is clean and synchronized with `origin`.

- [ ] **Step 3: Monitor the complete CI matrix**

Wait for Ubuntu, macOS, and Windows jobs to finish.

Expected: all jobs succeed; do not report completion while any job is pending or failed.

### Task 9: Canonical engine fixtures and complete hosted MSVC environment

**Files:**
- Modify and test: `crates/orchestrator-engine/src/lib.rs`
- Modify and test: `crates/orchestrator-engine/src/checkpoint.rs`
- Modify and test: `crates/orchestrator-engine/src/rollback.rs`
- Modify and test: `crates/orchestrator-engine/src/verification.rs`
- Modify and test: `crates/orchestrator-engine/src/worktree.rs`
- Modify and test: `crates/orchestrator-process/src/runner.rs`

**Interfaces:**
- Produces: test-only `crate::test_support::CanonicalTempDir` with `new() -> io::Result<Self>` and `path() -> &Path`.
- Produces: a static, credential-free Visual Studio build environment allowlist for hosted Windows verification commands.

- [x] **Step 1: Add and adopt a canonical engine tempdir fixture**

Wrap `tempfile::TempDir` in a test-only helper that canonicalizes its path at construction while retaining the original handle for automatic cleanup. Replace every `tempfile::tempdir()?` in the four engine test modules with `CanonicalTempDir::new()?`.

Use macOS CI run `29667820725` as RED evidence: seven engine tests fail with `SymlinkEscape("/var")` or `UnsafePath("/var")`.

- [x] **Step 2: Complete the explicit Visual Studio build environment allowlist**

Add the hosted developer-shell variables needed for MSVC tool and library discovery, including `VCToolsInstallDir`, `VCToolsRedistDir`, `INCLUDE`, `LIB`, `LIBPATH`, `UniversalCRTSdkDir`, `UCRTVersion`, `WindowsLibPath`, `WindowsSdkBinPath`, `WindowsSdkVerBinPath`, `WindowsSDKLibVersion`, `VSCMD_ARG_HOST_ARCH`, `VSCMD_VER`, and `VisualStudioVersion`. Keep the allowlist static; do not inherit arbitrary variables.

- [x] **Step 3: Extend and run focused regression tests**

Extend `default_environment_preserves_msvc_tool_discovery` to cover the complete list. Run the engine test suite and process test suite.

Expected: all local tests pass; Windows CI run `29667820725` remains RED evidence that the prior partial allowlist selected GNU `link.exe`.

- [ ] **Step 4: Run required verification, commit, push, and monitor**

Run the required format, Clippy, and workspace test commands; commit only the planned files; push the branch; and monitor all three CI jobs to completion.

Expected: Ubuntu, macOS, and Windows jobs all succeed.

### Task 10: Canonical state tempdirs

**Files:**
- Modify and test: `crates/orchestrator-state/src/lib.rs`
- Modify and test: state unit-test modules and `MigrationManager::dry_run`
- Modify and test: `crates/orchestrator-state/tests/config_migration.rs`
- Modify and test: `crates/orchestrator-state/tests/migration_contract.rs`

**Interfaces:**
- Produces: internal `CanonicalTempDir` that retains `tempfile::TempDir` and exposes its canonical path.

- [x] **Step 1: Add the state canonical tempdir wrapper**

Map creation and canonicalization failures into `StateError::Io`, retain the original tempdir handle for cleanup, and expose `path() -> &Path`.

- [x] **Step 2: Adopt it in production dry-run and all state unit tests**

Replace direct default tempdir creation in `MigrationManager::dry_run` and state unit tests. Update the checkpoint fixture return type to the wrapper.

- [x] **Step 3: Canonicalize state integration-test roots**

Keep each integration test's `TempDir` handle alive, canonicalize `path()` immediately, and derive all security-sensitive paths from that root.

- [ ] **Step 4: Verify and publish**

Run focused state tests, then required workspace format/Clippy/tests; commit, push, and monitor the complete CI matrix.

Expected: macOS advances through all state and integration tests, and every CI job succeeds.

### Task 11: Canonical multi-provider E2E roots

**Files:**
- Modify and test: `crates/orchestrator-test-support/tests/multi_provider_handover_e2e.rs`

- [x] **Step 1: Preserve canonical state and workspace roots**

Canonicalize both temporary directories immediately and pass only those roots to artifact storage, worker requests, persistence preflight, and verification.

- [ ] **Step 2: Verify and publish**

Run the focused E2E test and required workspace checks, commit, push, and monitor the complete CI matrix.

Expected: the multi-provider handover E2E test no longer rejects macOS `/var` aliases.
