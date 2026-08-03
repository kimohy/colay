# WSL-Native Provider and Read-Only Conversation Tools Design

Date: 2026-08-03
Status: Approved for implementation

## Context

Clean-install WSL QA of nightly `0.1.1-nightly.20260803.28d0d5f`
validated package provenance, schema migration, daemon lifecycle, SQLite
integrity, and public compatibility probes. The same run exposed two remaining
provider usability defects.

First, a bounded real Codex `run --plan-only` conversation completed with the
canonical answer `CLEAN_OK`, but Colay persisted the attempt as failed because
the provider first ran a read-only command to inspect a required local skill
file. The worker remained inside Colay's read-only sandbox and produced no file
change, task, worktree, or lease. Colay nevertheless treats every
`CommandStarted` event as a lifecycle violation. This is `WSL-022`.

Second, WSL had no Linux-native `agy` executable while a Windows `agy.exe` was
visible below `/mnt/c`. Provider discovery must not silently cross the WSL/Windows
runtime boundary. The same rule must apply to Codex, Claude, Gemini, and Agy so
that behavior does not depend on the selected provider. This is `WSL-023`.

The defects share a boundary theme: provider integrations need a uniform,
capability-based policy while Colay continues to own the sandbox, persistence,
approval, and native-runtime boundaries. Automated tests and CI continue to use
only `orchestrator-test-support` fake binaries.

## Goals

- Permit provider-initiated read-only tool execution during a plan-only
  conversation when the selected provider advertises or has verified read-only
  sandbox capability.
- Preserve a bounded, redacted audit record of read-only command events without
  turning them into user instructions or writable task state.
- Continue to fail closed on file changes, write-capable sandbox requests,
  unsupported read-only capability, lifecycle failures, nonzero provider exit,
  or any task/worktree materialization.
- Apply identical conversation safety semantics to Codex, Claude, Gemini, and
  Agy.
- Require Linux-native provider executables under WSL and reject Windows PE
  executables and `/mnt/<drive>` Windows-path candidates for every provider.
- Add actionable, provider-neutral WSL remediation when no native executable is
  available.
- Record `WSL-022` and `WSL-023` in the WSL nightly QA tracker and close them
  only after release-candidate and published-nightly verification.

## Non-Goals

- No command-name or executable allowlist for plan-only conversations.
- No permission to write merely because a provider describes a command as
  harmless.
- No weakening of task approval, Git preflight, isolated worktrees, scheduler
  leases, append-only audit, redaction, or state-schema guarantees.
- No automatic fallback from a WSL-native provider to a Windows `.exe`.
- No automatic provider failover and no provider-specific exception for Codex
  or Agy.
- No real provider invocation from unit tests, integration tests, or CI.
- No credential inspection, identity rotation, quota bypass, usage scraping,
  unofficial endpoints, or external telemetry.
- No SQLite schema migration. The design uses existing conversation evidence
  and compatibility structures.

## Selected Architecture

### Capability-gated read-only command evidence

`run --plan-only` continues to construct provider work with
`SandboxMode::ReadOnly`. The conversation orchestrator evaluates provider
events against both that requested sandbox and the selected provider's
read-only capability assessment.

When the sandbox is read-only and the capability is `Advertised` or `Verified`,
a `CommandStarted` event is accepted as evidence rather than treated as an
error. The evidence records only bounded, redacted command metadata already
available from the normalized provider event. It passes through the same
normalization, deduplication, line limit, and 16 KiB aggregate conversation
evidence bound used for other diagnostic evidence. Raw provider output does not
become a shell command, remediation instruction, or subsequent Colay action.

Capability states that do not establish read-only support remain fail-closed.
An unexpected command in those states produces a deterministic compatibility
failure explaining that the selected provider has not established the required
read-only boundary. This avoids relying on provider identity, command spelling,
or prompt wording as a security control.

`FileChanged` remains a terminal lifecycle violation in every plan-only
conversation, including after accepted read-only command events. A write-capable
sandbox request, writable tool event, nonzero provider exit, protocol lifecycle
error, task creation, worktree creation, or lease creation also remains a
failure. Colay does not infer that an arbitrary command is safe; it relies on
the enforced read-only sandbox plus the provider's compatible capability.

The outcome flow is therefore:

```text
plan-only request
  -> request ReadOnly sandbox
  -> require Advertised or Verified read-only capability
  -> CommandStarted: retain bounded/redacted evidence
  -> FileChanged or lifecycle/write violation: fail closed
  -> valid terminal answer and exit 0: persist successful conversation
  -> never materialize task/worktree before explicit approved task transition
```

### Provider-neutral policy boundary

The orchestrator consumes vendor-neutral lifecycle events and capability states.
Provider wire event decoding stays in provider/compatibility crates. No provider
name is used to grant a broader permission. Codex, Claude, Gemini, and Agy fake
providers must all pass the same matrix:

- read-only capability plus a command event can complete successfully;
- command evidence is bounded and redacted;
- a file-change event fails;
- missing read-only capability plus a command event fails; and
- no path creates a task, task attempt, worktree, coordinator lease, or worker
  lease.

The provider request may continue to instruct the provider not to modify files
or use write-capable tools. It must not promise that all command execution is
forbidden, because read-only inspection is a legitimate implementation detail.
Prompt text remains defense in depth, not the authorization mechanism.

### WSL-native executable resolution

Provider executable discovery classifies the host before accepting a resolved
candidate. Native Windows, Linux, and macOS discovery retain their current
platform behavior. Under WSL, every selected provider must resolve to a
Linux-native executable.

A WSL candidate is rejected when either of these conditions is true:

- the resolved path is on a Windows-mounted drive such as `/mnt/c`; or
- executable inspection identifies a Windows PE executable, including a file
  reached through a shim, symlink, or unexpected extension.

Path classification is an early and readable guard; executable-format
classification is the authoritative defense against renamed or indirectly
resolved PE files. The implementation must use structured `Command` arguments
for any probe and must not add shell interpolation.

If a rejected Windows candidate is the only discovered provider executable,
the compatibility result is unavailable/degraded with a common remediation:

```text
WSL requires a Linux-native provider executable. Install the provider inside
this WSL distribution and ensure its Linux binary appears on PATH before any
Windows-mounted path.
```

Provider-specific installation hints may be appended only when already
supported by trusted Colay configuration. The core requirement and rejection
reason remain identical for all providers. Colay never launches the rejected
candidate and does not search Windows application state for credentials.

### Diagnostics and persistence

Accepted command events are auditable conversation evidence, not health or
account-readiness claims. Evidence remains local, redacted, append-only, and
bounded by the existing conversation evidence limits. Successful completion
stores the canonical outcome and evidence through existing state APIs; no new
provider wire type enters `orchestrator-domain` and no database column is added.

Rejected WSL candidates appear in compatibility and doctor diagnostics with the
resolved redacted path class, native-runtime requirement, and remediation. A
public version/help probe is executed only after the candidate passes the WSL
native check. Default diagnostics remain non-inference and account readiness
remains unverified unless an explicitly authorized live probe has succeeded.

The QA tracker receives two open entries before implementation:

- `WSL-022`: plan-only read-only command event is misclassified as writable
  command execution;
- `WSL-023`: provider discovery can expose a Windows executable to a WSL
  runtime instead of requiring a Linux-native provider.

Each entry records the clean nightly version, reproduction, durable-state
observations, selected fix, and completion criteria. It changes to `fixed` only
after the published nightly passes the acceptance flow below.

## Error and Safety Semantics

- `CommandStarted` is non-terminal only when Colay requested a read-only sandbox
  and provider read-only capability is `Advertised` or `Verified`.
- `FileChanged` is always terminal in plan-only mode, even if all earlier events
  were accepted.
- Capability absence or contradiction fails closed and directs the user to
  `colay doctor providers` or `colay compatibility`.
- A Windows provider candidate under WSL is never executed, including for
  `--version` or `--help`.
- A missing WSL-native candidate never falls back to Windows and never changes
  the selected provider silently.
- Diagnostic paths do not invoke inference unless the user explicitly
  authorizes a bounded real-provider QA call.
- Evidence redaction and size limits apply before persistence and presentation.
- Plan-only completion or failure creates zero writable task state and leaves no
  active daemon, socket, or lease after the QA harness cleans up its own runtime.

## Testing Strategy

All automated provider processes use fake binaries from
`orchestrator-test-support`.

### Conversation event matrix

For Codex, Claude, Gemini, and Agy:

- emit a read-only capability and `CommandStarted`, then a valid terminal answer
  and exit zero; assert success and exact requested provider identity;
- assert command evidence is normalized, redacted, deterministically bounded,
  and does not alter the canonical answer;
- emit duplicate and oversized command evidence; assert deduplication and UTF-8
  safe truncation within the existing aggregate limit;
- emit `FileChanged` before or after a command; assert terminal failure;
- omit or contradict read-only capability and emit a command; assert fail-closed
  compatibility behavior;
- exit nonzero or emit a lifecycle error; assert existing failure
  classification; and
- assert zero tasks, task attempts, worktrees, coordinator leases, and worker
  leases for every success and failure case.

### Executable resolution matrix

- Under simulated WSL, accept a Linux-native executable for each provider.
- Under simulated WSL, reject `.exe`, `/mnt/c` candidates, renamed PE files,
  symlinked PE files, and shims that ultimately resolve to PE.
- Assert rejected candidates are not invoked even for public probes.
- Assert the common native-WSL remediation names the selected provider without
  suggesting Windows fallback.
- Verify ordinary Linux, macOS, and Windows native discovery behavior is
  unchanged.
- Keep fixtures deterministic and free of real provider processes or account
  state.

### Repository and release verification

Run the required Windows workspace gates:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Run full Clippy under WSL and the relevant fake-provider integration suite on
both Windows and WSL. The pull request must pass all configured Windows, Ubuntu,
and macOS checks before merge.

After the merge produces a new nightly, install it into a new timestamped npm
prefix and `COLAY_HOME` inside WSL. Confirm package version, registry integrity,
Linux ELF identity, schema migration, doctor/compatibility output, SQLite
integrity, and daemon start/status/stop cleanup. Verify all four provider public
probes with Linux-native binaries where installed and verify that a Windows
candidate is rejected without execution.

For each provider whose account readiness is established by the operator, run
at most one explicitly authorized bounded plan-only turn. Assert requested
provider identity, terminal outcome, bounded/redacted command evidence, zero
writable state, and replay behavior. An unavailable or account-blocked provider
is recorded as such and is not retried or replaced by another provider. Real
credentials and raw account identifiers are never printed or persisted in QA
artifacts.

## Acceptance Criteria

1. `WSL-022` and `WSL-023` are present in the QA tracker with reproducible
   evidence before being marked fixed.
2. All four providers share one capability-gated plan-only command policy.
3. A read-only command event can coexist with a successful canonical answer
   when read-only capability is advertised or verified.
4. Command evidence is redacted, deterministic, and bounded by existing
   conversation limits.
5. File changes, write-capable requests, missing read-only capability, lifecycle
   errors, and nonzero exits remain fail-closed.
6. Plan-only success and failure create no task, task attempt, worktree, or
   lease.
7. WSL accepts Linux-native provider executables and rejects Windows PE and
   Windows-mounted provider candidates without launching them.
8. Missing native providers receive a common actionable WSL remediation; Colay
   never falls back across the runtime boundary.
9. Native provider resolution outside WSL remains compatible.
10. Fake-provider automated tests, required Windows workspace checks, WSL full
    Clippy, pull-request CI, and isolated published-nightly WSL QA pass.

## Delivery Boundary

Design, implementation, tests, and verification occur in the isolated
`codex/wsl-provider-readonly-policy` worktree. The design document is committed
and reviewed before an implementation plan is written. Production changes then
follow test-driven development. The worktree is retained in accordance with
repository policy; merge and deployment occur only after review and successful
CI under explicit maintainer authorization.
