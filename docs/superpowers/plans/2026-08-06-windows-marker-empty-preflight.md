# Windows Marker Empty-Preflight Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the Windows marker A/B diagnostic accept the required zero-candidate-process preflight under strict mode and preserve explicit failure for actual residue.

**Architecture:** Normalize only the main preflight function output with PowerShell's array subexpression operator. Preserve the existing helper's pipeline behavior because its two cleanup callers already normalize the result, then verify the fix with an isolated real-function reproduction before one deliberate fake-only A/B rerun.

**Tech Stack:** PowerShell 7.6.4, PowerShell AST parser, Git, existing Rust `colay` and `colay-e2e-fake-provider` test-fixtures binaries.

## Global Constraints

- Keep `Set-StrictMode -Version Latest`; do not weaken cardinality or residue checks.
- Change no product source, provider behavior, response timeout, measurement threshold, cleanup policy, hash checkpoint, or identity rule.
- Automated and diagnostic execution is fake-provider-only; never invoke real provider inference.
- The failed first invocation had zero observations and no evidence JSON; preserve it as a pre-observation harness defect.
- Do not rerun A/B until the source fix is reviewed, committed, the worktree is clean, and exact-HEAD binaries and hashes are rebuilt.
- Do not run authoritative Windows stress unless the amended A/B completes all eight observations and four counterbalanced pairs.
- Preserve `WIN-005`, `WIN-006`, `WSL-022`, and `WSL-023` as open/pending.

---

### Task 1: Normalize the empty preflight collection and record the defect

**Files:**
- Modify: `artifacts/qa/windows-state-acl/marker-attribution-ab-diagnostic.ps1:1955`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Test: isolated PowerShell 7.6.4 function-prefix reproduction

**Interfaces:**
- Consumes: pipeline output from `Get-AbExactCandidateProcesses([string[]] $ExecutablePaths)`.
- Produces: `$preexisting` as `Object[]` with exact cardinality zero, one, or greater before the unchanged residue check.

- [ ] **Step 1: Preserve the RED evidence**

Use the committed marker function-definition prefix under the verified portable PowerShell 7.6.4.
Call `Get-AbExactCandidateProcesses` for the exact rebuilt `colay.exe` and fake-provider paths with
no candidates running, assign it without `@(...)`, and access `.Count` under strict mode.

Expected: `is_null=True`, followed by exit 1 and `The property 'Count' cannot be found on this object.`
No product command or A/B observation runs.

- [ ] **Step 2: Implement the minimal caller-side normalization**

Replace only the assignment before the preexisting-residue check:

```powershell
$preexisting = @(
    Get-AbExactCandidateProcesses @($script:ResolvedColay, $script:ResolvedFake)
)
```

Do not change `Get-AbExactCandidateProcesses` or the two cleanup callers that already use this
pattern.

- [ ] **Step 3: Verify GREEN without running A/B**

Repeat the isolated function-prefix command using the amended assignment.

Expected: `is_null=False`, type `System.Object[]`, `count=0`, exit 0.

Parse both scripts:

```powershell
$paths = @(
  'scripts/qa/windows-state-acl-stress.ps1',
  'artifacts/qa/windows-state-acl/marker-attribution-ab-diagnostic.ps1'
)
foreach ($path in $paths) {
  $tokens = $null
  $errors = $null
  [void][System.Management.Automation.Language.Parser]::ParseFile(
    (Resolve-Path -LiteralPath $path), [ref]$tokens, [ref]$errors
  )
  if ($errors.Count -ne 0) { throw ($errors.Message -join '; ') }
}
```

Expected: zero parser errors for both scripts.

- [ ] **Step 4: Add `WIN-007` to the QA tracker**

Record severity `medium` and status `fix-in-progress`. Include the first invocation's exact binary,
stress, and marker hashes; exit 1; zero observations; absence of evidence JSON; isolated RED; root
cause; selected fix; and the requirement for reviewed exact-HEAD A/B before closure. Do not alter
other issue states or historical evidence.

- [ ] **Step 5: Review and commit**

Request an independent PowerShell correctness/security review of the exact two-file diff. The
review must check zero/one/many cardinality, strict-mode behavior, unchanged residue failure,
cleanup callers, fake-only constraints, and tracker accuracy.

After a Ready verdict:

```powershell
git add -- artifacts/qa/windows-state-acl/marker-attribution-ab-diagnostic.ps1 docs/qa/wsl-nightly-error-tracker.md
git diff --cached --check
git commit -m "fix: normalize empty marker preflight results"
```

---

### Task 2: Rebuild exact HEAD and verify the amended marker once

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Evidence: `artifacts/qa/windows-state-acl/marker-attribution-ab-*.json`

**Interfaces:**
- Consumes: a clean exact source HEAD, reviewed marker/stress hashes, and freshly built test-fixture binaries.
- Produces: eight fake-only observations, four counterbalanced pairs, ten exact input hash checkpoints, cleanup evidence, and one retain/split decision.

- [ ] **Step 1: Require a clean exact source tree**

```powershell
$head = git rev-parse HEAD
$status = git status --porcelain=v1 --untracked-files=all --ignore-submodules=none
if ($status) { throw 'worktree is dirty' }
```

- [ ] **Step 2: Rebuild and hash exact-HEAD binaries**

```powershell
$env:CARGO_BUILD_JOBS = '1'
$env:CARGO_INCREMENTAL = '0'
cargo build -p colay --bins --features test-fixtures
Get-FileHash -Algorithm SHA256 target/debug/colay.exe
Get-FileHash -Algorithm SHA256 target/debug/colay-e2e-fake-provider.exe
```

Expected: build exit 0 and two fresh hashes recorded with the exact HEAD.

- [ ] **Step 3: Run the amended marker exactly once**

Invoke the verified portable PowerShell with separated arguments for the exact binaries, stress
harness, evidence root, and all four expected SHA-256 values. Do not retry automatically.

Expected: exit 0; eight observations; four pairs; zero retries; ten exact hash checkpoints; zero
credential keys; zero cleanup errors/residual candidate processes; and an explicit retain/split
decision.

- [ ] **Step 4: Validate and record the evidence**

Read the timestamped JSON and verify its own counts, hashes, status, cleanup, and decision. Add the
evidence filename and SHA-256 to `WIN-007`. Mark `WIN-007` fixed only if every requirement passed;
otherwise retain `fix-in-progress` with the exact failure and do not run authoritative stress.

- [ ] **Step 5: Commit the QA result**

```powershell
git add -- docs/qa/wsl-nightly-error-tracker.md
git diff --cached --check
git commit -m "docs: record marker empty-preflight verification"
```
