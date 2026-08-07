# Windows Marker Empty-Preflight Collection Design

**Status:** approved for implementation on 2026-08-06 under the standing instruction to proceed without further approval prompts.

## Problem

The first authoritative invocation of `marker-attribution-ab-diagnostic.ps1` exited before creating
an evidence file or starting any A/B observation:

```text
The property 'Count' cannot be found on this object.
```

The diagnostic runs with `Set-StrictMode -Version Latest`. When
`Get-AbExactCandidateProcesses` finds no exact `colay.exe` or
`colay-e2e-fake-provider.exe` process, PowerShell emits no pipeline object and the unwrapped caller
assignment stores `$null`. Accessing `$preexisting.Count` then fails under strict mode instead of
accepting the required zero-residue state.

An isolated preflight reproduction using the exact committed function and binaries confirmed
`is_null=True` followed by the same `Count` exception. The actual A/B invocation produced zero
observations and no evidence JSON, so it is a harness preflight defect rather than product or
provider evidence.

## Options

1. **Normalize the preflight call result at the caller (selected).** Assign
   `@(Get-AbExactCandidateProcesses ...)` before checking `.Count`. This is the existing pattern at
   both cleanup call sites and preserves the helper's pipeline semantics.
2. Force the helper to emit a non-enumerated array. This risks nested arrays at the two existing
   callers that already normalize with `@(...)` and broadens a one-call-site defect.
3. Relax strict mode or replace the check with truthiness. This hides cardinality mistakes and
   weakens a security-sensitive diagnostic.

## Change

Change only the main preflight assignment in
`artifacts/qa/windows-state-acl/marker-attribution-ab-diagnostic.ps1`:

```powershell
$preexisting = @(
    Get-AbExactCandidateProcesses @($script:ResolvedColay, $script:ResolvedFake)
)
```

The subsequent exact-residue failure remains unchanged. No product source, provider behavior,
timeout, measurement threshold, cleanup policy, hash checkpoint, or process-identity rule changes.

## Verification

- RED: execute only the committed function-definition prefix and the exact empty-residue preflight
  expression under portable PowerShell 7.6.4; confirm `$preexisting` is `$null` and `.Count` throws.
- GREEN: repeat the same isolated preflight with caller-side `@(...)`; require an `Object[]`, count
  zero, and exit code zero.
- Parse both PowerShell QA scripts with zero parser errors.
- Re-run the marker's existing static-contract review on the exact amended hash.
- Commit the marker and tracker changes, rebuild exact-HEAD binaries, then perform one deliberate
  A/B rerun. Record the first invocation as a pre-observation failure; do not treat it as an A/B
  sample or silently discard it.
- Only after a successful A/B decision may the authoritative Windows stress harness run once.

## QA Status

Track this as `WIN-007` with status `fix-in-progress` until the amended marker completes all eight
fake-provider-only observations, four counterbalanced pairs, exact hash checkpoints, and cleanup
requirements. The run never authorizes real provider inference.
