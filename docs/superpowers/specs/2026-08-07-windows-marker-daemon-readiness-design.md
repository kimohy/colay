# Windows marker daemon-readiness design

## Problem

The reviewed one-shot marker A/B completed six observations, then an exit-zero `colay --json
daemon start` returned an exact daemon instance in `booting`. The marker required `online`
immediately and failed before opening its retained process handle or measuring registration.

This is a readiness-contract mismatch, tracked as `WIN-009`, not registration latency. The current
daemon starts its IPC server before it transitions `booting -> probing -> online`, and the current
lifecycle tests poll status for up to five seconds after start. The failed arm observed the booting
record roughly 21.6 ms after the instance was created. Its registration command never ran.

An older startup-recovery design says a successful start should return only `online`; the global
daemon implementation and its current tests now expose asynchronous readiness. This drift must be
recorded, but changing user-facing daemon supervision semantics is broader than the diagnostic
failure and is not required to obtain an unbiased registration measurement.

## Requirements

- Accept only the progress states `booting` and `probing` before `online`; reject stopped, failed,
  stale, unknown, malformed, or state/phase-inconsistent documents.
- Anchor the exact instance ID, integer PID, and canonical expected executable path from the original
  schema-v1 `daemon_start` document.
- Poll only `colay --json daemon status`, with separated arguments, a monotonic five-second overall
  deadline, bounded individual command timeouts, and a fixed bounded poll interval.
- Require every schema-v1 `daemon_status` progress document to identify the same instance ID, PID,
  and executable path as the start document.
- Perform no CIM query, native process-handle open, direct process mutation, workspace registration,
  or provider call inside the readiness loop.
- Open the existing retained native handle exactly once, and only after an identity-preserving
  document reports exact state and phase `online`.
- Keep readiness time and poll evidence separate from `registration.elapsed_ms`; the A/B decision
  continues to use only OS-process-lifetime registration measurements.
- Preserve all marker hash, fake-only, credential, cleanup, SQLite, and cardinality gates.
- Do not alter the product or stress harness in this correction.

## Selected design

Add a strict document-identity parser and a bounded readiness helper to the marker.

1. Parse the initial `daemon_start` document, requiring schema `1`, command `daemon_start`, an exact
   progress/online state whose instance phase matches it, a valid UUID, a safe integer PID, and the
   reviewed `colay.exe` path.
2. If it is already `online`, return it with zero poll commands.
3. Otherwise poll exact `daemon_status` documents. Record each command through the existing bounded
   process runner. Permit only `booting` or `probing` while waiting, require the anchored identity on
   every response, and stop only at exact `online`.
4. Return the original state, final state, poll count, elapsed time, bounds, anchored identity, and
   the exact online document as readiness evidence.
5. Pass the online document to `Open-AbDaemonIdentity`. That function accepts only a schema-v1
   `daemon_start` or `daemon_status` identity document, still requires exact `online` before its CIM
   query, and otherwise retains its existing path/creation-time/native-handle verification.
6. Store readiness evidence before retained-handle evidence. Begin the measured registration only
   after both readiness and identity capture succeed.

The five-second deadline follows current daemon lifecycle coverage. Each status subprocess receives
only the remaining bounded budget (with a small fixed floor for cleanup), so one hung status command
cannot silently turn the poll into a 30-second wait.

## Failure and cleanup behavior

Identity drift, terminal state, malformed JSON, a nonzero status command, or timeout fails before CIM
and registration. The existing `finally` path issues a bounded daemon stop and checks stopped status,
leases, exact process residue, source immutability, and fake-only configuration. Because no retained
handle was opened, stable raw database hashing remains correctly refused unless the normal cleanup
gate can prove quiescence.

After an online document is accepted, any later retained-handle capture failure keeps the existing
close/error evidence. Once a handle is retained, all existing identity-bound cleanup rules apply.

## Rejected alternatives

- **Treat booting as online.** This would allow CIM and registration before readiness and weaken the
  reviewed identity boundary.
- **Poll without identity comparison.** A restarted or replaced daemon could be mistaken for the
  instance returned by the original start command.
- **Open a handle while booting.** The reviewed marker contract requires online before CIM/native
  inspection; changing it is unnecessary.
- **Sleep once and assume readiness.** Scheduling time is nondeterministic and provides no state or
  identity evidence.
- **Change `daemon start` product semantics here.** That requires broader incumbent/spawned-child
  supervision and timeout policy work. The marker can follow the current tested contract without
  altering user-facing behavior.

## Verification and closure

Focused PowerShell 7.6.4 tests must cover immediate online with zero polls; booting/probing/online on
one identity; every identity drift dimension; state/phase mismatch; terminal state; malformed JSON;
and deadline/individual-command bounds. Static checks must prove readiness precedes handle capture,
handle capture precedes registration, no CIM occurs in the poll, and the existing static contract
still passes.

After independent review of the exact marker hash, rebuild exact clean HEAD binaries and invoke the
marker once. `WIN-007`, `WIN-008`, and `WIN-009` may close only with eight observations, four pairs,
ten exact input checkpoints, zero retries/credentials/cleanup errors/residual processes, complete
active/post-stop database evidence, bounded readiness evidence, and an explicit retain/split
decision. Authoritative stress remains gated on that decision.
