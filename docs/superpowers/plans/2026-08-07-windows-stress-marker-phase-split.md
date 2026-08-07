# Windows stress marker phase-split implementation plan

> Execute in the existing `codex/fix-workspace-register-latency` linked worktree. Use only fake
> providers. Do not consume the authoritative one-shot stress run until implementation and review
> are complete on a clean exact commit.

**Goal:** Separate attributed-marker correctness evidence from Windows registration latency while
preserving every existing database, source, process, and cleanup gate.

**Architecture:** Make marker mode explicit in isolated environments, use aggregate-only markers in
the timed main runtime, retain strict attributed markers in the threshold-excluded process-audit
runtime, and make the generated audit child follow the asynchronous daemon readiness contract.

---

### Task 1: Add focused RED contracts

**Files:**
- Add: `scripts/qa/test-windows-state-acl-marker-phases.ps1`
- Read: `scripts/qa/windows-state-acl-stress.ps1`

1. Extract or statically inspect only the necessary harness functions; do not execute the harness
   main body, product, provider, marker A/B, or stress.
2. Add RED checks proving latency environments must omit the attributed key, correctness
   environments must include its exact path, and callers must specify one exact mode.
3. Add RED fixtures for latency aggregate count 18 with zero attributed groups and correctness
   aggregate count 2 with one 64-hex group and two distinct empty events.
4. Add RED/static checks for main=off, audit=on, phase-specific evidence, and threshold exclusion.
5. Add generated-audit-child readiness fixtures for immediate online, booting/probing/online,
   identity drift, state/phase mismatch, terminal state, and five-second timeout.
6. Run with verified portable PowerShell 7.6.4 and record the intended pre-fix failures.

### Task 2: Implement the split and bounded audit readiness

**Files:**
- Modify: `scripts/qa/windows-state-acl-stress.ps1`
- Modify: `scripts/qa/test-windows-state-acl-marker-phases.ps1`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

1. Add mandatory validated `MarkerPhase` construction and omit the attributed environment key in
   `LatencyAttributedOff`.
2. Convert the main serial/concurrent phase to aggregate cardinality plus an always-empty
   attributed sentinel, preserving all timed command and durable-state checks.
3. Advance evidence schema to 2 and add exact split policy plus explicit latency/correctness marker
   evidence. Rename per-source `inspection_group_id` evidence to `source_root_hash`.
4. Keep the process-audit phase attributed-on, strengthen aggregate count to exactly two, and record
   its exact marker contract in correctness evidence.
5. Add identity-preserving bounded daemon readiness to the generated audit child before its legacy
   registration.
6. Update the tracker with the passed A/B decision and this source fix; do not close `WIN-005` before
   authoritative stress and do not change `WIN-006` or WSL deployment statuses.
7. Run the focused GREEN matrix, parser checks, existing static/process-audit helper tests, and
   `git diff --check`. Do not run authoritative stress or real providers.
8. Commit only the intended files and obtain an independent Critical/Important/Minor review. Fix all
   findings before proceeding.

### Task 3: Run authoritative Windows acceptance exactly once

1. Require an exact clean HEAD and zero exact-path candidate process residue.
2. Rebuild `colay` and `colay-e2e-fake-provider` with process-scoped
   `CARGO_BUILD_JOBS=1` and `CARGO_INCREMENTAL=0`.
3. Hash the exact binaries, harness, migration inputs, and verified portable PowerShell.
4. Invoke `windows-state-acl-stress.ps1` once with separated arguments and exact
   `-ExpectedSourceCommit <40hex HEAD>`. Never retry automatically.
5. Validate schema 2, exact source/hash identity, marker phase policy, latency 18/0 marker
   cardinality, correctness 2/1/2 marker cardinality, five serial p95 <= 5000 ms, four concurrent
   commands each <= 8000 ms, 10000 ms response timeout, database integrity/foreign keys, zero
   writable rows/leases, fake-only execution, process audit, and complete cleanup.
6. Record and independently review the JSON. Close `WIN-005` only if every gate passes.

### Task 4: Complete branch and release verification

1. Run exact-source `cargo fmt --all -- --check`, full workspace/all-target/all-feature Clippy with
   `-D warnings`, and full workspace/all-feature tests.
2. Obtain final branch review, publish a PR, resolve CI findings, and merge only when all required
   checks are green.
3. Confirm the merged nightly publication, clean-install it in WSL, and run isolated fake-provider
   QA plus bounded read-only real-provider checks under one uniform policy, including Agy.
4. Keep LocalSystem `WIN-006` open unless a native LocalSystem execution independently proves its
   ACL contract.
