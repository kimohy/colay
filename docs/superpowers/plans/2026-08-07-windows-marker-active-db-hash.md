# Windows marker active-database hash implementation plan

> Execute in the existing `codex/fix-workspace-register-latency` linked worktree. Do not run a real
> provider. Do not retry marker or stress automatically.

**Goal:** Keep live SQLite health validation while deferring raw global database-family hashes until
the daemon is verifiably stopped.

**Architecture:** Change only the marker diagnostic's database-health wrapper and its two call
sites. Preserve the reviewed stress-harness primitives and all product code. Record the exact failed
run and the new reviewed one-shot result in the QA tracker.

---

### Task 1: Add explicit active and post-stop health phases

**Files:**
- Modify: `artifacts/qa/windows-state-acl/marker-attribution-ab-diagnostic.ps1`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

1. Record the focused RED proof that `Get-FileHash` fails when a read/write handle is live even when
   that handle shares reads and writes.
2. Build an isolated PowerShell test around the extracted `Get-AbDatabaseHealthEvidence` function.
   Initially require the two explicit phases and prove the active phase makes zero family-hash calls;
   confirm the test fails before implementation.
3. Add a mandatory `ValidateSet('ActiveDaemon', 'PostStopStable')` phase parameter and an explicit
   post-stop-quiescence input. Preserve exact integrity/foreign-key checks. Return an explicit hash
   scope and null hashes for `ActiveDaemon`; refuse `PostStopStable` before hashing unless its gate
   is true; call `Get-SqliteFamilyHashes` exactly once for a confirmed stable phase.
4. Pass `ActiveDaemon` at the pre-cleanup call. Derive the post-stop gate from a successful daemon
   stop document, a signaled retained daemon handle with zero errors, a stopped endpoint, and zero
   live leases; then pass `PostStopStable`. Do not substitute a sleep or endpoint status alone.
5. Re-run the focused test, parser check, exact-callsite AST check, and existing static-contract
   verification. Do not execute marker, stress, product, or provider in this task.
6. Update `WIN-008` to `fix-in-progress` with the design, focused RED/GREEN evidence, exact amended
   marker hash, and the remaining one-shot closure gate. Do not close `WIN-007` or `WIN-008`.
7. Commit the marker and tracker as `fix: defer live SQLite family hashing`.
8. Obtain an independent security/correctness review; fix all Critical, Important, and Minor
   findings before continuing.

### Task 2: Run the amended A/B exactly once

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Evidence: `artifacts/qa/windows-state-acl/marker-attribution-ab-*.json`

1. Require an exact clean source HEAD and zero exact candidate-process residue.
2. Rebuild `colay` and the fake provider with `CARGO_BUILD_JOBS=1`, `CARGO_INCREMENTAL=0`, and
   `--features test-fixtures`; record their SHA-256 values with the reviewed script hashes.
3. Invoke the reviewed marker through verified portable PowerShell 7.6.4 exactly once, with separated
   arguments and all expected hashes. Never retry automatically.
4. Validate the JSON independently: success, eight observations, four pairs, zero retries, ten exact
   input-hash checkpoints, fake-only execution, zero credential keys, zero cleanup errors/residual
   processes, active health with intentionally omitted raw hashes, stable post-stop family hashes,
   and an explicit retain/split decision.
5. Update the tracker. Close `WIN-007` and `WIN-008` only if every gate passes; otherwise retain the
   exact failure and stop before authoritative stress.
6. Commit the tracker result as `docs: record active-database hash verification` and obtain an
   independent task review.

### Task 3: Follow the A/B decision and finish Windows acceptance

1. If the decision is `retain-attributed-markers-in-latency-phase`, use the reviewed stress harness
   unchanged. If it is `split-latency-marker-off-and-correctness-marker-on-phases`, implement and
   review that split before stress.
2. On a newly built exact clean HEAD, run authoritative Windows stress once with
   `-ExpectedSourceCommit` and all exact SHA-256 inputs.
3. Validate five sequential registrations at p95 <= 5000 ms, four concurrent registrations each
   <= 8000 ms, the 10000 ms response timeout, SQLite integrity/foreign keys, zero writable rows and
   leases, exact marker cardinality, and complete process cleanup.
4. Record evidence and review it before branch-wide Rust gates, PR, CI, merge, nightly, WSL clean
   install, and bounded read-only provider QA.
