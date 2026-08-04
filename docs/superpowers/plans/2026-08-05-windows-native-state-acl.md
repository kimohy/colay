# Native Windows State ACLs Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace process-heavy Windows state-artifact hardening with retained-handle native ACL enforcement, complete exact-owner workspace receipt reuse, and prove the unchanged ten-second registration budget through Windows and published-nightly WSL QA.

**Architecture:** Keep all Windows `unsafe` and descriptor parsing in the existing `orchestrator-windows-ipc` audited boundary, with separate process-identity and three-principal state-artifact modules so current-user-only pipe/mutex contracts cannot be reused accidentally. `orchestrator-state` retains path checks and maps its existing permission entry points to the safe native API; the client commits the already-started owner receipt only after incumbent imports satisfy their existing durable-response contract.

**Tech Stack:** Rust 1.95, `windows-sys` 0.61, Tokio named pipes, rusqlite/SQLite schema 17, PowerShell, GitHub Actions, npm nightly packaging, WSL 2.

## Global Constraints

- The approved specs are `docs/superpowers/specs/2026-08-04-windows-native-state-acl-design.md` and `docs/superpowers/specs/2026-08-04-workspace-register-receipt-design.md`. Keep `RESPONSE_TIMEOUT=10s` and `CONNECT_TIMEOUT=30s`.
- Exact state DACL: current process-primary-token user, Local System (`S-1-5-18`), and Builtin Administrators (`S-1-5-32-544`), each with exactly `FILE_ALL_ACCESS`; directory ACE flags exactly `OI|CI`, file flags zero; DACL present, non-null, protected, exactly three allow ACEs.
- Preserve owner. Never pass a NULL DACL, follow a reparse point, share delete access, cache path/DACL results, fall back to `whoami.exe`/`hostname.exe`/`icacls.exe`, or downgrade a native error to a warning.
- Resolve identity only with `GetCurrentProcess` + `OpenProcessToken(TOKEN_QUERY)` + `GetTokenInformation(TokenUser)`. Do not use thread tokens. Cache successful immutable identity only; retry failures.
- Do not change existing named-pipe, mutex, or secure-directory-tree owner-plus-one-ACE contracts. Their verifier stays separate.
- Keep `orchestrator-state` free of `unsafe`; preserve canonical/component/symlink/junction/reparse checks, Unix `0700`/`0600`, path-aware errors, legacy sealed-plan reinspection, source bytes, schema, audit, redaction, and approvals.
- Tests/CI use fake providers only; never invoke real Codex/Claude/Gemini/Agy inference there. Use separated executable/argument arrays.
- Never delete a worktree. Each task ends in independent read-only review; fix Critical/Important findings and rerun its GREEN gate.
- Current uncommitted `crates/orchestrator-cli/src/ipc_client.rs` and `crates/orchestrator-cli/tests/global_doctor.rs` are intentional receipt work. Tasks 1-3 preserve them byte-for-byte and stage explicit paths only; Task 4 audits/finishes them without assuming they were staged.
- Final required gates: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`; set process-scoped `CARGO_BUILD_JOBS=1` before full Windows verification.

## File Structure

- Create `crates/orchestrator-windows-ipc/src/process_identity.rs`: process-token SID, owned validation/conversion, success-only cache.
- Create `crates/orchestrator-windows-ipc/src/state_artifact.rs`: retained handle, bounded ACL parser, exact verifier/builder, repair/post-verify.
- Modify `crates/orchestrator-windows-ipc/src/lib.rs`: safe re-exports; the crate's existing
  `Win32_Security`, `Win32_Security_Authorization`, `Win32_Storage_FileSystem`, and
  `Win32_System_Threading` features already cover the required calls, so do not add an unrelated
  Windows feature.
- Modify `crates/orchestrator-state/Cargo.toml`, `Cargo.lock`, and `src/permissions.rs`: Windows target dependency/backend switch; delete utility path; retain path/Unix contracts.
- Modify `crates/orchestrator-cli/src/ipc_client.rs`, `tests/global_doctor.rs`, and `tests/global_concurrency.rs`: finish receipt/fallback/durable concurrency tests.
- Create `scripts/qa/windows-state-acl-stress.ps1`; modify `docs/testing.md` and `docs/qa/wsl-nightly-error-tracker.md`.

## Stable Interfaces

```rust
pub use process_identity::current_process_user_sid;
pub use state_artifact::{
    StateArtifactKind, ensure_private_state_artifact, verify_private_state_artifact,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StateArtifactKind { File, Directory }

pub fn current_process_user_sid() -> std::io::Result<String>;
pub fn ensure_private_state_artifact(path: &Path, kind: StateArtifactKind) -> io::Result<()>;
pub fn verify_private_state_artifact(path: &Path, kind: StateArtifactKind) -> io::Result<()>;
```

`ensure` opens one existing target with `READ_CONTROL|WRITE_DAC`, validates the pinned identity/kind/reparse state, returns without writing when exact, otherwise installs one non-null ACL and re-verifies DACL+owner through the same handle. `verify` opens with `READ_CONTROL` and never writes. Both share read/write but not delete.

```rust
enum PingReadiness {
    Legacy,
    Owner { owner_pid: u32, startup_workspace_id: Option<WorkspaceId> },
}
struct ReadyEndpoint { endpoint: DaemonEndpoint, startup_workspace_id: Option<WorkspaceId> }
fn ping_readiness(response: &IpcResponse) -> anyhow::Result<PingReadiness>;
fn startup_workspace_receipt(
    readiness: PingReadiness,
    disposition: ReadyChildDisposition,
) -> Option<WorkspaceId>;
```

Receipt returns `Some` only for `LiveOwner`; incumbent/`NoSpawn`, `ReapContender`, legacy, old peer, or pre-spawn discovery must call `workspace.register`.

---

### Task 1: Process-primary-token SID resolver and success-only cache

**Files:**
- Create: `crates/orchestrator-windows-ipc/src/process_identity.rs`
- Modify: `crates/orchestrator-windows-ipc/src/lib.rs:1-45`
- Test: `crates/orchestrator-windows-ipc/src/process_identity.rs`

**Interfaces:**
- Consumes: `GetCurrentProcess`, `OpenProcessToken`, `GetTokenInformation`, `IsValidSid`, `GetLengthSid`, `ConvertSidToStringSidW`, close-on-drop handle/local allocation patterns.
- Produces: public `current_process_user_sid`; crate-private `current_process_user() -> io::Result<&'static ProcessUser>` where `pub(crate) struct ProcessUser` owns SID bytes/text and exposes `pub(crate) fn sid_bytes(&self) -> &[u8]` to Task 2.

- [ ] **Step 1: Write RED cache/token tests and declarations**

Declare/re-export the new module without touching current IPC calls or changing the existing
`windows-sys` feature list. Tests: first resolver error raw code 5 is not cached and second success
is cached (`calls==2`); 16 barrier-released callers execute one successful resolver; public native
SID is numeric canonical `S-1-...` and stable across two calls.

- [ ] **Step 2: Run RED**

Run: `cargo test -p orchestrator-windows-ipc process_identity -- --nocapture`

Expected: FAIL to compile because `SuccessCache`, `ProcessUser`, and resolver functions do not exist.

- [ ] **Step 3: Implement safe owned identity**

```rust
const MAX_TOKEN_USER_BYTES: u32 = 64 * 1024;
struct AlignedBuffer { words: Box<[usize]>, byte_len: usize }
impl AlignedBuffer {
    fn zeroed(byte_len: usize) -> io::Result<Self> {
        let width = size_of::<usize>();
        let words = byte_len.checked_add(width - 1)
            .ok_or_else(|| io::Error::other("aligned buffer length overflow"))? / width;
        Ok(Self { words: vec![0; words].into_boxed_slice(), byte_len })
    }
    fn as_bytes(&self) -> &[u8] {
        // SAFETY: `words` is initialized, suitably aligned storage and `byte_len` is bounded by it.
        unsafe { slice::from_raw_parts(self.words.as_ptr().cast(), self.byte_len) }
    }
    fn as_mut_void_ptr(&mut self) -> *mut c_void { self.words.as_mut_ptr().cast() }
}
pub(crate) struct ProcessUser { sid: AlignedBuffer, sid_text: String }
impl ProcessUser { pub(crate) fn sid_bytes(&self) -> &[u8] { self.sid.as_bytes() } }
struct SuccessCache<T> { value: OnceLock<T>, resolve: Mutex<()> }
impl<T> SuccessCache<T> {
    const fn new() -> Self { Self { value: OnceLock::new(), resolve: Mutex::new(()) } }
    fn get_or_resolve(&self, f: impl FnOnce() -> io::Result<T>) -> io::Result<&T> {
        if let Some(value) = self.value.get() { return Ok(value); }
        let _guard = self.resolve.lock()
            .map_err(|_| io::Error::other("process identity cache lock was poisoned"))?;
        if let Some(value) = self.value.get() { return Ok(value); }
        let resolved = f()?;
        let _ = self.value.set(resolved);
        self.value.get()
            .ok_or_else(|| io::Error::other("process identity cache publication failed"))
    }
}
static PROCESS_USER: SuccessCache<ProcessUser> = SuccessCache::new();
pub fn current_process_user_sid() -> io::Result<String> {
    Ok(current_process_user()?.sid_text.clone())
}
```

`resolve_process_user`: `OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token)` into an
owned handle; first `GetTokenInformation(TokenUser,null,0,&required)` must fail with
`ERROR_INSUFFICIENT_BUFFER`; require `size_of::<TOKEN_USER>() <= required <= 64KiB`. Allocate an
initialized `usize`-aligned buffer large enough for `required` bytes before treating its prefix as
`TOKEN_USER`; never cast a `Vec<u8>`/`Box<[u8]>` to `TOKEN_USER` or `PSID`. The second call fills the
buffer. With checked start/end arithmetic, require `TOKEN_USER.User.Sid` inside the returned bytes;
inspect the bounded SID header first, compute `8 + 4 * SubAuthorityCount`, require that exact range
inside the returned buffer, then call `IsValidSid`/`GetLengthSid` and require the reported length to
equal the bounded structural length. Copy the SID into separately aligned owned storage before
closing the token. Convert that owned SID using `ConvertSidToStringSidW`, own with `LocalFree`, and
reject missing NUL/invalid UTF-16. Stable stages: `process-token open`, `token-user size query`,
`token-user query`, `token-user bounds`, `SID text conversion`. Never log SID bytes or call
`OpenThreadToken`.

- [ ] **Step 4: GREEN and commit/review**

```powershell
cargo test -p orchestrator-windows-ipc process_identity -- --nocapture
cargo test -p orchestrator-windows-ipc -- --nocapture
cargo fmt --all -- --check
cargo clippy -p orchestrator-windows-ipc --all-targets -- -D warnings
git add crates/orchestrator-windows-ipc/src/lib.rs crates/orchestrator-windows-ipc/src/process_identity.rs
git diff --cached --check
git commit -m "feat: resolve Windows process identity natively"
```

Review token/local-allocation lifetimes, two-call error contract, pointer bounds, failure retry/concurrency, and unchanged one-principal IPC tests. Fix findings in `fix: harden Windows process identity` and rerun GREEN.

### Task 2: Retained-handle exact three-principal DACL engine

**Files:**
- Create: `crates/orchestrator-windows-ipc/src/state_artifact.rs`
- Modify: `crates/orchestrator-windows-ipc/src/lib.rs:1-50`
- Test: `crates/orchestrator-windows-ipc/src/state_artifact.rs`

**Interfaces:**
- Consumes: Task 1 cached binary user SID; bounded SYSTEM/Admin SIDs via `CreateWellKnownSid` and `SECURITY_MAX_SID_SIZE`.
- Produces: stable `StateArtifactKind`, `ensure_private_state_artifact`, `verify_private_state_artifact`; internal `VerifiedDescriptor { owner_sid: Box<[u8]> }` and aligned non-null `OwnedAcl`.

- [ ] **Step 1: Write bounded-parser RED tests**

```rust
fn verify_acl_bytes(
    acl: Option<&[u8]>, protected: bool, kind: StateArtifactKind,
    principals: &ExpectedPrincipals<'_>,
) -> io::Result<()>;
```

Synthetic owned buffers cover missing/NULL, short header, `AclSize`/bytes-in-use overflow,
ACE-count mismatch, truncated header/SID-start/SID, invalid SID, ACE range/size/trailing bytes,
duplicate/missing/unknown/broad trustee, deny/audit/object/callback/inherited ACE, wrong/extra mask,
file/directory flags, unprotected DACL, and order-independent exact acceptance. Before calling any
Windows SID validator, parse the bounded eight-byte SID header, compute
`8 + 4 * SubAuthorityCount` with checked arithmetic, and require that exact SID range to end within
the containing ACE.

- [ ] **Step 2: Run parser RED then implement exact parsing**

Run: `cargo test -p orchestrator-windows-ipc state_artifact::tests::bounded -- --nocapture`

Expected: FAIL to compile. Implement checked slice offsets; require protected, non-null, exactly three ACEs; each `AceType == ACCESS_ALLOWED_ACE_TYPE as u8`, mask `FILE_ALL_ACCESS`, flags zero or exactly `OBJECT_INHERIT_ACE|CONTAINER_INHERIT_ACE`, valid SID ending exactly at ACE end, and one unique required binary trustee. Reject trailing bytes/all other types.

- [ ] **Step 3: Write retained-handle/repair RED tests**

Add tests for file+directory idempotent fast path, permissive and deny repair, read-only tamper detection/no mutation, wrong kinds, reparse rejection, owner preservation, required post-write verify, parent repair preserving protected file+directory children, and observed unprotected-child propagation not accepted as target hardening. Test tampering/building uses native non-null ACL helpers, not `icacls`; a serialized test-only counter around the sole `SetSecurityInfo` proves zero exact-path writes and one repair write.

- [ ] **Step 4: Implement target pinning and descriptor extraction**

```rust
let access = READ_CONTROL | if writable { WRITE_DAC } else { 0 };
let flags = FILE_FLAG_OPEN_REPARSE_POINT | match kind {
    StateArtifactKind::File => 0,
    StateArtifactKind::Directory => FILE_FLAG_BACKUP_SEMANTICS,
};
let handle = CreateFileW(path, access, FILE_SHARE_READ | FILE_SHARE_WRITE,
    ptr::null(), OPEN_EXISTING, flags, ptr::null_mut());
```

No delete sharing/non-inheritable handle. Before ACL access, `GetFileInformationByHandle` rejects reparse and exact kind mismatch. `GetSecurityInfo(SE_FILE_OBJECT, OWNER_SECURITY_INFORMATION|DACL_SECURITY_INFORMATION)` returns LocalAlloc-owned descriptor; `GetSecurityDescriptorLength` bounds owner/DACL pointers; `GetSecurityDescriptorControl` requires `SE_DACL_PROTECTED`; `GetSecurityDescriptorDacl` requires present non-null DACL. Copy validated owner SID.

- [ ] **Step 5: Implement bounded ACL construction/repair/post-verify**

```rust
let ace_len = size_of::<ACCESS_ALLOWED_ACE>()
    .checked_sub(size_of::<u32>()).and_then(|n| n.checked_add(sid_len))
    .ok_or_else(bounds_error)?;
let acl_len = size_of::<ACL>().checked_add(sum_ace_len).ok_or_else(bounds_error)?;
if acl_len > usize::from(u16::MAX) { return Err(bounds_error()); }
InitializeAcl(acl, acl_len as u32, ACL_REVISION);
AddAccessAllowedAceEx(acl, ACL_REVISION, exact_flags, FILE_ALL_ACCESS, sid);
```

Build deterministic user/SYSTEM/Admin order in aligned storage and validate it. `ensure` retains one handle and process-local repair mutex; exact read returns; mismatch calls exactly `SetSecurityInfo(handle, SE_FILE_OBJECT, DACL_SECURITY_INFORMATION|PROTECTED_DACL_SECURITY_INFORMATION, null, null, non_null_acl, null)`, then same-handle exact DACL plus binary-equal owner verification. `verify` never reaches write. Stages: `target open`, `object-kind validation`, `descriptor read`, `ACL construction`, `descriptor write`, `post-write verification`.

- [ ] **Step 6: GREEN, commit, and security review**

```powershell
cargo test -p orchestrator-windows-ipc state_artifact -- --nocapture
cargo test -p orchestrator-windows-ipc -- --nocapture
cargo fmt --all -- --check
cargo clippy -p orchestrator-windows-ipc --all-targets -- -D warnings
git add crates/orchestrator-windows-ipc/src/lib.rs crates/orchestrator-windows-ipc/src/state_artifact.rs
git diff --cached --check
git commit -m "feat: harden Windows state ACLs natively"
```

Review pointer/ACL bounds, ownership/freeing, no-delete-sharing/reparse/kind ordering, exact ACEs/non-null ACL, owner/propagation, and IPC separation. Fix in `fix: close native state ACL review findings`; rerun GREEN.

### Task 3: Switch orchestrator-state to the native backend

**Files:**
- Modify: `crates/orchestrator-state/Cargo.toml:1-30`
- Modify: `Cargo.lock:924-942`
- Modify/Test: `crates/orchestrator-state/src/permissions.rs:1-1050`
- Test: `crates/orchestrator-state/tests/legacy_import.rs`

**Interfaces:**
- Consumes: Task 2 public APIs.
- Produces: unchanged `ensure_private_directory`, `ensure_private_file`, `verify_private_file`, and Windows `current_windows_user_sid` state APIs.

- [ ] **Step 1: Add target dependency and RED mapping tests**

```toml
[target.'cfg(windows)'.dependencies]
orchestrator-windows-ipc = { path = "../orchestrator-windows-ipc" }
```

Regenerate the local package dependency entry with `cargo metadata --format-version 1 > $null`, then require `cargo metadata --locked --format-version 1 > $null`. Tests cover permissive/deny repair, idempotence, verify-no-mutation, path traversal/link/reparse precheck, wrong kind/access-denied path+native stage, native/current state SID equality.

- [ ] **Step 2: Capture receipt diff and run RED**

```powershell
git diff -- crates/orchestrator-cli/src/ipc_client.rs crates/orchestrator-cli/tests/global_doctor.rs > $env:TEMP\receipt-before.patch
cargo test -p orchestrator-state permissions::tests::windows_ -- --nocapture
```

Expected: new native-stage assertions FAIL against the utility backend. Do not stage receipt files.

- [ ] **Step 3: Replace Windows backend and delete obsolete command path**

```rust
fn set_windows_permissions(path: &Path, kind: StateArtifactKind) -> StateResult<()> {
    let _guard = windows_acl_guard()?;
    let target = canonical_acl_target(path)?;
    orchestrator_windows_ipc::ensure_private_state_artifact(&target, kind)
        .map_err(|error| StateError::io(&target, error))
}
fn verify_file_permissions(path: &Path, _: &fs::Metadata) -> StateResult<()> {
    let target = canonical_acl_target(path)?;
    orchestrator_windows_ipc::verify_private_state_artifact(&target, StateArtifactKind::File)
        .map_err(|error| StateError::io(&target, error))
}
pub fn current_windows_user_sid() -> StateResult<String> {
    orchestrator_windows_ipc::current_process_user_sid()
        .map_err(|error| StateError::io(Path::new("Windows process primary token"), error))
}
```

Map directory/file to stable enum. Keep path checks, canonical target, mutex, Unix/other branches. Remove utility identity/alias/constants/stages/retries/capture/timeout/trusted resolution, `Command`, icacls save/text/ACE parser, and obsolete tests. No `whoami.exe`, `hostname.exe`, `icacls.exe`, or `Command::new` remains.

- [ ] **Step 4: GREEN, preservation check, commit/review**

```powershell
cargo test -p orchestrator-state permissions::tests --all-features -- --nocapture
cargo test -p orchestrator-state --test legacy_import --all-features -- --nocapture --test-threads=1
cargo test -p orchestrator-state --all-features -- --nocapture
rg -n "whoami\.exe|hostname\.exe|icacls\.exe|Command::new" crates/orchestrator-state/src/permissions.rs
cargo fmt --all -- --check
cargo clippy -p orchestrator-state --all-targets --all-features -- -D warnings
git diff -- crates/orchestrator-cli/src/ipc_client.rs crates/orchestrator-cli/tests/global_doctor.rs > $env:TEMP\receipt-after.patch
git diff --no-index -- $env:TEMP\receipt-before.patch $env:TEMP\receipt-after.patch
```

Expected: Cargo PASS, `rg` exits 1/no matches, receipt diff comparison exits 0. Then explicit commit:

```powershell
git add crates/orchestrator-state/Cargo.toml crates/orchestrator-state/src/permissions.rs Cargo.lock
git diff --cached --check
git commit -m "refactor: use native Windows state ACLs"
```

Review dependency/platform boundaries, path/error/Unix/import/source invariants and utility removal. Fix in `fix: preserve state permission invariants`; rerun GREEN.

### Task 4: Finish owner-bound receipt and durable registration tests

**Files:**
- Modify/Test: `crates/orchestrator-cli/src/ipc_client.rs:83-105,337-382,522-710,980-1770`
- Modify/Test: `crates/orchestrator-cli/tests/global_doctor.rs:136-210,1596-1710`
- Modify/Test: `crates/orchestrator-cli/tests/global_concurrency.rs:22-151,585-710`

**Interfaces:**
- Consumes: daemon receipt commit `4402cd1` and Task 3 native backend.
- Produces: stable typed receipt interfaces and exact-live-spawned-owner skip only.

- [ ] **Step 1: Audit existing uncommitted work and RED evidence**

Run `git status --short`, `git diff --check -- <two receipt files>`, and inspect their full diff. Expected work includes typed parsing, live-owner selection, cold count-two, and incumbent second-workspace test. If absent, recreate these contracts. The incumbent test's recorded pre-native RED is the ten-second response timeout; after Task 3 do not revert hardening to recreate it.

- [ ] **Step 2: Complete typed parse/fallback unit tests**

Accept legacy `{ready:true}`, old `{ready:true,owner_pid:42}`, and a valid UUID string. Reject
not-ready, zero/non-number owner, receipt without owner, explicit null, every other non-string
receipt, and malformed UUID. A field is optional only when absent; a present malformed value fails
readiness closed. Assert receipt only for `LiveOwner`; `NoSpawn`, incumbent, `ReapContender`,
legacy, and old peer return none.

- [ ] **Step 3: Run focused unit RED/GREEN**

```powershell
cargo test -p colay --lib readiness_response_preserves_version_one_legacy_shape --features test-fixtures -- --nocapture
cargo test -p colay --lib malformed_readiness_responses_fail_closed --features test-fixtures -- --nocapture
cargo test -p colay --lib startup_receipt_is_reused_only_for_the_exact_live_owner --features test-fixtures -- --nocapture
```

Expected initial null case FAIL, then all PASS. Assert timeout constants unchanged.

- [ ] **Step 4: Complete durable cold/incumbent tests**

Cold: source hash unchanged; import/workspace/path `1/1/1`; marker exactly two. Incumbent: start empty workspace, create distinct non-empty schema-v8 session, snapshot SQLite family hashes, run `--json status`, require durable import before response, counts `legacy_imports=1`, `workspaces=2`, `workspace_paths=2`, marker two, published ledger/fingerprint match, source unchanged. Run each exact test five times sequentially; expected 10/10 PASS.

- [ ] **Step 5: Add deterministic distinct-workspace contender test**

In `global_concurrency`, create four distinct non-empty schema-v8 repositories before a barrier, snapshot sources, simultaneously run four `--json status` commands on one fresh home. Assert successful outputs, each unique session in correct workspace, counts workspace/path/import `4/4/4`, one live daemon before stop, all contender records resolved owner/reaped, unchanged hashes, and zero tasks/attempts/worktrees/coordinator/worker leases. `stop_and_verify` must leave no endpoint/process/live lease. This prevents cross-workspace receipt reuse.

- [ ] **Step 6: GREEN, commit, review**

```powershell
cargo test -p colay --test global_concurrency --features test-fixtures distinct_legacy_workspace_contenders_import_once_without_receipt_cross_reuse -- --exact --nocapture --test-threads=1
cargo test -p colay --test global_concurrency --features test-fixtures concurrent_clients_never_observe_sqlite_busy_or_duplicate_rows -- --exact --nocapture --test-threads=1
cargo test -p colay --test global_doctor --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test daemon_lifecycle --features test-fixtures -- --nocapture --test-threads=1
cargo fmt --all -- --check
cargo clippy -p colay --all-targets --all-features -- -D warnings
git add crates/orchestrator-cli/src/ipc_client.rs crates/orchestrator-cli/tests/global_doctor.rs crates/orchestrator-cli/tests/global_concurrency.rs
git diff --cached --check
git commit -m "fix: reuse owner-bound workspace receipt"
```

Review null/malformed parsing, exact PID, all fallback routes, count two, durable/cardinality/source/cleanup/concurrency, and unchanged timeouts. Fix in `fix: preserve workspace registration fallbacks`; rerun GREEN.

### Task 5: Windows stress acceptance and source-fixed docs

**Files:**
- Create: `scripts/qa/windows-state-acl-stress.ps1`
- Modify: `docs/testing.md:55-125`
- Modify: `docs/qa/wsl-nightly-error-tracker.md:970,1759-1780,1877-1885`

**Interfaces:**
- Consumes: fake-provider binary and content-free inspection marker.
- Produces: timestamped JSON evidence and WIN-005 `fix-in-progress (source checks passed; published CI/nightly verification pending)`; WSL-022/023 unchanged.

- [ ] **Step 1: Write hard-failing stress harness**

Arguments: `-ColayExe`, `-FakeProviderExe`, `-EvidenceRoot`. Create isolated home/root; fake-only
flag; clear provider keys; use argument arrays. For every `ProcessStartInfo`, add each executable
argument separately through `ArgumentList.Add`; never build `.Arguments` or invoke a command shell.
Start empty incumbent. Register five distinct non-empty legacy workspaces sequentially; record
hashes/markers/cardinality/times; nearest-rank p95 `sorted[Ceiling(.95*n)-1] <= 5000ms`. Register
four more distinct non-empty workspaces simultaneously; each `<=8000ms`. Observe process ancestry
through `Win32_Process`; fail new attributable `whoami.exe`/`icacls.exe`. Assert response timeout
10, source hashes, exact counts, SQLite integrity/FKs, zero writable tables. Stop and fail residual
endpoint/live row/Colay/fake/whoami/icacls. Emit `summary.json`.

- [ ] **Step 2: Repeat original failures and groups**

```powershell
1..10 | % { cargo test -p colay --test global_doctor --features test-fixtures live_doctor_fails_corrupt_legacy_import_completion_without_mutation -- --exact --nocapture --test-threads=1; if ($LASTEXITCODE) { exit $LASTEXITCODE } }
1..10 | % { cargo test -p colay --test global_doctor --features test-fixtures live_doctor_reports_changed_legacy_import_as_pending -- --exact --nocapture --test-threads=1; if ($LASTEXITCODE) { exit $LASTEXITCODE } }
cargo test -p colay --test global_doctor --features test-fixtures live_doctor_ -- --nocapture --test-threads=1
cargo test -p colay --test global_doctor --features test-fixtures live_doctor_ -- --nocapture
cargo test -p colay --test global_doctor --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_doctor --features test-fixtures -- --nocapture
```

Expected: all PASS, no registration timeout.

- [ ] **Step 3: Run harness, document, commit/review**

```powershell
cargo build -p colay --bins --features test-fixtures
New-Item -ItemType Directory -Force artifacts\qa\windows-state-acl | Out-Null
& scripts\qa\windows-state-acl-stress.ps1 -ColayExe (Resolve-Path target\debug\colay.exe) -FakeProviderExe (Resolve-Path target\debug\colay-e2e-fake-provider.exe) -EvidenceRoot (Resolve-Path artifacts\qa\windows-state-acl)
```

Record commits, exact repeated/group results, five times+p95, four times+max, zero utility launches/residuals, hashes/counts and unchanged timeout in testing/tracker docs. Do not close WIN-005/WSL-022/023. Then:

```powershell
git add scripts/qa/windows-state-acl-stress.ps1 docs/testing.md docs/qa/wsl-nightly-error-tracker.md
git diff --cached --check
git commit -m "test: stress Windows workspace registration"
```

Review ancestry, percentile/thresholds, non-empty sources, durability/cleanup/fake-only/status. Fix in `fix: harden Windows registration stress evidence`; rerun stress.

### Task 6: Full verification, review, publication, and nightly WSL QA

**Files:**
- Verify: entire workspace
- Later modify in a new isolated docs worktree: `docs/qa/wsl-nightly-error-tracker.md`

**Interfaces:**
- Consumes: Tasks 1-5 and retained evidence.
- Produces: reviewed/green/merged PR, green main CI+Release, one exact nightly WSL QA, separate reviewed tracker closure PR.

- [ ] **Step 1: Exact Windows full gate**

```powershell
$env:CARGO_BUILD_JOBS='1'; $env:CARGO_INCREMENTAL='0'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
npm test
node --test scripts/release/test/workflow-contract.test.mjs
```

Every command must PASS; timeout/OOM/ignored/retry-only is not a pass.

- [ ] **Step 2: WSL fresh Linux-native full Clippy log**

```powershell
wsl.exe -- bash -lc 'set -euo pipefail; d=$(mktemp -d /home/kimohy/.cache/colay-clippy.XXXXXX); export CARGO_TARGET_DIR="$d/target" CARGO_BUILD_JOBS=1 CARGO_INCREMENTAL=0; cd /mnt/c/Users/kimoh/Documents/Codex/2026-07-18/principal-rust-engineer-ai-agent-orchestration/.worktrees/workspace-register-latency; cargo clippy --workspace --all-targets --all-features -- -D warnings 2>&1 | tee "$d/clippy.log"; test ${PIPESTATUS[0]} -eq 0; echo "$d/clippy.log"'
```

Require PASS and retained WSL-native log.

- [ ] **Step 3: Independent final security/correctness review**

Review merge-base diff plus both specs: token/cache failures, pointer/ACL bounds/non-null, handle/reparse/kind/owner, exact three principals, unchanged IPC one-principal, state Unix/path/import, receipt fallbacks, concurrency, timeout/fake-only. Fix all Critical/Important atomically; repeat full gates and affected stress.

- [ ] **Step 4: Push/draft PR, green CI, normal merge**

```powershell
git status --short --branch
git diff origin/main...HEAD --check
git push -u origin codex/fix-workspace-register-latency
gh pr create --draft --base main --head codex/fix-workspace-register-latency --title "fix: remove Windows workspace registration bottleneck" --body-file .superpowers/sdd/2026-08-04-workspace-register-receipt/pr-body.md
```

PR body cites WIN-005, rejected timeout-only fix, security/receipt semantics, evidence, fake-only,
rollback. Require all Ubuntu/macOS/Windows push+PR checks green; root-fix failures, never accept a
flaky rerun. Mark ready and merge with `gh pr merge --merge`; omit `--delete-branch` so the branch
remains, and never delete worktrees.

- [ ] **Step 5: Require merge main CI and Release green**

Identify exact merge-triggered CI and Release runs; every platform/native build/smoke/attestation/npm publish job must pass. On failure stop nightly QA and fix through another reviewed PR.

- [ ] **Step 6: Resolve one exact nightly and clean-install in isolated WSL**

After both green, query `npm view @colay/cli@nightly version dist.integrity dist.tarball --json` once and pin that exact version. Under fresh `/home/kimohy/.cache` npm prefix, `COLAY_HOME`, evidence and WSL-native committed repo: install; prove `which`; root/native npm integrity/provenance; release SHA256SUMS/attestation; ELF identity/binary SHA; exact `--version`.

- [ ] **Step 7: WSL product/provider verification**

Require schema 17, migration/init, compatibility, doctor, daemon start/status/restart/stop, SQLite integrity/FKs/cleanup. For Codex/Claude/Gemini/Agy equally: resolve and prove Linux-native before one public version/help/compat probe; reject `/mnt/<drive>`/PE before invocation and record marker.

- [ ] **Step 8: Bounded authorized real-provider QA**

For each already account-ready provider, at most one manual plan-only turn asking exactly `CLEAN_OK`; no retry for auth/quota/credit/capability/terminal failures, no secrets. After every attempt require schema-17 redacted evidence, replay no second invocation, and zero tasks, attempts, worktrees, coordinator leases, worker leases. Real inference stays manual post-release only.

- [ ] **Step 9: Cleanup and separate tracker closure PR**

Stop daemon; require endpoint/live row/process cleanup, SQLite integrity/FKs, unchanged source, zero writable rows. Preserve redacted evidence; remove only verified isolated WSL runtime paths, never worktrees. In a new docs worktree update WIN-005/WSL-022/WSL-023 only with exact merge/run/job/nightly/integrity/SHA/schema/doctor/daemon/SQLite/provider/Agy/real-turn/zero-write/cleanup evidence; review, green CI, normal merge.

## Self-Review Checklist

- [ ] Tasks cover primary-token cache, bounds, exact DACL/non-null/owner/propagation, state mapping, receipt fallbacks, Windows stress, publication, Agy, and separate closure.
- [ ] Stable API/type names agree across tasks; IPC timeout and schema remain unchanged.
- [ ] No command fallback/cache/thread token/schema mutation/real-provider automated test/worktree deletion/premature closure appears.
- [ ] Tasks 1-3 preserve the two receipt files; Task 4 alone commits them.
