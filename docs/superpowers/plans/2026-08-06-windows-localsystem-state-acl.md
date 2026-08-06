# Windows LocalSystem State-ACL Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Windows state-artifact ACL construction and verification support LocalSystem by normalizing only the current-user/SYSTEM role collision while preserving every existing fail-closed ACL invariant.

**Architecture:** Keep all logic inside `orchestrator-windows-ipc::state_artifact`. Build one validated `ExpectedPrincipals` view from the three roles, pass that same view to the bounded verifier and native ACL builder, and keep the public `orchestrator-state` API unchanged. Normal users retain the existing three-ACE byte order; LocalSystem receives exactly two unique trustees.

**Tech Stack:** Rust 2024, `windows-sys`, existing bounded ACL parser/native retained-handle tests, Cargo workspace verification.

## Global Constraints

- The only permitted role collision is current process user SID equal to SYSTEM SID.
- Reject current user equal to Builtin Administrators, SYSTEM equal to Builtin Administrators, and all-three-equal synthetic inputs before ACL construction or verification.
- Compare validated binary SID bytes only; do not use SDDL text, localized account names, or SID prefixes.
- Actual ACLs must contain every required unique trustee exactly once; duplicate ACEs remain invalid.
- Normal-user canonical order remains current user, SYSTEM, Administrators; LocalSystem canonical order is SYSTEM, Administrators.
- Preserve exact masks, inheritance flags, protected/non-null DACL requirements, owner preservation, retained non-reparse handles, and same-handle post-write verification.
- Keep all Windows `unsafe` and provider-specific wire details inside their existing crates; `orchestrator-domain` remains vendor-neutral and I/O-free.
- Do not change persisted schema, IPC schema, audit records, provider behavior, Unix permissions, named-pipe/mutex policy, or default telemetry.
- Automated tests and CI must not invoke real `codex`, `claude`, `gemini`, or `agy` inference.
- Preserve unrelated dirty worktree changes and never delete a worktree.
- Required final verification: `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

### Task 1: Normalize required principals and use one view in builder and verifier

**Files:**
- Modify: `crates/orchestrator-windows-ipc/src/state_artifact.rs:142-220`
- Modify: `crates/orchestrator-windows-ipc/src/state_artifact.rs:236-378`
- Modify: `crates/orchestrator-windows-ipc/src/state_artifact.rs:464-548`
- Test: `crates/orchestrator-windows-ipc/src/state_artifact.rs:1077-1583`
- Test: `crates/orchestrator-windows-ipc/src/state_artifact.rs:1585-1895`

**Interfaces:**
- Consumes: validated role SID byte slices from `OwnedExpectedPrincipals`.
- Produces: `ExpectedPrincipals::from_roles(user, system, administrators) -> io::Result<ExpectedPrincipals<'_>>`, `ExpectedPrincipals::as_slice() -> &[&[u8]]`, and unchanged public `ensure_private_state_artifact` / `verify_private_state_artifact` behavior.

- [ ] **Step 1: Add RED tests for the permitted and forbidden role collisions**

Add test helpers and tests beside the existing synthetic SID constants. The exact assertions must cover the normalized order and every forbidden collision:

```rust
#[test]
fn required_principals_only_normalize_local_system_collision() -> io::Result<()> {
    let local_system = ExpectedPrincipals::from_roles(&SYSTEM_SID, &SYSTEM_SID, &ADMIN_SID)?;
    assert_eq!(
        local_system.as_slice(),
        [SYSTEM_SID.as_slice(), ADMIN_SID.as_slice()]
    );

    let normal = ExpectedPrincipals::from_roles(&USER_SID, &SYSTEM_SID, &ADMIN_SID)?;
    assert_eq!(
        normal.as_slice(),
        [
            USER_SID.as_slice(),
            SYSTEM_SID.as_slice(),
            ADMIN_SID.as_slice()
        ]
    );
    Ok(())
}

#[test]
fn required_principals_reject_other_role_collisions() {
    for result in [
        ExpectedPrincipals::from_roles(&ADMIN_SID, &SYSTEM_SID, &ADMIN_SID),
        ExpectedPrincipals::from_roles(&USER_SID, &ADMIN_SID, &ADMIN_SID),
        ExpectedPrincipals::from_roles(&SYSTEM_SID, &SYSTEM_SID, &SYSTEM_SID),
    ] {
        assert_invalid(result.map(|_| ()), "role SID collision");
    }
}
```

- [ ] **Step 2: Add RED parser tests for the exact LocalSystem ACL**

Add `local_system_principals()` and `exact_local_system_file_acl()` helpers, then prove exact two-ACE acceptance, order independence, count enforcement, and actual duplicate rejection:

```rust
fn local_system_principals() -> ExpectedPrincipals<'static> {
    ExpectedPrincipals {
        sids: [&SYSTEM_SID, &ADMIN_SID, &ADMIN_SID],
        len: 2,
    }
}

#[test]
fn bounded_localsystem_acl_requires_each_unique_trustee_once() {
    let expected = local_system_principals();
    for exact in [
        acl(&[TestAce::allow(&SYSTEM_SID, 0), TestAce::allow(&ADMIN_SID, 0)]),
        acl(&[TestAce::allow(&ADMIN_SID, 0), TestAce::allow(&SYSTEM_SID, 0)]),
    ] {
        assert!(verify_acl_bytes(Some(&exact), true, StateArtifactKind::File, &expected).is_ok());
    }

    for invalid in [
        acl(&[TestAce::allow(&SYSTEM_SID, 0)]),
        acl(&[
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&SYSTEM_SID, 0),
            TestAce::allow(&ADMIN_SID, 0),
        ]),
        acl(&[TestAce::allow(&SYSTEM_SID, 0), TestAce::allow(&EVERYONE_SID, 0)]),
    ] {
        assert!(verify_acl_bytes(Some(&invalid), true, StateArtifactKind::File, &expected).is_err());
    }
}
```

- [ ] **Step 3: Run the focused tests and capture the expected RED**

Run:

```powershell
$env:CARGO_BUILD_JOBS='1'
$env:CARGO_INCREMENTAL='0'
cargo test -p orchestrator-windows-ipc state_artifact::tests::required_principals_ -- --nocapture --test-threads=1
cargo test -p orchestrator-windows-ipc state_artifact::tests::bounded_localsystem_ -- --nocapture --test-threads=1
```

Expected: compile failure because `ExpectedPrincipals::from_roles`, `sids`, and `len` do not yet exist, or assertion failure because the verifier still requires exactly three ACEs. Record this RED before editing implementation code.

- [ ] **Step 4: Implement the minimal normalized principal view**

Replace the role-field-only borrowed struct with one checked view. Validate all SIDs before comparing them:

```rust
const MAX_EXPECTED_PRINCIPALS: usize = 3;

struct ExpectedPrincipals<'a> {
    sids: [&'a [u8]; MAX_EXPECTED_PRINCIPALS],
    len: usize,
}

impl<'a> ExpectedPrincipals<'a> {
    fn from_roles(
        user: &'a [u8],
        system: &'a [u8],
        administrators: &'a [u8],
    ) -> io::Result<Self> {
        for sid in [user, system, administrators] {
            validate_sid_prefix(sid)?;
        }
        if system == administrators || user == administrators {
            return Err(invalid_acl("required state ACL roles have an unsupported role SID collision"));
        }
        if user == system {
            return Ok(Self {
                sids: [system, administrators, administrators],
                len: 2,
            });
        }
        Ok(Self {
            sids: [user, system, administrators],
            len: 3,
        })
    }

    fn as_slice(&self) -> &[&'a [u8]] {
        &self.sids[..self.len]
    }
}
```

Change `OwnedExpectedPrincipals::borrowed` to return `io::Result<ExpectedPrincipals<'_>>`. In `ensure_private_state_artifact`, `verify_private_state_artifact`, and test-support callers, build the view once and pass `&ExpectedPrincipals` to both `read_descriptor_state` and `OwnedAcl::build`.

- [ ] **Step 5: Make the bounded verifier require the exact dynamic trustee count**

Use `principals.as_slice()` for count, membership, and completion:

```rust
let required = principals.as_slice();
let required_count = u16::try_from(required.len())
    .map_err(|_| structural_acl("required principal count overflow"))?;
if ace_count != required_count {
    return Err(policy_acl("DACL trustee ACE count does not match the exact required principal count"));
}

let mut found = [false; MAX_EXPECTED_PRINCIPALS];
```

In `verify_ace_record`, replace the role `if/else` chain with an exact position lookup in `as_slice()`. Continue rejecting an already-found position as `DACL contains a duplicate trustee`, and require `found[..required.len()]` to be all true.

- [ ] **Step 6: Make `OwnedAcl::build` consume the same view**

Change its signature to `fn build(kind: StateArtifactKind, principals: &ExpectedPrincipals<'_>)`. Use `principals.as_slice()` for SID validation, checked capacity, and `AddAccessAllowedAceEx` calls. Update comments from “all three ACEs” to “all required ACEs.” Do not add another deduplication branch in the builder.

- [ ] **Step 7: Add builder and conditional native fast-path coverage**

Add a synthetic builder test that parses the two-ACE result back through the same view for both file and directory kinds. Add a conditional native test that runs only when the actual process user equals SYSTEM; when applicable it must perform `ensure -> verify -> ensure` for one file and one directory and assert the second ensure performs zero additional `SetSecurityInfo` calls.

Name the tests:

```rust
owned_acl_builds_exact_normal_and_localsystem_principal_sets
retained_handle_localsystem_file_and_directory_fast_paths_when_applicable
```

- [ ] **Step 8: Run focused and crate-level GREEN verification**

Run:

```powershell
$env:CARGO_BUILD_JOBS='1'
$env:CARGO_INCREMENTAL='0'
cargo test -p orchestrator-windows-ipc state_artifact::tests::required_principals_ -- --nocapture --test-threads=1
cargo test -p orchestrator-windows-ipc state_artifact::tests::bounded_localsystem_ -- --nocapture --test-threads=1
cargo test -p orchestrator-windows-ipc state_artifact --all-features -- --nocapture --test-threads=1
cargo test -p orchestrator-state permissions::tests::windows_ --all-features -- --nocapture --test-threads=1
```

Expected: all selected tests pass; normal-user existing tests remain three-principal exact; no real provider processes start.

- [ ] **Step 9: Commit the implementation atomically**

```powershell
git add -- crates/orchestrator-windows-ipc/src/state_artifact.rs
git diff --cached --check
git commit -m "fix: normalize LocalSystem state ACL principals"
```

---

### Task 2: Correct the durable security, test, and QA documentation

**Files:**
- Modify: `docs/superpowers/specs/2026-08-04-windows-native-state-acl-design.md`
- Modify: `docs/superpowers/plans/2026-08-05-windows-native-state-acl.md`
- Modify: `docs/security.md`
- Modify: `docs/testing.md`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

**Interfaces:**
- Consumes: Task 1 test names, exact commit, and verification results.
- Produces: one consistent documented policy: normally three ACEs, exactly two for LocalSystem, only `user == SYSTEM` normalized, unpatched LocalSystem downgrade unsupported.

- [ ] **Step 1: Amend the original design without erasing history**

Add an explicit 2026-08-06 amendment near the original “exact three ACEs” contract. State that
`2026-08-06-windows-localsystem-state-acl-design.md` controls collisions, count, tests, and rollback.
Update later verification bullets so they say “exact required unique trustees” and explicitly note
normal three / LocalSystem two. Do not silently rewrite historical timing evidence.

- [ ] **Step 2: Add a completed-plan correction note**

Append a dated addendum to `2026-08-05-windows-native-state-acl.md`; do not rewrite the completed
task history. Include the implementation commit, focused test commands/results, the only permitted
collision, and the LocalSystem downgrade limitation.

- [ ] **Step 3: Update security and testing contracts**

In `docs/security.md`, replace “exactly three allow ACEs” with “one ACE for each required unique
binary SID: normally current user/SYSTEM/Administrators, or SYSTEM/Administrators for LocalSystem.”
Retain the explicit trust caveat for elevated Administrators and SYSTEM.

In `docs/testing.md`, add the exact synthetic collision tests, normal-user three-ACE regression,
conditional LocalSystem native fast-path test, and the requirement for bounded native release QA
before `WIN-006` closes.

- [ ] **Step 4: Advance WIN-006 only to evidence-supported status**

In the issue index and `WIN-006` section, set the state to
`fix-in-progress (source tests passed; LocalSystem native/nightly verification pending)` only after
Task 1 tests pass. Record exact test commands and implementation commit. Do not mark fixed or close
`WIN-005`, `WIN-006`, `WSL-022`, or `WSL-023`.

- [ ] **Step 5: Verify and commit only the intended documentation**

Run:

```powershell
rg -n "exactly three|three-principal|LocalSystem|unique binary SID|WIN-006" docs/security.md docs/testing.md docs/qa/wsl-nightly-error-tracker.md docs/superpowers/specs/2026-08-04-windows-native-state-acl-design.md docs/superpowers/plans/2026-08-05-windows-native-state-acl.md
git diff --check -- docs/security.md docs/testing.md docs/qa/wsl-nightly-error-tracker.md docs/superpowers/specs/2026-08-04-windows-native-state-acl-design.md docs/superpowers/plans/2026-08-05-windows-native-state-acl.md
```

Inspect the existing dirty documentation before staging so prior WIN-005 evidence and reviewed
stress-harness notes are preserved. Then stage only these explicit paths and commit:

```powershell
git add -- docs/security.md docs/testing.md docs/qa/wsl-nightly-error-tracker.md docs/superpowers/specs/2026-08-04-windows-native-state-acl-design.md docs/superpowers/plans/2026-08-05-windows-native-state-acl.md
git diff --cached --check
git commit -m "docs: record LocalSystem state ACL verification"
```

---

### Task 3: Independent review and verification gate

**Files:**
- Review: `crates/orchestrator-windows-ipc/src/state_artifact.rs`
- Review: all Task 2 documentation
- Test: entire Cargo workspace

**Interfaces:**
- Consumes: Task 1 implementation commit and Task 2 documentation commit.
- Produces: review-ready source with fresh focused and workspace-wide evidence; no release-state claims.

- [ ] **Step 1: Request a security/correctness review**

The reviewer must check binary SID validation before equality, collision policy, array/slice bounds,
builder/verifier shared view, normal-user byte stability, actual duplicate rejection, unchanged
pipe/mutex policy, owner/handle invariants, and every documentation status claim. Fix every
Critical/Important finding with a new RED test before proceeding.

- [ ] **Step 2: Run focused Windows verification after review fixes**

```powershell
$env:CARGO_BUILD_JOBS='1'
$env:CARGO_INCREMENTAL='0'
cargo test -p orchestrator-windows-ipc state_artifact --all-features -- --nocapture --test-threads=1
cargo test -p orchestrator-state permissions::tests --all-features -- --nocapture --test-threads=1
```

Expected: all tests pass with nonzero counts and no real provider invocation.

- [ ] **Step 3: Run the mandatory workspace gates with fresh exit codes**

```powershell
$env:CARGO_BUILD_JOBS='1'
$env:CARGO_INCREMENTAL='0'
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: every command exits 0. A timeout without an exit code is not a pass; rerun with captured
output after confirming no stale Cargo process remains.

- [ ] **Step 4: Record exact verification without overstating native coverage**

Update `WIN-006` with test counts, commands, commit hashes, and whether the conditional native test
actually ran as LocalSystem or skipped on a normal-user host. Keep native/nightly verification
pending when the host was not LocalSystem.

- [ ] **Step 5: Commit review fixes or verification-only documentation**

Stage only reviewed files. Use `fix: close LocalSystem ACL review findings` for code changes or
`docs: record LocalSystem ACL source verification` when only evidence text changed. Confirm
`git diff --cached --check` before each commit.
