# WSL-Native Provider and Read-Only Conversation Tools Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make plan-only provider tool inspection succeed under an established read-only capability while requiring Linux-native provider executables under WSL for Codex, Claude, Gemini, and Agy.

**Architecture:** Centralize WSL executable rejection in `orchestrator-process`, before any probe or worker spawn, so every provider receives the same native-runtime policy. Keep command authorization in the vendor-neutral conversation orchestrator: only `Advertised` or `Verified` read-only capability converts `CommandStarted` into bounded redacted evidence, while `FileChanged` and all existing lifecycle failures remain terminal.

**Tech Stack:** Rust 1.95, Tokio process runtime, serde/serde_json, thiserror, rusqlite test assertions, `orchestrator-test-support` fake providers, Cargo workspace checks, GitHub Actions, npm nightly packaging, WSL 2 Ubuntu 24.04.

## Global Constraints

- Automated tests and CI invoke only `orchestrator-test-support` fake binaries; never invoke real Codex, Claude, Gemini, or Agy inference.
- Use Rust `Command` with separated executable and arguments; never add shell interpolation.
- Keep provider wire types inside compatibility/provider crates; `orchestrator-domain` remains vendor-neutral and I/O-free.
- Missing usage remains unknown and raw quota units are never compared across providers.
- Plan-only remains `SandboxMode::ReadOnly` and creates no task, task attempt, worktree, coordinator lease, or worker lease.
- Only `CapabilitySupport::Advertised` and `CapabilitySupport::Verified` authorize read-only command evidence; `Unsupported` and `Degraded` fail closed for command events.
- `FileChanged`, write-capable sandbox selection, protocol lifecycle errors, nonzero exits, and process cleanup uncertainty remain failures.
- WSL never launches a provider executable from a Windows-backed mount or a Windows PE image and never falls back to a Windows executable.
- Normal Linux, macOS, and Windows native discovery remains unchanged.
- Preserve schema version 16, append-only audit semantics, redaction, and explicit approval gates; add no migration.
- Writable implementation work remains in the existing isolated `codex/wsl-provider-readonly-policy` worktree; do not delete any worktree.
- Required verification is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

### Task 1: Record the Two Open WSL Nightly Defects

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md:1-95`
- Modify: `docs/qa/wsl-nightly-error-tracker.md:830-870`
- Modify: `docs/qa/wsl-nightly-error-tracker.md` after the `WSL-021` detail section

**Interfaces:**
- Consumes: clean-install QA evidence from nightly `0.1.1-nightly.20260803.28d0d5f`.
- Produces: stable tracker entries `WSL-022` and `WSL-023`, both initially `fix-in-progress`, with reproduction and release completion conditions.

- [ ] **Step 1: Add failing-state entries before changing production code**

Add these rows to the issue index directly after `WSL-021`:

```markdown
| `WSL-022` | high | fix-in-progress | plan-only rejects a provider command even when the read-only sandbox and capability are established |
| `WSL-023` | high | fix-in-progress | WSL provider discovery can expose a Windows executable instead of requiring a Linux-native binary |
```

Add detailed sections containing the exact observed nightly, command, persisted outcome, zero writable-state observation, root cause, and completion conditions. Use these status statements verbatim:

```markdown
- Source completion: fake-provider regression tests pass for all four provider identities and required workspace checks pass.
- Release completion: a newly published nightly passes isolated WSL clean-install QA and the issue status changes to `fixed`.
```

Record that the bounded Codex turn returned `CLEAN_OK` and exit zero but was persisted as failed with `read-only conversation reported command execution`. Record that WSL resolved no native `agy`, while the Windows `agy.exe` below `/mnt/c` is not an acceptable fallback. Do not record credentials, account identifiers, or raw home-directory contents.

- [ ] **Step 2: Verify tracker consistency**

Run:

```powershell
rg -n "WSL-022|WSL-023|0\.1\.1-nightly\.20260803\.28d0d5f|CLEAN_OK" docs/qa/wsl-nightly-error-tracker.md
git diff --check
```

Expected: each ID appears in the index and one detailed section; the observed nightly and redacted outcome are present; `git diff --check` exits zero.

- [ ] **Step 3: Commit the QA record**

```powershell
git add -- docs/qa/wsl-nightly-error-tracker.md
git commit -m "docs: record WSL provider boundary defects"
```

---

### Task 2: Reject Non-Native Provider Executables Under WSL

**Files:**
- Modify: `crates/orchestrator-process/src/executable.rs:1-117`
- Modify: `crates/orchestrator-process/src/executable.rs:245-417`
- Modify: `crates/orchestrator-process/src/executable.rs:530-794`
- Modify: `crates/orchestrator-process/src/runner.rs:33-101`
- Modify: `crates/orchestrator-process/src/runner.rs:103-174`
- Modify: `crates/orchestrator-process/src/lib.rs:10-15`
- Modify: `crates/orchestrator-providers/src/process_runtime.rs:100-114`
- Modify: `crates/orchestrator-providers/src/process_runtime.rs:606-619`

**Interfaces:**
- Consumes: the configured executable, effective `PATH`, working directory, current host WSL evidence, and `/proc/self/mountinfo`.
- Produces: public `ExecutableHostContext { is_wsl: bool, windows_mounts: Vec<PathBuf> }`, `ExecutablePolicy::{General, Provider}`, `ExecutableSearch::{host, policy}`, and `ExecutableResolutionError::WslNonNativeCandidate { configured, candidate, reason }`.
- Produces: `CommandSpec::require_native_provider()`; provider public probes and workers opt into it, while Git and other general subprocesses retain `ExecutablePolicy::General`.
- Produces: private provider-runtime builders `probe_command_spec(executable: &Path, args: &[OsString], redaction: &RedactionConfig) -> CommandSpec` and the existing `command_spec(...)`, both marked as provider executables.
- Produces: `validate_wsl_native_candidate(configured: &Path, candidate: &Path, search: &ExecutableSearch) -> Result<(), ExecutableResolutionError>` called before any provider `ResolvedExecutable` is returned.

- [ ] **Step 1: Write resolver tests that describe the WSL boundary**

Extend `SearchFixture::search` to initialize a native host context, then add a helper for simulated WSL:

```rust
fn wsl_search<const N: usize>(
    &self,
    directories: [&str; N],
    windows_mounts: Vec<PathBuf>,
) -> ExecutableSearch {
    let mut search = self.search(ExecutablePlatform::Unix, "", directories);
    search.host = ExecutableHostContext {
        is_wsl: true,
        windows_mounts,
    };
    search.policy = ExecutablePolicy::Provider;
    search
}

fn make_executable(&self, relative: impl AsRef<Path>) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;

        let path = self.path(relative);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .unwrap_or_else(|error| panic!("set fixture executable permission: {error}"));
    }
    #[cfg(not(unix))]
    {
        let _ = relative;
    }
}
```

Add tests with these exact behaviors:

```rust
#[test]
fn wsl_accepts_a_native_linux_provider_candidate() {
    let fixture = SearchFixture::new();
    fixture.write_bytes("linux/bin/provider", b"#!/bin/sh\nexit 0\n");
    fixture.make_executable("linux/bin/provider");
    let search = fixture.wsl_search(["linux/bin"], Vec::new());

    let resolved = resolve_executable(Path::new("provider"), &search)
        .unwrap_or_else(|error| panic!("resolve native WSL provider: {error}"));

    assert_eq!(resolved.path, fixture.path("linux/bin/provider"));
}

#[test]
fn wsl_rejects_a_provider_on_a_windows_backed_mount() {
    let fixture = SearchFixture::new();
    fixture.write_bytes("windows/bin/provider", b"#!/bin/sh\nexit 0\n");
    fixture.make_executable("windows/bin/provider");
    let mount = fixture.path("windows");
    let search = fixture.wsl_search(["windows/bin"], vec![mount]);

    let error = resolve_executable(Path::new("provider"), &search)
        .expect_err("Windows-backed WSL provider unexpectedly resolved");

    assert!(matches!(error, ExecutableResolutionError::WslNonNativeCandidate { .. }));
    assert!(error.to_string().contains("Install the provider inside this WSL distribution"));
}

#[test]
fn wsl_rejects_a_renamed_pe_candidate() {
    let fixture = SearchFixture::new();
    fixture.write_bytes("linux/bin/provider", b"MZ\x90\0fake-pe");
    fixture.make_executable("linux/bin/provider");
    let search = fixture.wsl_search(["linux/bin"], Vec::new());

    assert!(matches!(
        resolve_executable(Path::new("provider"), &search),
        Err(ExecutableResolutionError::WslNonNativeCandidate { .. })
    ));
}

#[test]
fn wsl_general_process_resolution_is_not_reclassified_as_a_provider() {
    let fixture = SearchFixture::new();
    fixture.write_bytes("windows/bin/git", b"MZ\x90\0fixture");
    fixture.make_executable("windows/bin/git");
    let mut search = fixture.wsl_search(
        ["windows/bin"],
        vec![fixture.path("windows")],
    );
    search.policy = ExecutablePolicy::General;

    let resolved = resolve_executable(Path::new("git"), &search)
        .unwrap_or_else(|error| panic!("general command policy changed: {error}"));

    assert_eq!(resolved.path, fixture.path("windows/bin/git"));
}
```

On Unix, add a separate symlink test whose target contains `MZ`; assert the same typed error. Add focused tests for mountinfo parsing with `drvfs` and `9p` plus `aname=drvfs`, and for lexical `/mnt/c` detection when mountinfo is unavailable. Keep all fixtures local and never invoke them.

Use these concrete assertions for the symlink and parser cases:

```rust
#[cfg(unix)]
#[test]
fn wsl_rejects_a_symlink_to_a_pe_candidate() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let fixture = SearchFixture::new();
    fixture.write_bytes("payload/provider.exe", b"MZ\x90\0fake-pe");
    fs::set_permissions(
        fixture.path("payload/provider.exe"),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap_or_else(|error| panic!("set PE fixture permission: {error}"));
    fs::create_dir_all(fixture.path("linux/bin"))
        .unwrap_or_else(|error| panic!("create symlink parent: {error}"));
    symlink(
        fixture.path("payload/provider.exe"),
        fixture.path("linux/bin/provider"),
    )
    .unwrap_or_else(|error| panic!("create provider symlink: {error}"));
    let search = fixture.wsl_search(["linux/bin"], Vec::new());

    assert!(matches!(
        resolve_executable(Path::new("provider"), &search),
        Err(ExecutableResolutionError::WslNonNativeCandidate { .. })
    ));
}

#[test]
fn mountinfo_parser_recognizes_drvfs_and_drvfs_backed_9p() {
    let mounts = windows_mounts_from_mountinfo(
        "36 25 0:32 / /mnt/c rw - drvfs C: rw\n\
         37 25 0:33 / /windows rw - 9p drvfs rw,aname=drvfs",
    );

    assert!(mounts.contains(&PathBuf::from("/mnt/c")));
    assert!(mounts.contains(&PathBuf::from("/windows")));
}

#[test]
fn lexical_windows_drive_mount_is_detected_without_mountinfo() {
    assert!(is_lexical_windows_drive_mount(Path::new("/mnt/c/tools/agy.exe")));
    assert!(!is_lexical_windows_drive_mount(Path::new("/opt/agy/bin/agy")));
}
```

- [ ] **Step 2: Run the resolver tests and confirm RED**

Run:

```powershell
cargo test -p orchestrator-process executable::tests::wsl_ --all-features
```

Expected: compilation fails because `ExecutableHostContext`, `ExecutableSearch::host`, and `WslNonNativeCandidate` do not exist.

- [ ] **Step 3: Add the host context and typed error**

Add the search context and error shape in `executable.rs`:

```rust
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ExecutableHostContext {
    pub is_wsl: bool,
    pub windows_mounts: Vec<PathBuf>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutablePolicy {
    #[default]
    General,
    Provider,
}

#[derive(Clone, Debug)]
pub struct ExecutableSearch {
    pub platform: ExecutablePlatform,
    pub host: ExecutableHostContext,
    pub policy: ExecutablePolicy,
    pub path: Vec<PathBuf>,
    pub pathext: Vec<OsString>,
    pub working_directory: PathBuf,
}
```

Add this error variant:

```rust
#[error(
    "WSL requires a Linux-native provider executable; rejected configured executable `{configured}` candidate `{candidate}`: {reason}. Install the provider inside this WSL distribution and ensure its Linux binary appears on PATH before any Windows-mounted path"
)]
WslNonNativeCandidate {
    configured: PathBuf,
    candidate: PathBuf,
    reason: String,
},
```

Export `ExecutableHostContext` and `ExecutablePolicy` from `orchestrator-process/src/lib.rs`.

- [ ] **Step 4: Implement current-host and Windows-mount classification**

Add `ExecutableHostContext::current()` as crate-visible code. It must detect WSL from `WSL_DISTRO_NAME`, `WSL_INTEROP`, or a case-insensitive `microsoft` marker in `/proc/sys/kernel/osrelease`. When WSL is detected, read `/proc/self/mountinfo` and retain mount points whose filesystem is `drvfs`, whose source is `drvfs`, or whose `9p` options contain `aname=drvfs`.

Use a pure parser with this interface so tests do not depend on the host:

```rust
fn windows_mounts_from_mountinfo(mountinfo: &str) -> Vec<PathBuf>;
fn is_lexical_windows_drive_mount(path: &Path) -> bool;
```

Decode mountinfo escapes `\040`, `\011`, `\012`, and `\134`. Sort mount roots from longest to shortest before checking `candidate.starts_with(root)`, so nested mounts resolve deterministically.

Change `EnvironmentPolicy::executable_search` to populate:

```rust
ExecutableSearch {
    platform,
    host: ExecutableHostContext::current(),
    policy: ExecutablePolicy::General,
    path,
    pathext,
    working_directory,
}
```

- [ ] **Step 5: Implement fail-closed candidate inspection before resolution succeeds**

Add:

```rust
fn validate_wsl_native_candidate(
    configured: &Path,
    candidate: &Path,
    search: &ExecutableSearch,
) -> Result<(), ExecutableResolutionError>;
```

The function returns immediately outside WSL. Under WSL it canonicalizes the candidate for symlink and mount checks, rejects either the original or canonical path when it is a lexical `/mnt/<single-letter>` drive path or starts with a discovered Windows-backed mount, then opens the canonical file and rejects the `MZ` prefix. I/O inspection failure must map to the existing access-denied/invalid-candidate paths and must never permit execution by default.

Return immediately when `search.policy == ExecutablePolicy::General`; this preserves Git and other non-provider subprocess behavior. Call the validator in both `resolve_explicit` and `resolved_bare` before constructing a provider `ResolvedExecutable`.

Add `executable_policy: ExecutablePolicy` to `CommandSpec`, default it to `General`, and add:

```rust
#[must_use]
pub const fn require_native_provider(mut self) -> Self {
    self.executable_policy = ExecutablePolicy::Provider;
    self
}
```

In `ProcessSupervisor::start`, copy `spec.executable_policy` into the `ExecutableSearch` before `resolve_executable`. Extract the public-probe spec construction into this private helper and call it from `ProcessAdapterRuntime::run_probe`:

```rust
fn probe_command_spec(
    executable: &Path,
    args: &[OsString],
    redaction: &RedactionConfig,
) -> CommandSpec {
    let mut spec = CommandSpec::new(executable)
        .require_native_provider()
        .args(args.iter().cloned());
    spec.timeout = Duration::from_secs(30);
    spec.stdout_limit = 4 * 1024 * 1024;
    spec.stderr_limit = 1024 * 1024;
    spec.redaction = redaction.clone();
    spec
}
```

Make `command_spec` use the same native-provider builder for every worker and fallback transport:

```rust
let mut spec = CommandSpec::new(&invocation.executable)
    .require_native_provider()
    .args(invocation.args.iter().cloned())
    .current_dir(&invocation.working_directory)
    .with_stdin(invocation.stdin.clone());
```

Add provider-runtime unit assertions that both a public probe spec and a worker spec carry `ExecutablePolicy::Provider`. Do not add provider-name branching; all four adapters share `ProcessAdapterRuntime`.

- [ ] **Step 6: Run focused resolver and process tests and confirm GREEN**

Run:

```powershell
cargo test -p orchestrator-process executable --all-features
cargo test -p orchestrator-process --all-features
cargo test -p orchestrator-providers process_runtime --all-features
```

Expected: all resolver tests pass on Windows; Unix-only permission and symlink cases compile only on Unix; existing Windows and native Unix discovery tests remain green.

- [ ] **Step 7: Commit the centralized runtime policy**

```powershell
git add -- crates/orchestrator-process/src/executable.rs crates/orchestrator-process/src/runner.rs crates/orchestrator-process/src/lib.rs crates/orchestrator-providers/src/process_runtime.rs
git commit -m "fix: require native provider executables in WSL"
```

---

### Task 3: Accept Capability-Gated Read-Only Command Events

**Files:**
- Modify: `crates/orchestrator-cli/src/conversation_orchestrator.rs:1-25`
- Modify: `crates/orchestrator-cli/src/conversation_orchestrator.rs:268-393`
- Modify: `crates/orchestrator-cli/src/conversation_orchestrator.rs:439-525`
- Modify: `crates/orchestrator-test-support/src/runtime.rs:20-43`
- Modify: `crates/orchestrator-test-support/src/runtime.rs:446-525`
- Modify: `crates/orchestrator-test-support/src/runtime.rs:735-751`
- Modify: `crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs:88-145`
- Modify: `crates/orchestrator-cli/tests/global_plan_first.rs:125-253`

**Interfaces:**
- Consumes: `WorkerEvent::CommandStarted`, the request sandbox, and the selected provider's `ProviderCapabilities::read_only` value.
- Produces: `observe_read_only_command(provider: ProviderId, sandbox: SandboxMode, support: CapabilitySupport, executable: &str, args: &[String], evidence: &mut ConversationEvidence) -> Option<String>`; `None` means bounded evidence was accepted, while `Some(reason)` is a lifecycle violation.
- Produces: fake scenarios `ReadOnlyCommand` and `ReadOnlyCommandWithFileChange` used only by test support.

- [ ] **Step 1: Write the four-provider command policy tests**

Import `CapabilitySupport` and add unit tests in `conversation_orchestrator.rs`:

```rust
#[test]
fn all_providers_accept_commands_only_with_established_read_only_capability() {
    for provider in [
        ProviderId::Codex,
        ProviderId::Claude,
        ProviderId::Gemini,
        ProviderId::Agy,
    ] {
        for support in [CapabilitySupport::Advertised, CapabilitySupport::Verified] {
            let mut evidence = ConversationEvidence::default();
            let violation = observe_read_only_command(
                provider,
                SandboxMode::ReadOnly,
                support,
                "/bin/sh",
                &["-c".to_owned(), "pwd".to_owned()],
                &mut evidence,
            );
            assert!(violation.is_none(), "{provider} {support:?}");
            let evidence = evidence.finish();
            assert!(evidence.contains("read-only provider command started"));
            assert!(evidence.contains("pwd"));
        }
    }
}

#[test]
fn command_policy_rejects_degraded_unsupported_and_writable_contexts() {
    for support in [CapabilitySupport::Unsupported, CapabilitySupport::Degraded] {
        let mut evidence = ConversationEvidence::default();
        assert!(observe_read_only_command(
            ProviderId::Codex,
            SandboxMode::ReadOnly,
            support,
            "pwd",
            &[],
            &mut evidence,
        )
        .is_some());
    }
    let mut evidence = ConversationEvidence::default();
    assert!(observe_read_only_command(
        ProviderId::Codex,
        SandboxMode::WorkspaceWrite,
        CapabilitySupport::Verified,
        "pwd",
        &[],
        &mut evidence,
    )
    .is_some());
}
```

Add an oversized command test that confirms `ConversationEvidence::finish()` remains at most 64 lines, each line at most 2 KiB, and total evidence at most `CONVERSATION_MAX_EVIDENCE_BYTES`. Include a repeated command and assert first-seen deduplication.

- [ ] **Step 2: Add fake end-to-end reproductions for command success and file-change failure**

Add `ReadOnlyCommand` and `ReadOnlyCommandWithFileChange` to `FakeRuntimeScenario`. In `conversation_lines`, insert this Codex event before the agent message for both scenarios:

```rust
serde_json::json!({
    "type": "item.started",
    "item": {
        "id": "read-only-command",
        "type": "command_execution",
        "command": "/bin/sh -lc pwd",
        "status": "in_progress"
    }
})
```

For `ReadOnlyCommandWithFileChange`, also insert:

```rust
serde_json::json!({
    "type": "item.started",
    "item": {
        "id": "unexpected-write",
        "type": "file_change",
        "path": "README.md",
        "status": "in_progress"
    }
})
```

Make `emit_conversation_fixture` select the scenarios when the redacted transcript contains `scenario:read-only-command` or `scenario:read-only-command-file-change`. Keep normal fake conversations unchanged.

Extend `global_plan_first.rs` with one success and one failure test. The success test must query `conversation_attempts` and assert `status = 'completed'`, canonical `answer_complete`, and evidence containing `read-only provider command started`. Both tests must execute this zero-state loop:

```rust
for table in [
    "tasks",
    "task_attempts",
    "worktrees",
    "coordinator_leases",
    "worker_leases",
] {
    assert_eq!(fixture.count(table)?, 0, "unexpected row in {table}");
}
```

The file-change test must exit nonzero, persist a failed attempt, and contain `read-only conversation reported a file change` in redacted evidence.

- [ ] **Step 3: Run focused tests and confirm RED**

Run:

```powershell
cargo test -p colay --lib conversation_orchestrator::tests::all_providers_accept_commands_only_with_established_read_only_capability --all-features
cargo test -p colay --test global_plan_first --all-features
```

Expected: the unit test does not compile because `observe_read_only_command` is absent, and the end-to-end command case fails under the existing unconditional `CommandStarted` lifecycle error.

- [ ] **Step 4: Implement the minimal capability-gated policy**

Add this helper beside the evidence accumulator:

```rust
fn observe_read_only_command(
    provider: ProviderId,
    sandbox: SandboxMode,
    support: CapabilitySupport,
    executable: &str,
    args: &[String],
    evidence: &mut ConversationEvidence,
) -> Option<String> {
    if sandbox != SandboxMode::ReadOnly
        || !matches!(support, CapabilitySupport::Advertised | CapabilitySupport::Verified)
    {
        return Some(format!(
            "provider command execution lacks an established read-only capability: {executable}"
        ));
    }
    let args = serde_json::to_string(args).unwrap_or_else(|_| "[]".to_owned());
    evidence.push_provider_text(
        provider,
        &format!(
            "read-only provider command started: executable={executable}; args={args}"
        ),
    );
    None
}
```

Before the event loop, copy the selected capability value:

```rust
let read_only_support = self.planner.capabilities[&provider].read_only;
```

Replace the unconditional `CommandStarted` failure arm with:

```rust
Ok(WorkerEvent::CommandStarted {
    executable, args, ..
}) => {
    if let Some(error) = observe_read_only_command(
        provider,
        request.sandbox,
        read_only_support,
        &executable,
        &args,
        &mut evidence,
    ) {
        lifecycle_error = Some(error);
    }
}
```

Do not change the `FileChanged` arm. Do not use `CapabilitySupport::usable()`, because that would incorrectly admit `Degraded`.

- [ ] **Step 5: Run conversation unit, integration, and persistence tests and confirm GREEN**

Run:

```powershell
cargo test -p colay --lib conversation_orchestrator --all-features
cargo test -p colay --test chat_conversation_fake_provider --all-features
cargo test -p colay --test global_plan_first --all-features
cargo test -p orchestrator-test-support --all-features
```

Expected: all four provider identities pass the shared policy matrix; the Codex-like fake end-to-end command turn completes; the file-change turn fails; all writable-state tables remain empty.

- [ ] **Step 6: Commit the conversation policy and fake regressions**

```powershell
git add -- crates/orchestrator-cli/src/conversation_orchestrator.rs crates/orchestrator-test-support/src/runtime.rs crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs crates/orchestrator-cli/tests/global_plan_first.rs
git commit -m "fix: accept verified read-only provider commands"
```

---

### Task 4: Document the Common Provider Policy and Source-Fixed QA State

**Files:**
- Modify: `docs/compatibility.md:1-35`
- Modify: `docs/testing.md:1-12`
- Modify: `docs/testing.md:88-116`
- Modify: `docs/qa/wsl-nightly-error-tracker.md` in `WSL-022`, `WSL-023`, and the update log

**Interfaces:**
- Consumes: passing focused tests from Tasks 2 and 3.
- Produces: user-facing compatibility guidance and tracker state `fix-in-progress` with an explicit `source checks passed; published-nightly verification pending` note. The issues do not become `fixed` in this task.

- [ ] **Step 1: Document capability-gated command evidence**

Add a compatibility paragraph stating:

```markdown
Plan-only conversations always request a read-only sandbox. A normalized command-start event is retained as bounded redacted evidence only when the selected provider advertises or verifies read-only capability. A file-change event, degraded or missing read-only capability, write-capable sandbox, lifecycle error, or nonzero provider exit still fails closed. This policy is identical for Codex, Claude, Gemini, and Agy and does not use a command-name allowlist.
```

- [ ] **Step 2: Document WSL-native provider discovery and remediation**

Add this compatibility guidance:

```markdown
Under WSL, every provider must resolve to a Linux-native executable installed inside the current distribution. Colay rejects provider candidates on Windows-backed mounts and files with a Windows PE signature before version, help, or inference execution. Install the provider inside WSL and place its Linux binary on PATH before Windows-mounted entries; Colay does not fall back to `.exe` candidates.
```

Update `docs/testing.md` to say resolver tests simulate WSL mounts and PE signatures with local inert fixture files. Add the post-release WSL checks: native path, ELF or Linux script identity as appropriate, public probe only after native validation, zero writable state, SQLite integrity, and daemon/socket cleanup.

- [ ] **Step 3: Update the tracker without prematurely closing the defects**

Under each new issue add the commit IDs produced by Tasks 2 and 3 and the focused test results. Set the detail status line to:

```markdown
- Status: fix-in-progress (source checks passed; published-nightly verification pending)
```

Keep the index status `fix-in-progress` until Task 6.

- [ ] **Step 4: Verify documentation and commit**

Run:

```powershell
rg -n "read-only capability|Windows-backed|WSL-022|WSL-023|published-nightly verification pending" docs/compatibility.md docs/testing.md docs/qa/wsl-nightly-error-tracker.md
git diff --check
```

Expected: all policy phrases and both IDs are present; no issue claims published-nightly success; diff check exits zero.

Commit:

```powershell
git add -- docs/compatibility.md docs/testing.md docs/qa/wsl-nightly-error-tracker.md
git commit -m "docs: explain WSL-native provider policy"
```

---

### Task 5: Run Full Verification and Prepare the Pull Request

**Files:**
- Verify: every file changed by Tasks 1 through 4
- No production file is created in this task.

**Interfaces:**
- Consumes: all committed implementation and documentation changes.
- Produces: clean required checks, review evidence, a pushed feature branch, and a pull request whose CI is green on Ubuntu, macOS, and Windows.

- [ ] **Step 1: Run formatting and diff hygiene**

Run:

```powershell
cargo fmt --all -- --check
git diff origin/main...HEAD --check
git status --short
```

Expected: both checks exit zero and the worktree has no uncommitted files.

- [ ] **Step 2: Run full Clippy**

Run:

```powershell
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: exit zero with no warnings.

- [ ] **Step 3: Run the complete Windows test suite**

Run:

```powershell
cargo test --workspace --all-features
```

Expected: exit zero for all unit, integration, and doc tests. Record elapsed time and test totals from Cargo output without summarizing a truncated or interrupted run as success.

- [ ] **Step 4: Run WSL full Clippy from a Linux-native target directory**

Run from WSL with the source mounted read-only or used only as source, and place Cargo output under the WSL filesystem:

```bash
export CARGO_TARGET_DIR="$(mktemp -d /tmp/colay-wsl-target.XXXXXX)"
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: exit zero. Retain the command output location in the PR evidence and do not delete a repository worktree.

- [ ] **Step 5: Review the branch against the approved design**

Invoke `superpowers:requesting-code-review`, then inspect:

```powershell
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
git diff origin/main...HEAD -- crates/orchestrator-process/src/executable.rs crates/orchestrator-cli/src/conversation_orchestrator.rs
```

Confirm no provider-specific permission exception, schema migration, shell interpolation, real-provider test, worktree deletion, or tracker premature closure was introduced. Address valid findings and repeat all affected focused tests plus the required gates.

- [ ] **Step 6: Push and open the pull request**

Invoke `github:yeet` to confirm commit scope, push `codex/wsl-provider-readonly-policy`, and open a draft pull request. Include:

- root-cause evidence for `WSL-022` and `WSL-023`;
- exact security invariants;
- focused test commands and full Windows/WSL check results;
- statement that automated tests used only fake providers; and
- release validation checklist from Task 6.

Mark the pull request ready only after its diff matches the approved design.

- [ ] **Step 7: Monitor CI, repair failures, and merge only when green**

Use `github:gh-fix-ci` for any failing GitHub Actions check. Require all configured Ubuntu, macOS, and Windows checks to pass on the final head commit. Re-run review if a CI fix changes behavior. Merge through the repository's normal reviewed path; do not force-push, bypass protections, or delete the worktree.

---

### Task 6: Validate the Published Nightly in WSL and Close the Tracker

**Files:**
- Modify after successful published-nightly QA: `docs/qa/wsl-nightly-error-tracker.md`
- Preserve: the implementation worktree and timestamped QA evidence directory

**Interfaces:**
- Consumes: merged commit, successful release workflow, and the newly published npm nightly.
- Produces: isolated WSL clean-install evidence for package provenance, all four provider policies, persistence invariants, cleanup, and final `fixed` tracker states through a follow-up documentation pull request.

- [ ] **Step 1: Confirm release workflow and resolve the exact nightly once**

Wait for the merge-triggered release workflow to pass all native build, package validation, smoke, provenance, and npm publication jobs. In WSL with NVM Node 22 or newer:

```bash
nightly="$(npm view @kimohy/colay@nightly version)"
case "$nightly" in
  0.1.1-nightly.202608*) ;;
  *) printf '%s\n' "unexpected nightly: $nightly" >&2; exit 1 ;;
esac
qa_root="$(mktemp -d "$HOME/.cache/colay-wsl-native-provider-qa.XXXXXX")"
mkdir -p "$qa_root/prefix" "$qa_root/home" "$qa_root/repository" "$qa_root/evidence"
npm install --global --prefix "$qa_root/prefix" "@kimohy/colay@$nightly"
export COLAY_HOME="$qa_root/home"
export PATH="$qa_root/prefix/bin:$PATH"
```

Record the exact version, npm integrity, installed package shasum, native binary SHA-256, and merged commit prefix. Do not modify the user's existing global npm installation.

- [ ] **Step 2: Validate package, state, and daemon lifecycle**

In the fresh WSL-native repository create a committed Git baseline using separated Git arguments, then run:

```bash
colay --version
colay --json migrate apply
colay --json doctor
colay --json compatibility
colay --json daemon start
colay --json daemon status
colay --json daemon stop
```

Expected: schema 16, SQLite `integrity_check = ok`, zero foreign-key violations, exact nightly daemon binary identity, final stopped state, no Unix socket, and no remaining QA Colay process.

- [ ] **Step 3: Validate WSL-native discovery for all four providers**

Run public non-inference provider diagnostics for Codex, Claude, Gemini, and Agy. For each provider, record configured and resolved executable paths, version, capabilities, and account-readiness status. Assert every accepted path is WSL-native and is not below `/mnt/c`, `/mnt/d`, or another Windows-backed mount.

If a Linux-native provider is absent but a Windows candidate exists, configure an isolated probe path exposing only that candidate and assert `colay compatibility` rejects it with the common `WSL requires a Linux-native provider executable` remediation without launching it. Use a marker wrapper outside the candidate only to detect attempted invocation; the marker count must remain zero.

- [ ] **Step 4: Run bounded real-provider turns only where account readiness is established**

This is a manual QA step outside tests and CI. For each account-ready provider, invoke at most one fixed-token plan-only turn requesting exactly `CLEAN_OK`. Do not retry quota, authentication, entitlement, or unsupported-client failures and do not substitute another provider.

For each invoked provider assert:

- requested provider identity is preserved;
- a read-only command event, when emitted, is bounded redacted evidence rather than a lifecycle failure;
- the terminal answer is canonical and persisted;
- no file-change event is accepted;
- tasks, task attempts, worktrees, coordinator leases, and worker leases remain zero; and
- replay does not invoke the provider a second time.

If Agy or another provider is not account-ready, record `unverified` or the redacted blocked category and make no inference call.

- [ ] **Step 5: Audit final persistence and cleanup**

Stop the daemon and inspect the isolated database read-only. Confirm schema 16, `integrity_check = ok`, zero foreign-key violations, bounded conversation evidence, no credential values, zero writable task state, and no active lease. Confirm the QA PID and socket are absent. Preserve the timestamped QA root and record its path; do not delete a worktree.

- [ ] **Step 6: Mark both defects fixed only after all release checks pass**

Create a separate documentation branch from the updated `origin/main` using `superpowers:using-git-worktrees`. Update `WSL-022` and `WSL-023` index rows and detail status to `fixed`. Add the exact nightly, merge commit, CI/release run identifiers, npm integrity, binary SHA-256, provider probe results, bounded real-turn results, database counts, and cleanup evidence.

Run:

```powershell
rg -n "WSL-022|WSL-023|fixed|nightly" docs/qa/wsl-nightly-error-tracker.md
git diff --check
```

Commit:

```powershell
git add -- docs/qa/wsl-nightly-error-tracker.md
git commit -m "docs: close WSL provider boundary defects"
```

Push and merge the documentation pull request only after its CI passes. Retain both worktrees according to repository policy.
