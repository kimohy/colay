# CI Permissions Fix Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the failing Linux/macOS Clippy jobs and Windows permission test pass without broadening trusted principals.

**Architecture:** Keep platform-specific permission logic in `orchestrator-state`. Represent the current Windows identity as an exact SID plus a locally verified SDDL alias, and feed that identity into the existing DACL parser and verifier.

**Tech Stack:** Rust 1.95, standard library `Command`, `icacls.exe`, `whoami.exe`, `hostname.exe`, Cargo tests and Clippy.

## Global Constraints

- Use separated executable and argument values; no shell interpolation.
- Preserve fail-closed DACL verification and existing redaction/audit behavior.
- Tests and CI must not invoke real provider inference.
- Do not push, merge, or delete the worktree.

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
