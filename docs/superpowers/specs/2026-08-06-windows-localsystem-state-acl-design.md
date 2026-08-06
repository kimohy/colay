# Windows LocalSystem State-ACL Principal Normalization

Status: approved for implementation on 2026-08-06

## Purpose

This document amends the state-artifact DACL contract in
`2026-08-04-windows-native-state-acl-design.md`. Where the two documents conflict, this amendment
controls. The earlier design modeled three security roles as three necessarily distinct trustees.
That assumption is false when Colay runs as LocalSystem: the current process user and the SYSTEM
role are both binary SID `S-1-5-18`.

The change must let LocalSystem create, repair, and verify private state artifacts without weakening
the normal-user policy or accepting duplicate ACL entries.

## Approved policy

The required trustees are derived from these ordered roles:

1. current process user;
2. Local System; and
3. Builtin Administrators.

Normalization compares validated binary SID bytes, not SID text, localized names, role labels, or
prefixes. The only permitted role collision is current user equal to SYSTEM. That collision preserves
the first occurrence, so the canonical repair order is SYSTEM, Administrators. SYSTEM equal to
Administrators, or current user equal to Administrators, is an invalid policy input and fails closed.

For a normal user, the required set remains exactly three trustees and the generated ACL bytes keep
the existing order. For LocalSystem, the required set is exactly `[SYSTEM, Administrators]` and the
canonical DACL contains exactly two allow ACEs. A duplicate ACE in an actual ACL is never accepted;
only the permitted current-user/SYSTEM role collision is normalized before construction and
verification.

## Scope and boundaries

The normalization belongs only to the Windows state-artifact ACL engine in
`orchestrator-windows-ipc::state_artifact` and its `orchestrator-state` callers. It does not change:

- current-user-only named-pipe, mutex, or secure-directory-tree policies;
- Unix `0700`/`0600` behavior;
- state schema, SQLite data, IPC schema, configuration, audit records, or provider wire types;
- owner preservation, retained non-reparse handles, same-handle repair and verification, or the
  fail-closed FFI boundary; or
- provider execution, telemetry, identity, quota, or credential behavior.

## Components and data flow

### Required-principal view

`OwnedExpectedPrincipals` continues to own or borrow the three role inputs. Its borrowed view builds
one validated normalized SID view. The builder and verifier consume that same view; they must not
implement separate collision or normalization rules.

All three input SIDs are validated before normalization. The view contains two entries only when
current user equals SYSTEM and both differ from Administrators. It contains three entries only when
all roles are distinct. Every other collision or cardinality is an internal policy error and fails
closed.

### ACL construction

`OwnedAcl::build` computes capacity and adds one exact `ACCESS_ALLOWED_ACE_TYPE` record for each SID
in the unique view. Every ACE retains the existing exact `FILE_ALL_ACCESS` mask and file/directory
flags. Checked arithmetic, Windows ACL validation, and deterministic ordering remain mandatory.

### ACL verification

The parser requires the on-disk ACE count to equal the unique required-principal count, not merely
to fall in a two-to-three range. Each ACE trustee must match exactly one entry in the unique view.
The verifier remains order-independent, rejects repeated matches, and requires every unique trustee
exactly once.

All existing rejection rules remain unchanged: missing or NULL DACL, unprotected or inherited
policy, deny/audit/object/callback ACEs, unexpected or broad trustees, malformed bounds, trailing
bytes, wrong masks, and wrong flags all fail verification.

## Error handling and observability

Normal errors retain the existing stable stages: ACL construction, descriptor read/write, and
post-write verification. No SID bytes or account names are added to errors or persisted evidence.
An invalid unique-principal cardinality or mismatch is reported as a state ACL policy error and does
not fall back to `icacls.exe`, SDDL text parsing, relaxed permissions, or a broader trustee set.

## Test strategy

Tests use synthetic owned SID buffers and existing Windows-native fixtures; they do not invoke a
provider.

Required unit coverage:

- `user == SYSTEM` normalizes to `[SYSTEM, Administrators]` and a canonical two-ACE file and
  directory ACL builds and verifies;
- `user == Administrators`, `SYSTEM == Administrators`, and all-three-equal synthetic inputs fail
  closed before ACL construction and verification;
- `[SYSTEM, SYSTEM, Administrators]` remains rejected as an actual duplicate ACL;
- `[SYSTEM]`, `[Administrators]`, and ACLs with an unknown or broad trustee are rejected;
- `[SYSTEM, Administrators]` and `[Administrators, SYSTEM]` both verify for LocalSystem, preserving
  order-independent acceptance;
- a normal distinct user still requires exactly three ACEs, and the builder preserves the existing
  user/SYSTEM/Administrators order;
- masks, inheritance flags, protection, ACE types, malformed lengths, and trailing-data failures
  remain identical for both two- and three-principal policies; and
- builder and verifier use the same normalized view, with regression coverage preventing divergent
  trustee counts or ordering.

Required Windows integration coverage:

- when the process actually runs as LocalSystem, file and directory `ensure -> verify -> ensure`
  succeeds, preserves owner and handle identity, and the second ensure takes the exact-DACL fast
  path;
- when CI cannot provide a LocalSystem job, the synthetic unit cases remain mandatory and a bounded
  LocalSystem-native verification is recorded in release QA before closing `WIN-006`; and
- normal-user native state, receipt/concurrency, SQLite integrity/foreign-key, and zero-writable-row
  regressions remain green.

## Documentation and rollout

Implementation updates the original native-ACL spec, completed plan addendum, `docs/security.md`,
`docs/testing.md`, and QA issue `WIN-006` to describe an exact unique-binary-SID policy: normally
three ACEs, exactly two for LocalSystem. It must not claim `WIN-006` fixed until source tests and the
applicable native verification pass, and it must not close `WIN-005` until current Windows stress,
published CI/nightly, and planned WSL validation also pass.

This correction is compatible with existing normal-user artifacts because their canonical ACL is
unchanged. LocalSystem artifacts that previously failed construction or verification become
readable only when they contain the exact two-principal canonical policy. No migration or automatic
permission widening is introduced. A binary without this correction cannot verify the new
LocalSystem two-ACE canonical ACL, so downgrading a LocalSystem state directory to an unpatched build
is unsupported and fails closed unless the correction is backported. This amends the earlier
mutual-readability and rollback statements for LocalSystem only; normal-user rollback compatibility
is unchanged.
