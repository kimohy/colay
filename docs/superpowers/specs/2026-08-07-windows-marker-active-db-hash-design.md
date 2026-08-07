# Windows marker active-database hash design

## Problem

The reviewed marker A/B reached its first arm on Windows and completed workspace registration, but
failed while collecting pre-cleanup health evidence. `Get-AbDatabaseHealthEvidence` called the
stress harness `Get-SqliteFamilyHashes`, whose generic `Get-Sha256` uses `Get-FileHash`.

While the daemon owns a read/write SQLite handle, `Get-FileHash` opens a second handle with sharing
that does not admit the existing writer. Windows therefore returns a sharing violation for the
global `state.db`. A focused PowerShell 7.6.4 reproduction confirms the same behavior with an
ordinary read/write handle. After the daemon stops, the preserved A/B evidence shows that the same
database family can be hashed and that integrity, foreign keys, leases, and writable-row cleanup
are healthy.

This is a diagnostic-harness defect, tracked as `WIN-008`; the failed evidence does not establish a
product regression.

## Requirements

- Keep the active-daemon integrity and foreign-key checks. They use SQLite's read-only interface
  and validate the database through SQLite rather than treating live files as an atomic snapshot.
- Never raw-hash the global SQLite family while the daemon is active.
- Hash the global SQLite family only after the daemon is stopped, the endpoint reports stopped,
  the live lease count is zero, and no owned process remains.
- Keep binary, script, migration, source-database, fake-provider, marker, cleanup, and credential
  gates unchanged.
- Keep `Get-Sha256` and `Get-SqliteFamilyHashes` fail-closed for immutable inputs. Do not relax
  their file-sharing behavior globally.
- Make the health phase explicit at every call site so a future caller cannot accidentally rely on
  an ambiguous default.
- Preserve the one-shot rule: the next full A/B run occurs only after the amended marker hash and
  focused verification have been independently reviewed.

## Selected design

Give `Get-AbDatabaseHealthEvidence` a mandatory validated phase with two values:

- `ActiveDaemon`: run `PRAGMA integrity_check` and `PRAGMA foreign_key_check`, report that raw family
  hashes were intentionally omitted, and do not call `Get-SqliteFamilyHashes`.
- `PostStopStable`: run the same SQLite checks and include the raw database-family hashes.

The pre-cleanup call must pass `ActiveDaemon`; the cleanup call must pass `PostStopStable`. Both
result objects keep the same health fields. They also carry an explicit hash scope, with the active
record containing a null family-hash value rather than silently implying a snapshot.

`PostStopStable` also requires explicit quiescence evidence. The caller must have a successful
`daemon stop` document, a signaled retained daemon handle with no cleanup errors, a stopped endpoint,
and zero live leases. The health helper refuses the stable phase before calling the hasher when that
gate is false. Merely receiving `state=stopped`, entering a `finally` block, or waiting for an
arbitrary delay is not proof that the database writer released its handle.

## Rejected alternatives

- **Open hashes with `FileShare.ReadWrite`.** This removes the sharing error but permits the daemon
  heartbeat or another transaction to change the database or WAL during a non-atomic multi-file
  read. Such hashes are reproducible-looking but do not identify a coherent SQLite state.
- **Retry until the file opens.** The owning daemon is deliberately live, so a retry is unbounded
  or biases the measured arm without making the snapshot coherent.
- **Stop the daemon before all health checks.** This discards useful active-daemon SQLite validation
  and conflates pre-cleanup correctness with cleanup correctness.
- **Remove global database hashes entirely.** Stable post-stop hashes remain useful evidence and
  already succeed in the preserved failed run.

## Verification and closure

Before another A/B run:

1. Reproduce the `Get-FileHash` sharing failure with a focused temporary-file fixture (RED).
2. Parse the amended marker with PowerShell 7.6.4.
3. Extract the health helper into an isolated test scope with stubbed SQLite/hash functions and
   prove that `ActiveDaemon` makes zero hash calls, rejected `PostStopStable` makes zero hash calls,
   and confirmed-quiescent `PostStopStable` makes exactly one.
4. Statically verify the two production call sites use the intended explicit phases and that the
   post-stop gate is derived from the stop document, retained-handle wait, endpoint status, and live
   lease result.
5. Re-run the existing marker static-contract checks and review the exact marker hash.

After review, rebuild exact clean HEAD binaries and invoke the marker once. `WIN-007` and `WIN-008`
may close only if the run produces eight observations, four counterbalanced pairs, ten exact input
hash checkpoints, zero retries, zero credentials, zero cleanup errors or residual processes, and an
explicit retain/split decision. Authoritative Windows stress remains gated on that decision.
