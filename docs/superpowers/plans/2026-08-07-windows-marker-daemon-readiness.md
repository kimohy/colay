# Windows marker daemon-readiness implementation plan

> Execute in the existing `codex/fix-workspace-register-latency` linked worktree. Do not run a real
> provider. Never retry marker or stress automatically.

**Goal:** Make the marker follow the current asynchronous daemon-start contract without weakening
its online-before-CIM identity boundary or contaminating registration timing.

**Architecture:** Add a pure strict daemon-document identity parser and a bounded status-poll helper
inside the marker. Preserve product code and the reviewed stress harness. Capture a retained native
handle only after exact online readiness for the anchored daemon instance.

---

### Task 1: Implement identity-preserving bounded readiness

**Files:**
- Modify: `artifacts/qa/windows-state-acl/marker-attribution-ab-diagnostic.ps1`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

1. Build focused extracted-function RED tests for immediate online, delayed online, identity drift,
   terminal/malformed status, state/phase mismatch, and timeout behavior. Record the current failure
   on booting input.
2. Add strict parsing for exact schema/command/state/phase/UUID/integer-PID/executable identity.
3. Add a monotonic five-second readiness loop using only exact `daemon status` commands, bounded
   per-command timeouts, and a fixed bounded interval. Require one anchored identity throughout.
4. Return explicit readiness evidence and the exact online document. Update `Open-AbDaemonIdentity`
   only so it accepts the online start/status command variants; preserve its exact-online check
   before CIM and all existing CIM/native path/creation/handle checks.
5. Insert readiness before handle capture and handle capture before registration. Add the readiness
   evidence field to every observation.
6. Run focused GREEN tests, parser checks, exact AST ordering/no-CIM-in-loop checks, all existing
   marker static-contract fixtures, and exact hash calculations. Do not run full marker, stress,
   product, Cargo, or provider commands.
7. Update `WIN-009` to `fix-in-progress`, record the older-design/current-contract drift, exact
   focused evidence, amended marker hash, and one-shot closure gate. Do not close `WIN-007`–`009`.
8. Force-add the excluded marker, stage only marker+tracker, and commit as
   `fix: wait for exact daemon readiness`.
9. Obtain an independent security/correctness review and fix every Critical, Important, and Minor
   finding before proceeding.

### Task 2: Run the readiness-amended A/B exactly once

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Evidence: `artifacts/qa/windows-state-acl/marker-attribution-ab-*.json`

1. Require exact clean HEAD and zero exact candidate-process residue.
2. Rebuild `colay` and the fake provider with process-scoped single-job/non-incremental settings;
   hash binaries, reviewed marker, unchanged stress harness, and migration inputs.
3. Invoke the marker through verified portable PowerShell 7.6.4 exactly once with separated
   arguments and every expected hash. Do not retry.
4. Validate success, eight observations, four pairs, zero retries, ten exact checkpoints, fake-only
   execution, zero credential keys, bounded identity-preserving readiness evidence, online-before-CIM
   identity evidence, complete active/stable database evidence, zero cleanup errors/residual
   processes, and an explicit retain/split decision.
5. Close `WIN-007`–`009` only if every gate passes. Otherwise record the exact failure and stop before
   authoritative stress.
6. Commit only the tracker result as `docs: record daemon-readiness verification` and obtain an
   independent review.

### Task 3: Follow the A/B decision and complete Windows acceptance

1. For `retain-attributed-markers-in-latency-phase`, run the reviewed stress harness unchanged. For
   `split-latency-marker-off-and-correctness-marker-on-phases`, implement and review the split first.
2. Rebuild an exact clean HEAD and run authoritative stress once with `-ExpectedSourceCommit` and all
   reviewed hashes.
3. Validate five sequential p95 <= 5000 ms, four concurrent registrations each <= 8000 ms, the
   10000 ms response timeout, SQLite integrity/foreign keys, zero writable rows/leases, exact marker
   cardinality, and complete cleanup.
4. Record/review evidence, then run branch-wide Rust gates and continue through PR, CI, merge,
   nightly, WSL clean install, and bounded read-only provider QA.
