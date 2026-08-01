# Provider Readiness and Diagnostic Hygiene Design

Date: 2026-07-31
Status: Approved for implementation

## Context

Manual WSL QA of the public nightly and the local real-provider conversation
candidate found two remaining usability defects:

1. `compatibility`, `doctor providers`, and the provider portion of `doctor`
   report a provider as `healthy` after safe public `--version` and `--help`
   probes even though those probes do not establish that the current account can
   complete a request; and
2. a correctly classified terminal provider failure can still overwhelm the
   actionable recovery message with repeated unknown-event notices, provider
   implementation stacks, and unsafe provider-authored permission-bypass advice.

The first defect is a vocabulary problem, not a request to make normal
diagnostics consume quota. The second is a presentation and evidence-hygiene
problem, not a request to discard all forensic evidence. Automated tests and CI
must continue to use only fake provider binaries.

## Goals

- Preserve the existing serialized `health.status` contract while distinguishing
  public binary compatibility from account/runtime readiness.
- Keep default provider diagnostics non-inference and report that account
  readiness is unverified when no safe readiness signal exists.
- Show a concise, provider-specific recovery message on normal CLI and TUI
  failure paths.
- Persist useful redacted failure evidence after deterministic normalization,
  duplicate removal, line bounding, and byte bounding.
- Prevent provider-authored advice from weakening Colay's sandbox, approval, or
  permission boundaries.
- Close WSL-017 and WSL-018 with fake-provider regression tests plus isolated
  Windows and WSL user-flow verification.

## Non-Goals

- No implicit live inference from `compatibility`, `doctor`, or `providers`.
- No authentication-file inspection, account-identifier display, credential
  extraction, login automation, entitlement scraping, unofficial endpoint, or
  external telemetry.
- No claim that version/help compatibility proves billing, quota, authentication,
  entitlement, model availability, or provider service availability.
- No provider failover after a process starts.
- No weakening of read-only sandbox enforcement, writable Git preflight,
  approval gates, or worktree isolation.
- No persisted state-schema migration. Account readiness is a diagnostic report
  property, while normalized failure evidence uses the existing conversation
  attempt fields.

## Selected Architecture

### Report-level account readiness

`ProviderReport` gains an additive `account_readiness` object without changing
`ProviderHealth` or its persistence contract. The report contains:

- `status`: `ready`, `blocked`, or `unverified`;
- `detail`: a bounded, non-sensitive explanation; and
- `checked_at`: the timestamp of the readiness assessment.

The initial implementation emits `unverified` for all results based only on
public version/help/capability probes. A successful binary probe therefore keeps
the existing `health.status = healthy` value for backward compatibility while
the adjacent readiness object states that account readiness was not checked.
Failed binary probes remain unhealthy or degraded as they are today; they do not
invent an account failure and also report account readiness as unverified.

`doctor` maps a compatible binary with unverified readiness to a warning rather
than a pass and uses the deterministic detail:

```text
compatible; account readiness unverified (safe public probes do not make an inference request)
```

The provider report remains available in the check data, including executable
resolution and capabilities. `inference_requests` remains zero. No provider's
login or account-status command is added until its public contract can be shown
to be non-inference, non-mutating, bounded, and free of account identifiers and
credentials. An explicit live readiness command may be designed separately; it
is not hidden inside this change.

This approach deliberately keeps readiness out of `orchestrator-domain` and the
global provider-health table. A transient CLI diagnostic must not silently
redefine routing health or require a database migration.

### Two-stage diagnostic normalization

Failure evidence is processed before persistence and separately from the normal
user-facing error.

#### Stage 1: provider/runtime evidence collection

The official CLI conversation orchestrator uses a bounded evidence accumulator
instead of appending arbitrary strings directly. It:

1. normalizes line endings and drops blank lines;
2. coalesces repeated unknown non-lifecycle events by event type and records one
   bounded summary with an occurrence count;
3. retains a lifecycle-affecting unknown event as a terminal failure while still
   deduplicating its evidence;
4. removes exact duplicate diagnostic lines while preserving first-seen order;
5. limits each line and the total number of retained lines;
6. summarizes omitted lines; and
7. applies the existing final 16 KiB conversation evidence limit.

The selected limits are 64 retained lines, 2 KiB per line, and 16 KiB total.
The byte bound is authoritative; truncation preserves valid UTF-8 and appends a
deterministic truncation marker.

Provider-specific normalization stays in `orchestrator-providers`, where raw
provider vocabulary already belongs. It may collapse implementation stack
frames and rewrite unsafe permission-bypass suggestions, but it must not emit
provider wire types into `orchestrator-domain` or persisted state. A permission
bypass suggestion is replaced with neutral evidence such as:

```text
provider requested an unsafe permission bypass; Colay did not enable it
```

Colay never adds the suggested bypass flag or tells the user to do so.

#### Stage 2: classification, persistence, and presentation

The engine classifies the normalized redacted evidence into the existing
vendor-neutral failure kinds. It retains the normalized detail in
`ConversationFailureDiagnostic.evidence_redacted`, bounded to 16 KiB.

The durable failed/cancelled attempt stores:

- the existing deterministic `needs_attention` recovery outcome;
- a concise `error_redacted` containing only the actionable classified response;
  and
- the normalized detailed evidence in the outcome's existing redacted evidence
  location.

The command error returned to noninteractive callers uses the concise recovery
response and does not append the forensic evidence. The TUI shows the same
recovery response. Detailed evidence remains available through explicit
diagnostic/state inspection, subject to existing redaction and local-state
access controls.

## Compatibility and Schema Contract

- Existing `health.status` values and `ProviderHealth` serialized fields do not
  change.
- `account_readiness` is additive to `ProviderReport`. Current provider-report
  schema envelopes remain readable by consumers that tolerate additive fields.
- No SQLite migration or schema-version increment is required.
- `doctor` and `doctor providers` continue to report zero inference requests.
- Historical conversation attempts remain readable. This change only affects
  normalization and presentation of newly captured failures.
- Routing continues to use provider health and capabilities; unverified account
  readiness does not disable a provider or trigger automatic failover.

## Error and Safety Semantics

- Account readiness is never promoted to `ready` solely from version/help
  output.
- Absence of a safe account probe is `unverified`, not `blocked`.
- Binary incompatibility does not masquerade as an account failure.
- A real request failure can be classified as authentication, quota/billing,
  unsupported client/account, timeout, cancellation, compatibility, or process
  failure without persisting credentials or unbounded stderr.
- Unknown event summaries are evidence, not user instructions.
- Provider stacks may be retained only within the selected line and byte bounds
  after duplicate removal and redaction.
- Unsafe permission-bypass advice is not echoed verbatim in normal output or
  persisted evidence.
- Failure handling still creates no writable task, worktree, task attempt,
  coordinator lease, or worker lease.

## Testing Strategy

All automated provider processes use `orchestrator-test-support` fake binaries.

### Provider readiness tests

- Successful version/help compatibility keeps the existing healthy status and
  adds `account_readiness.status = unverified`.
- Missing, failing, or capability-incomplete binaries do not claim account
  readiness.
- The provider check is a warning when readiness is unverified and uses the
  deterministic explanation.
- `compatibility`, `doctor providers`, and `doctor` retain
  `inference_requests = 0`.
- JSON snapshots/assertions cover the additive field without changing existing
  health values.

### Diagnostic normalization tests

- Repeated unknown stderr event types produce one occurrence-count summary.
- Duplicate and blank stderr lines are removed while first-seen order is stable.
- Long lines, excess lines, and excess bytes receive deterministic truncation.
- JavaScript-style provider stacks cannot displace the classified recovery
  message and remain within the evidence bounds.
- Unsafe permission-bypass suggestions are replaced with safe Colay guidance;
  the raw bypass flag is absent from response, error, and persisted evidence.
- Authentication, quota/billing, unsupported account/client, timeout,
  cancellation, compatibility, and generic process failures keep their existing
  classification.
- Normal CLI errors contain the concise recovery response but not detailed
  evidence; persisted failure outcomes retain the normalized detail.
- Replay remains idempotent and does not invoke the provider again.

### Cross-platform and release verification

Run the required repository checks:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Then verify the release candidate on Windows and in WSL using isolated Colay
state. WSL verification installs the clean nightly into a timestamped npm
prefix, checks provider reports and doctor output, and performs only explicitly
authorized bounded real-provider calls. It verifies requested provider identity,
concise failure output, normalized persisted evidence, zero writable task state,
SQLite integrity, and daemon/process/socket cleanup. No credential value is
printed or copied.

## Acceptance Criteria

1. Existing `health.status` serialization remains unchanged.
2. Safe public probes report account readiness as unverified rather than imply
   that the current account can run a request.
3. Default compatibility and doctor paths make zero inference requests.
4. Normal terminal failure output is concise and actionable.
5. Repeated unknown events and duplicate stderr lines are deterministically
   collapsed.
6. Detailed redacted evidence is preserved within 64 lines, 2 KiB per line, and
   16 KiB total.
7. Unsafe provider-authored permission-bypass advice is replaced with safe Colay
   guidance and never acted upon.
8. Failure classification, persistence status, nonzero CLI exit, and replay
   semantics remain correct.
9. No writable task state or lease is created by readiness checks or read-only
   conversation failures.
10. Fake-provider automated tests, Windows verification, required workspace
    checks, and isolated WSL release-candidate QA pass.

## Delivery Boundary

Implementation and verification occur on the existing isolated feature
worktree. The branch will be committed and prepared with PR-ready evidence.
Repository policy prohibits this worker from pushing, auto-merging, or deleting
the worktree; those operations require an authorized maintainer after review.
