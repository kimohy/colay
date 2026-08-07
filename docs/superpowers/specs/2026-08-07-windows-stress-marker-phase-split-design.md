# Windows stress marker phase-split design

## Problem

The reviewed exact-HEAD marker A/B passed with eight observations, four counterbalanced pairs, zero
retries, and complete cleanup evidence, but attributed filesystem markers materially distorted the
registration measurement. Three of four registration pairs exceeded their pair limits, the median
attributed delta was 1370.5 ms against a 183 ms limit, and the order bias was 812 ms against the same
limit. Its required decision was
`split-latency-marker-off-and-correctness-marker-on-phases`.

The authoritative Windows stress harness currently enables both the aggregate marker file and the
attributed marker directory for the five serial and four concurrent registrations that determine
latency acceptance. It also asserts attributed groups inside those timed cases. This mixes
filesystem-observer cost into the product response-time measurement.

The harness already has a second, fresh process-audit runtime. That phase is explicitly excluded
from latency thresholds and verifies one legacy import against its durable opaque source hash. It is
the correct place to retain attributed-marker correctness checks.

The generated audit child also assumes an exit-zero `daemon start` response is immediately
`online`. The reviewed A/B observed a real `booting -> online` transition, so that assumption can
make the correctness phase fail nondeterministically before it exercises the marker contract.

## Requirements

- Keep the aggregate inspection marker enabled in both phases.
- Remove `COLAY_TEST_LEGACY_INSPECT_MARKER_DIR` entirely from the latency environment; an empty
  value is not disabled because the product reads it with `var_os`.
- Prove the latency phase starts with zero markers, produces exactly 18 aggregate events for nine
  imported sources, and produces zero attributed groups or events.
- Preserve the five serial and four concurrent OS-process-lifetime measurements, durable-state,
  publication, source immutability, SQLite health, zero-writable-row, process ownership, and cleanup
  gates.
- Enable the attributed marker only in the separate correctness/process-audit environment.
- Prove that correctness phase produces exactly two aggregate events and one attributed group with
  exactly two distinct empty event files, and that the group equals the durable `source_root_hash`.
- Keep the correctness/process-audit timing explicitly excluded from latency acceptance.
- Make environment construction require one exact marker phase mode so callers cannot silently
  inherit the wrong policy.
- In the generated audit child, accept only `booting` or `probing` before exact `online`; preserve
  one schema-v1 daemon instance ID, safe integer PID, and exact executable path throughout a
  monotonic five-second status poll.
- Use separated executable/argument invocation only. Do not run a real provider.
- Record the phase policy and phase-specific marker evidence without conflating aggregate events
  with attributed events.

## Selected design

Add a mandatory `MarkerPhase` parameter to `New-IsolatedEnvironment` with exactly two values:
`LatencyAttributedOff` and `CorrectnessAttributedOn`. Both set the aggregate marker. Only the
correctness mode adds the attributed-directory environment key.

The main runtime uses `LatencyAttributedOff`. It creates an empty marker directory solely as a
sentinel that any unexpected attributed write would make visible. It checks:

1. zero aggregate and attributed markers after empty-incumbent startup;
2. cumulative aggregate counts 2, 4, 6, 8, and 10 after the serial registrations while the
   attributed directory remains empty;
3. final aggregate count 18 after the four concurrent registrations while the attributed directory
   remains empty.

Latency evidence records the policy, exact key absence, aggregate count, zero attributed
cardinality, and `timing_included_in_latency_thresholds = true`. Durable source evidence names the
opaque value `source_root_hash`; it does not claim that a latency-phase marker group exists.

The process-audit runtime uses `CorrectnessAttributedOn`. Existing strict filesystem validation and
durable-hash comparison remain, and aggregate count becomes an exact assertion of two. Correctness
evidence records exact key presence, aggregate and attributed cardinality, durable-hash equality,
and `timing_included_in_latency_thresholds = false`.

Because the evidence meaning changes, the stress summary schema advances from 1 to 2. A top-level
`marker_phase_policy` fixes the A/B decision that authorized the split. The old ambiguous aggregate
and attributed summary fields are replaced by explicit `latency_phase` and `correctness_phase`
objects. `inspection_count` remains as a compatibility scalar for the latency aggregate count and
is therefore exactly 18.

Inside the generated process-audit child, add a strict daemon document parser and a bounded
readiness helper. Anchor identity from `daemon_start`, return immediately if already online, or poll
only `colay --json daemon status` until the same instance reports exact state/phase `online`. Reject
identity drift, terminal/unknown state, malformed documents, and timeout before registration.

## Failure and cleanup behavior

Any attributed marker in the latency sentinel directory, wrong aggregate count, missing or malformed
correctness marker, identity drift, readiness timeout, or phase-policy mismatch fails the harness.
The existing outer and audit-child cleanup paths still stop the exact daemon, wait for released
leases, terminate only owned exact-generation residue, and record every cleanup error.

The phase split changes observation overhead only. It does not relax response deadlines, database
checks, source immutability, process audit, or cleanup acceptance.

## Verification

Before implementation, a focused PowerShell 7.6.4 test must fail because the current environment
always includes the attributed key and the current main path requires attributed groups. The GREEN
matrix must cover exact key absence/presence, aggregate 18/2 cardinality, zero attributed latency
groups, strict correctness group/event shape, audit readiness immediate/delayed online, all identity
drift dimensions, terminal state, and timeout. Static checks must prove main=off, audit=on, and audit
timing remains excluded.

After focused tests, independent review, and a clean commit, rebuild the exact binaries and invoke
the authoritative stress exactly once with its exact 40-hex HEAD. Never retry blindly. Only its
complete JSON may close `WIN-005`; LocalSystem `WIN-006` and deployed WSL/nightly issues remain
separate.
