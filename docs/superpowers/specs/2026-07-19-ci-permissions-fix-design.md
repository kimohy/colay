# CI Permissions Fix Design

## Goal

Restore the three-platform GitHub Actions CI run without weakening Unix or Windows state-file permissions.

## Root causes

Rust 1.95 Clippy rejects the explicit Unix group/other mask check as `verbose_bit_mask`, and CI promotes that warning to an error. On the Windows hosted runner, `icacls /save` serializes the current local built-in Administrator SID with the SDDL alias `LA`; the verifier accepts the exact current SID but does not recognize that equivalent alias.

## Considered approaches

1. Allow `LA` unconditionally. This is small but unsafe because an unrelated local Administrator ACE could be accepted for a non-Administrator current user.
2. Infer `LA` from a SID ending in RID 500. This handles the runner but can confuse a domain RID 500 account with the local machine Administrator.
3. Recommended: recognize `LA` only when the current SID ends in RID 500 and the authority in trusted `whoami.exe` output matches the trusted `hostname.exe` output. This preserves exact-principal verification without unsafe Windows API calls or shell interpolation.

## Design

Replace the current SID string with a private `WindowsIdentity` value containing the canonical SID and an optional verified SDDL alias. `current_windows_identity` continues to invoke trusted System32 utilities with separated arguments. It derives `LA` only from a local-machine RID 500 identity. DACL verification accepts that alias solely for the current identity; `SY` and `BA` retain their existing fixed mappings.

The Unix permission check uses `mode().trailing_zeros() >= 6`, which is equivalent to requiring the low six group/other permission bits to be zero and satisfies Rust 1.95 Clippy.

## Error handling and security

Failure to parse the account authority or hostname fails closed by leaving the current identity without an alias. Existing DACL protection, deny-ACE rejection, broad-principal rejection, trusted utility resolution, output bounds, and serialization lock remain unchanged. No external telemetry, credentials, provider calls, schema changes, or shell interpolation are introduced.

## Tests

Add Windows-only regression tests showing that a synthetic protected DACL containing `LA`, `SY`, and `BA` is accepted for a verified local RID 500 identity and rejected for a non-local or non-RID-500 identity. Run the repository-required format, Clippy, and full workspace test commands.
