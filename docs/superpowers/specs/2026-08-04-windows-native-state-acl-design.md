# Native Windows state-artifact ACL design

## Problem and measured evidence

The owner-bound workspace registration receipt removes one redundant legacy inspection from a
fresh daemon start, but it does not make a legitimate registration on an incumbent daemon cheap.
That path must still inspect and durably import a new workspace before replying. Testing the
receipt exposed an independent Windows state-hardening bottleneck:

- A normal non-empty import performs at least `30 + N` state directory/file hardenings, where `N`
  is the number of additional staged source artifacts.
- Each Windows hardening currently starts one `whoami.exe` and eight `icacls.exe` processes: remove
  trusted-principal denies, grant trusted principals, reset, grant again, protect the DACL, remove
  broad grants, verify, and save/read the resulting descriptor.
- The lower bound is therefore `270 + 9N` external processes for one normal import. A local mutex
  serializes each process-heavy mutation sequence inside one process, while concurrent daemon/test
  processes still compete for Windows process, filesystem, and security-subsystem resources.
- Five incumbent registration observations took 17.77, 18.49, 20.75, 18.96, and 23.25 seconds
  overall. Each failed the unchanged ten-second `workspace.register` response deadline.
- Direct legacy-import observations, without the IPC request/response path, took 16.63 and 24.26
  seconds.

The request reaches the daemon writer, and the slow work reproduces without IPC. The involved
SQLite state is small and does not account for the direct-import latency. This is therefore not a
named-pipe readiness problem, a SQLite-size problem, or evidence that the ten-second response
budget should be raised. It is external-process amplification in Windows ACL hardening.

The two fixes remain complementary. The owner-bound receipt is still required to avoid the third
legacy inspection after a fresh bootstrap. Native ACL hardening is required because an incumbent
daemon registering a genuinely new workspace cannot skip the two required inspections or their
state writes.

## Decision

Replace the normal Windows state-artifact `whoami.exe`/`icacls.exe` path with a narrowly scoped,
audited, safe API implemented in the existing Windows FFI boundary. The API opens the exact target
without following a reparse point, pins its identity with a retained handle, verifies an exact
protected DACL, repairs only a mismatching DACL, and verifies the result again through that same
handle.

The exact state-artifact DACL contains three allow ACEs and no others:

1. the current process user's SID with full control;
2. Local System (`S-1-5-18`) with full control; and
3. Builtin Administrators (`S-1-5-32-544`) with full control.

Directory ACEs use object-inherit and container-inherit flags (`OI | CI`). File ACEs have no
inheritance flags. The DACL is non-null and protected. The operation preserves the object's owner.
Unknown trustees, broad trustees, deny ACEs, object/callback ACEs, inherited ACEs, extra access
bits, missing access bits, and non-canonical flags all fail exact verification.

> **2026-08-06 amendment — LocalSystem principal normalization.**
> `2026-08-06-windows-localsystem-state-acl-design.md` controls role collisions, ACE count,
> tests, and rollback for this contract. The required trustees are validated unique binary SIDs:
> normally the current user, SYSTEM, and Builtin Administrators (three ACEs); when the current
> user is LocalSystem, SYSTEM is the same SID and the exact set is SYSTEM and Builtin
> Administrators (two ACEs). Only `current user == SYSTEM` is normalized. Every other role
> collision, and every duplicate ACE in an on-disk ACL, fails closed. An unpatched LocalSystem
> downgrade is unsupported and must fail closed unless this correction is backported.

There is no path-result cache and no DACL-result cache. A same-handle exact-DACL check is the fast
path. Only the successfully acquired current process identity is cached. The process-local ACL
mutex remains for the first implementation so repair and verification sequences in one process are
serialized while correctness and load evidence are collected.

> **2026-08-07 amendment — one authoritative native serialization boundary.**
> The required process-local ACL mutex is `STATE_ARTIFACT_REPAIR` in the Windows FFI boundary.
> Both native ensure and native verify acquire it before non-reparse target pinning and retain it
> through descriptor read, optional complete-DACL write, and post-write verification. The safe
> `orchestrator-state` facade performs component, canonical-path, metadata, and link validation
> without a second global mutex, so preflight for disjoint artifacts may overlap. The removed outer
> mutex neither protected direct native callers nor coordinated external processes and duplicated
> the native gate while serializing unrelated filesystem work. A verifier that reaches the native
> gate before an ensure may fail closed on the pre-repair descriptor; it cannot observe a partial
> same-process repair. No path or ACL result is cached, and retained-handle, no-delete-sharing,
> exact-DACL, owner-preservation, and reparse rejection requirements are unchanged.

## Crate and safety boundary

`orchestrator-state` remains free of `unsafe`. On Windows it gains a target-specific dependency on
`orchestrator-windows-ipc`, which is already the repository's audited Windows security FFI boundary:
it owns handle lifetimes, security descriptor allocation, bounded ACE parsing, reparse-aware file
opens, and current-user-only named-pipe/mutex verification. Its historical crate name is narrower
than this new responsibility, but extending that existing audited boundary is smaller and safer
than creating a second security parser or allowing FFI in the state crate. Renaming the crate is
not part of this fix.

The boundary exposes safe, state-specific operations equivalent to:

```rust,ignore
pub enum StateArtifactKind {
    File,
    Directory,
}

pub fn ensure_private_state_artifact(
    path: &Path,
    kind: StateArtifactKind,
) -> io::Result<()>;

pub fn verify_private_state_artifact(
    path: &Path,
    kind: StateArtifactKind,
) -> io::Result<()>;

pub fn current_process_user_sid() -> io::Result<String>;
```

The exact names may follow existing crate naming, but the semantic split is mandatory:
`ensure` may replace a mismatching DACL and then revalidate it; `verify` is read-only and must not
mutate. These operations apply to one already-created target only. They do not create trees,
recurse, delete, rename, follow links, or relax a descriptor.

The existing named-pipe, mutex, and secure-directory-tree functions retain their current-user-only
owner and one-ACE contracts. The state-artifact normalized unique-principal verifier is separate
and must not be substituted into those IPC paths. Existing pipe endpoint identity validation and
mutex validation must not be loosened.

## Current process identity

The FFI boundary obtains the user SID from the current process's primary token:

1. call `GetCurrentProcess` and `OpenProcessToken` with `TOKEN_QUERY`;
2. call `GetTokenInformation(TokenUser, ...)` first to obtain the bounded buffer size and again to
   populate an owned buffer;
3. validate the returned `TOKEN_USER`, SID pointer range, SID structure, and length with checked
   arithmetic and the Windows SID validation APIs; and
4. copy the canonical SID into owned data before closing the token handle.

Only a successful process-primary-token result is stored in a `OnceLock`-style cache. Failures do
not initialize or poison the cache and are retried by the next caller. Concurrent successful
callers converge on one immutable identity.

Thread-token impersonation is explicitly unsupported. The implementation does not call
`OpenThreadToken`, does not silently prefer an impersonation token, and does not claim to secure
artifacts for an impersonated thread. Colay's daemon and CLI state ownership is defined by the
process primary identity. If impersonation becomes a supported execution model later, it requires
a separate threat model and API rather than a cache change.

`orchestrator-state::current_windows_user_sid` delegates to this safe native function, preserving
its public result shape while eliminating `whoami.exe`. Local Administrator RID/hostname aliases
are no longer needed because native verification compares binary SIDs, not localized SDDL aliases.

## Target acquisition and identity pinning

Before crossing the FFI boundary, `orchestrator-state` keeps its existing component-level
canonical, parent-traversal, symlink, junction, and reparse checks. The safe native operation then
opens the final object with `CreateFileW` using:

- `READ_CONTROL | WRITE_DAC` for `ensure`, or `READ_CONTROL` for read-only `verify`;
- `OPEN_EXISTING`;
- `FILE_FLAG_OPEN_REPARSE_POINT` so the object itself is inspected rather than followed;
- `FILE_FLAG_BACKUP_SEMANTICS` for directories;
- read/write sharing as needed by live SQLite and state workflows, but no delete sharing, so the
  opened identity cannot be replaced while it is checked or repaired; and
- a non-inheritable handle.

Immediately after opening, `GetFileInformationByHandle` verifies that the object is not a reparse
point and that its directory bit exactly matches `StateArtifactKind`. A directory supplied as a
file, a file supplied as a directory, any reparse point, a missing target, or an object that cannot
be pinned fails closed. No ACL operation occurs before these checks pass.

All descriptor reads, the optional mutation, and the post-mutation verification use this retained
handle. The implementation must not canonicalize again and reopen by path between verification and
mutation. The state layer may repeat its existing path checks around creation, but security of the
final ACL transition relies on the pinned handle rather than a path race.

## Exact descriptor verification and repair

`GetSecurityInfo` reads owner and DACL information from the retained file handle. The parser treats
all kernel lengths and pointers as untrusted and validates them before making slices or reading ACE
fields. Verification requires:

- the DACL-present bit set;
- a non-null DACL pointer (a NULL DACL grants access to everyone and is never accepted or passed to
  `SetSecurityInfo`);
- `SE_DACL_PROTECTED` set and no accepted reliance on inherited policy;
- exactly one `ACCESS_ALLOWED_ACE_TYPE` record for each required unique binary SID: normally
  current user, SYSTEM, and Builtin Administrators (three ACEs), or SYSTEM and Builtin
  Administrators for LocalSystem (two ACEs);
- exactly one binary SID match for every required unique trustee;
- full-control file access masks for every ACE;
- exactly `OBJECT_INHERIT_ACE | CONTAINER_INHERIT_ACE` on directory ACEs and zero flags on file
  ACEs; and
- no deny, audit, object, callback, broad, duplicate, unknown, malformed, or trailing ACE data.

ACE comparison is order-independent for acceptance, because trustee order does not change this
allow-only policy. Repair constructs the canonical ACL in deterministic normalized unique-principal
order so repeated reads are stable: current-user, SYSTEM, Administrators for a normal user, and
SYSTEM, Administrators for LocalSystem. All ACL and ACE sizes use checked arithmetic, are bounded
by Windows ACL limits, and are validated with `InitializeAcl`, `AddAccessAllowedAceEx`, and the
relevant SID/ACL validation APIs before installation.

If the first same-handle read is already exact, `ensure` returns without a write. This is the normal
steady-state path. If it differs, `ensure` snapshots the owner SID, builds a non-null bounded ACL,
and calls `SetSecurityInfo` for `DACL_SECURITY_INFORMATION |
PROTECTED_DACL_SECURITY_INFORMATION` only. It does not pass owner information and therefore does
not intentionally change ownership. It then calls `GetSecurityInfo` again on the same handle and
requires the exact DACL plus the unchanged owner SID. Any native error or mismatch is a failure;
there is no optimistic success after `SetSecurityInfo`.

`verify_private_state_artifact` performs only the open and exact read-side verification. It never
calls `SetSecurityInfo`, even when the descriptor is repairable.

## Directory propagation

`SetSecurityInfo` on a directory can propagate inheritable ACE changes to unprotected descendants.
That Windows behavior is security-relevant and must not be assumed away. Colay continues to harden
each created state target explicitly, and every protected child must retain its own exact protected
DACL after a parent repair.

Tests create parent/child hierarchies, tamper the parent, repair it, and prove that already-protected
file and directory children remain exact. Tests also exercise a mismatching unprotected child so
the implementation documents observed propagation without treating inheritance as a substitute
for target hardening. The operation never passes a NULL DACL. Scratch, staging, published import,
lock, database, backup, and recovery artifacts continue to receive their existing target-level
hardening calls.

## State-layer integration and preserved invariants

On Windows, `permissions.rs` maps `ensure_private_directory`, `ensure_private_file`, and
`verify_private_file` to the safe target-only native API. The normal state-hardening path no longer
resolves or starts `whoami.exe`, `hostname.exe`, or `icacls.exe`. There is no silent utility
fallback: an unavailable API, access denial, malformed descriptor, wrong object kind, reparse
point, or failed post-write validation returns an error.

Unix behavior remains exactly `0700` for directories, `0600` for files, and read-only mode
verification. Other-platform behavior is unchanged.

This replacement does not alter:

- component-level canonical/symlink/junction/reparse rejection;
- legacy import plan sealing or the required `apply` reinspection;
- scratch ownership locks, staging creation, atomic publication, orphan cleanup, or backup and
  rollback semantics;
- SQLite schema versions, migration order, workspace cardinality, or source preservation;
- append-only audit semantics, redaction, or explicit approval gates;
- provider selection or execution policy; or
- the ten-second IPC response timeout.

The native call changes only how the same Windows private-state policy is enforced.

## Diagnostics and failure behavior

The safe FFI API returns `io::Error` with a stable operation stage, such as process-token open,
token-user query, target open, object-kind validation, descriptor read, ACL construction,
descriptor write, or post-write verification. `orchestrator-state` maps that error through the
existing path-aware `StateError::Io`/permission context so `doctor` and CLI users see the affected
artifact and the failed security stage.

Policy errors distinguish at least NULL/missing DACL, unprotected DACL, malformed ACE bounds,
unsupported ACE type, unexpected trustee, wrong access mask, wrong inheritance flags, reparse
target, wrong object kind, and owner change. Diagnostics do not dump raw descriptors, arbitrary
file contents, environment values, or credentials. Expected trustee categories may be named, but
the current user's SID need not be repeated in every error.

An `ensure` failure after a write is reported as a failed hardening operation; callers must not
continue with the artifact. The API cannot promise transactional DACL rollback, so correctness
comes from installing one fully constructed ACL in a single `SetSecurityInfo` call and then failing
closed unless exact revalidation succeeds. No code path converts a native failure into a warning.

## Test plan

### Audited FFI boundary

Unit and Windows integration coverage verifies:

- checked ACL/ACE buffer bounds, pointer ranges, truncated headers/SIDs, count/size overflow, and
  trailing data rejection;
- missing and NULL DACL rejection;
- required protected (`P`) control and rejection of inherited/unprotected descriptors;
- exact allow ACE type, exact full-control mask, one ACE for every required unique trustee
  (normally three, LocalSystem two), no duplicates, and exact file versus directory inheritance
  flags;
- rejection of deny, audit, object, callback, unknown, broad, and malformed ACEs;
- file/directory kind mismatch and reparse-point rejection while using the retained handle;
- permissive and deny-containing descriptor repair, owner preservation, exact post-write
  revalidation, and idempotent same-handle fast-path behavior;
- read-only verification detecting tamper without mutation;
- parent repair leaving protected children exact despite directory propagation rules;
- successful current-process SID caching under concurrent callers, plus failure-not-cached and
  next-call retry behavior; and
- unchanged current-user-only named-pipe, mutex, and secure-directory-tree tests, including their
  exact one-principal contracts.

Parser tests use owned synthetic byte buffers and explicit lengths; they do not rely only on SDDL
round trips that could normalize malformed input before the bounded parser sees it.

### State and legacy integration

State permission tests prove Windows file and directory repair from both permissive allow and deny
policies, exact read-only rejection without mutation, and idempotence. Existing Unix permission
tests continue unchanged.

Legacy-state and CLI integration coverage proves:

- a cold legacy daemon start records exactly two inspections: the plan inspection and sealed-plan
  reinspection; the receipt removes only the former third registration inspection;
- an incumbent daemon registers a second workspace containing non-empty legacy state, completes
  durable import before replying, preserves the legacy source, and produces the expected workspace,
  path, import-ledger, and artifact cardinalities;
- the two original Windows `global_doctor` failures are repeated enough to exercise the changed
  path: `live_doctor_fails_corrupt_legacy_import_completion_without_mutation` and
  `live_doctor_reports_changed_legacy_import_as_pending`;
- four distinct workspaces can register concurrently without cross-workspace identity or state
  reuse; and
- fresh-daemon contenders still reuse only an exact owner-bound receipt and leave no duplicate
  import, leaked lease, or residual daemon.

All automated tests and CI use `orchestrator-test-support` fake provider binaries only. They never
invoke real Codex, Claude, Gemini, or Agy inference.

## Performance acceptance and release verification

Performance evidence is manual/stress QA, not a brittle unit microbenchmark. On the Windows CI-like
host, collect and retain:

- five incumbent-daemon registrations of separate non-empty legacy workspaces, with p95 at or below
  five seconds;
- four distinct concurrent non-empty registrations, each completing within eight seconds;
- zero `workspace.register` timeout at the unchanged ten-second response limit;
- process observation showing no `whoami.exe` or `icacls.exe` launch from the normal state
  hardening path; and
- zero residual Colay, provider-test-support, `whoami.exe`, or `icacls.exe` process attributable to
  the run after cleanup.

The evidence also records legacy source hashes, exact inspection counts, import/workspace
cardinality, and elapsed times so speed cannot mask skipped durability work.

Before merge, run the repository-required Windows checks:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Run full workspace/all-target/all-feature Clippy in WSL from a fresh Linux-native target directory
and retain the log. Require an independent security/correctness review, green Windows, Ubuntu, and
macOS pull-request CI, a normal merge, and green merge-triggered main CI and Release. After a new
nightly is published, perform the planned isolated WSL clean-install and all-provider QA; bounded
real-provider probes remain manual and are never added to tests or CI.

## Compatibility and rollback

This change has no persisted-schema, IPC-schema, configuration, audit-record, or provider-wire
format change. Exact native DACLs express the same current user/SYSTEM/Administrators policy as the
existing `icacls` result, so normal-user artifacts hardened by either implementation remain
mutually readable. The LocalSystem two-ACE policy is governed by the 2026-08-06 amendment: an
unpatched binary cannot verify it, so an unpatched LocalSystem downgrade is unsupported and fails
closed unless the correction is backported. The current Windows SID API keeps its existing public
string result.

The required Windows APIs are standard supported security and file-handle APIs already represented
inside the audited FFI crate. Missing access rights or unsupported behavior fails closed; there is
no runtime downgrade to command utilities.

Rollback is code-only: revert to the prior implementation without a database or state migration.
For normal-user artifacts, the exact protected three-ACE DACLs written by the native implementation
remain valid for the old verifier. LocalSystem two-ACE artifacts do not: an unpatched verifier
rejects them, so an unpatched LocalSystem rollback is unsupported and fails closed unless the
correction is backported. Rollback must not delete or rewrite state, weaken ACLs, raise the IPC
timeout, or remove the owner-bound receipt. If production evidence requires disabling native
mutation, writable Windows state operations must fail closed until a reviewed fix is available
rather than silently using an unverified fallback.

## Rejected alternatives

- **Increase the response timeout.** This hides hundreds of external process launches, retains poor
  incumbent usability, and normalizes a blocked daemon writer.
- **Cache only the SID.** Removing `whoami.exe` saves one of nine processes per hardening but leaves
  the eight serialized `icacls.exe` launches and does not meet the measured latency requirement.
- **Add an `icacls` verify fast path.** It helps already-exact targets but new scratch, staging,
  backup, and artifact targets still require many processes; load-sensitive CI remains likely.
- **Cache ACL or path results.** Files and permissions can change after a cache entry. Such a cache
  would turn a security check into stale authorization and could miss tamper or path replacement.
- **Rely only on parent inheritance.** This weakens the target-level protected-DACL invariant and is
  unsafe for moved, restored, staged, or independently tampered artifacts.
- **Remove sealed-plan reinspection.** The second inspection protects import integrity between plan
  construction and apply. It is required even after the receipt and ACL optimizations.
- **Keep a silent `icacls` fallback.** It would reintroduce the same unpredictable latency and make
  production security behavior depend on which implementation happened to run.
