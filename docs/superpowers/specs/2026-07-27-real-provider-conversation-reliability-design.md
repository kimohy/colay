# Real-Provider Conversation Reliability Design

Date: 2026-07-27
Status: Approved for implementation

## Context

Manual WSL QA of nightly `0.1.1-nightly.20260726.46acc8d` reached the official
Codex, Claude, and Gemini CLIs and exposed three product defects:

1. read-only Codex conversations outside Git fail before inference because the
   generated `codex exec` invocation omits the public
   `--skip-git-repo-check` option;
2. `colay run --plan-only --provider <provider>` records the requested provider
   in the user message but the conversation orchestrator independently selects
   the highest-priority provider; and
3. provider authentication, billing, unsupported-client, timeout, and process
   failures are converted to a generic `needs_attention` outcome and persisted
   as a successful attempt while the CLI exits zero.

The existing conversation-first contract permits read-only conversation before
Git readiness, requires terminal provider errors to be finalized with bounded
diagnostics, and keeps all tests and CI on fake provider binaries.

## Goals

- Permit capability-gated read-only Codex conversations outside Git without
  weakening writable task Git preflight.
- Carry provider preference as typed control data from the CLI through the
  durable command boundary.
- Use the requested provider when it is eligible, with deterministic preflight
  fallback to configured priority when it is not.
- Never retry another provider after an official CLI process has started.
- Persist terminal provider failures as failures with bounded redacted evidence.
- Make noninteractive conversation commands exit nonzero on terminal provider
  failure while preserving an actionable TUI message and the durable session.

## Non-Goals

- No real provider inference in automated tests or CI.
- No credential inspection, refresh, login automation, unofficial endpoint,
  usage-page scraping, quota comparison, or telemetry.
- No automatic runtime failover after authentication, billing, quota, timeout,
  cancellation, malformed output, or process failure.
- No changes to writable task Git readiness, worktree isolation, approval,
  integration, or merge policy.
- No persisted schema migration; the existing attempt status, outcome, and
  redacted error columns are sufficient.

## Selected Architecture

### Typed provider preference

`AppendMessageCommandPayload` gains an optional `requested_provider` field with
a serde default so historical commands remain readable. The derived
`RequestConversationTurnCommandPayload` carries the same optional value. The
CLI `run --provider` path sets it directly instead of relying on JSON embedded
inside the user-visible message. TUI messages use `None` until the existing TUI
provider-selection control is implemented separately.

The daemon receives an ordered list of evidenced, configured, read-only
conversation providers from the activated planner services. Before beginning a
conversation attempt it selects:

1. the requested provider when it is present in the eligible list; otherwise
2. the first provider in configured priority order.

The selected provider is written to the attempt before process start and is
placed in `ConversationRequest`. `OfficialCliConversationOrchestrator` must use
that exact provider and must not call `primary_provider()` internally. If a
requested provider was unavailable and fallback was used, the stored outcome
and displayed response receive a deterministic bounded notice naming the
requested and selected provider. No second provider is selected after process
start.

This keeps durable attempt identity, emitted provider identity, and spawned
binary identity consistent without parsing or trusting user message content.

### Capability-gated Codex Git-check bypass

The Codex public capability probe records whether `codex exec --help`
advertises `--skip-git-repo-check`. For `CodexSandbox::ReadOnly`, the exec
invocation adds the option only when that capability is available. Adding the
flag for all read-only invocations is intentional: Colay's read-only
conversation contract permits both Git and non-Git workspaces, and the option
does not authorize writes. `WorkspaceWrite` invocations never receive it and
remain protected by Colay's committed-repository preflight.

Older Codex versions that do not advertise the option retain their existing
behavior and receive a compatibility diagnostic rather than an invented flag.

### Terminal failure classification

Provider output remains parsed inside compatibility/provider crates. The
conversation boundary reduces terminal evidence to these vendor-neutral
categories:

- `authentication`;
- `quota_or_billing`;
- `unsupported_client_or_account`;
- `timeout`;
- `cancelled`;
- `compatibility`; and
- `process_failure`.

Classification uses structured lifecycle events where available and bounded,
already-redacted stderr evidence as a fallback. Exact provider wire types and
raw payloads do not enter `orchestrator-domain` or persistent state.

Every category has deterministic recovery copy. Examples include signing in
again for authentication, checking account quota or billing, updating the CLI
or account tier for unsupported clients, running `colay doctor providers` for
compatibility, and inspecting the redacted attempt evidence for an unknown
process failure. Recovery copy must not include tokens, credentials, or
unbounded provider output.

## Persistence and Command Semantics

A valid parsed `ConversationOutcome`, including a provider-authored valid
`needs_attention`, finishes the attempt as `succeeded`.

Implementation correction approved on 2026-07-27: schema v15 required
`outcome_json IS NULL` for failed and cancelled attempts, which contradicts the
durable recovery-outcome contract below. Forward migration v16 rebuilds only
`conversation_attempts` so every terminal status carries an outcome, preserves
the workspace-partitioned composite identity and append-only triggers, and
backfills a deterministic `needs_attention` outcome for existing failed or
cancelled rows. Applied migrations 0010 and 0013 remain unchanged so their
checksums and existing user databases stay valid.

A provider invocation or collection failure follows this order:

1. construct a bounded redacted diagnostic and deterministic user recovery
   message;
2. atomically finalize the running attempt as `failed` or `cancelled`, storing
   both `error_redacted` and a `needs_attention` outcome;
3. append the recovery response to the durable session;
4. return a command error so the durable conversation command becomes failed;
5. make `colay run --plan-only` return exit code 1 when it waits for that failed
   command.

The TUI continues to show the appended recovery response and keeps the session
available. No task, task attempt, worktree, coordinator lease, or worker lease
is created.

Idempotent replay loads the already terminal attempt and reconciles its stored
outcome without starting another provider process. A replay of a failed attempt
must remain failed rather than being rewritten as success.

## Data Flow

```text
run --provider claude
  -> AppendMessage(requested_provider=claude)
  -> RequestConversationTurn(requested_provider=claude)
  -> select from ordered evidenced providers
     -> claude when eligible
     -> highest-priority eligible fallback when not eligible
  -> begin attempt(provider=actual selection)
  -> ConversationRequest(provider=actual selection, read-only)
  -> official CLI process (one provider only)
     -> valid outcome: attempt succeeded, command completed
     -> terminal failure: attempt failed, recovery message appended,
        command failed, CLI exit 1
```

For Codex read-only exec:

```text
capability probe observes --skip-git-repo-check
  -> codex exec --skip-git-repo-check --json --sandbox read-only ...
```

Writable Codex execution does not use the bypass.

## Error Handling and Redaction

- Persisted evidence is redacted before storage and capped at the existing
  conversation evidence limit of 16 KiB.
- Authentication messages may name the provider and recommend its official
  login command but never inspect or print auth files.
- Quota and billing remain provider-local classifications; Colay does not
  compare raw quota units.
- Unsupported-client errors recommend official CLI/account remediation without
  linking unofficial endpoints.
- Unknown exit code, malformed output, and lifecycle errors use
  `process_failure` with the provider name, exit state, and bounded evidence.
- Cancellation uses attempt status `cancelled`; all other terminal failures use
  `failed`.
- Fallback is allowed only before process start. A started process consumes the
  single attempt authority even when it fails immediately.

## Testing Strategy

All automated tests use `orchestrator-test-support` fake binaries.

### Codex compatibility tests

- A read-only invocation includes `--skip-git-repo-check` when the public help
  probe advertises it.
- A read-only invocation omits it when not advertised.
- A workspace-write invocation omits it even when advertised.

### Provider-selection tests

- A requested eligible provider is selected even when another provider has a
  higher configured priority.
- A requested disabled, missing, or incompatible provider falls back to the
  highest-priority eligible provider and records a fallback notice.
- The attempt provider, response provider, and fake binary marker agree.
- No runtime provider failure starts a second fake binary.
- Historical append/conversation payloads without `requested_provider` still
  deserialize and select the primary provider.

### Failure persistence tests

- Authentication, quota/billing, unsupported client/account, timeout,
  cancellation, malformed output, and nonzero exit fixtures produce bounded
  actionable diagnostics.
- Failed invocations persist `failed` or `cancelled`, retain a
  `needs_attention` outcome, and never persist `succeeded`.
- The durable conversation command is failed and noninteractive CLI execution
  exits nonzero.
- The TUI/session projection contains the recovery message.
- No task, worktree, task attempt, coordinator lease, or worker lease is
  created.
- Replay does not invoke the provider again or change the terminal status.

### Required verification

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

After automated verification, install the resulting Linux nightly in an
isolated WSL `COLAY_HOME`. Public `--version`, `--help`, compatibility probes,
non-Git read-only argv, provider selection, fallback, failure persistence, CLI
exit code, and daemon cleanup are verified. Real inference is attempted only
with explicit user authorization and valid local provider account state.

## Acceptance Criteria

1. Capability-supported read-only Codex conversation works in a non-Git
   workspace without changing writable Git preflight.
2. An eligible `--provider` choice is the provider persisted and executed.
3. An ineligible requested provider falls back before process start to the
   highest-priority eligible provider and tells the user which provider ran.
4. A provider failure after process start never invokes another provider.
5. Terminal invocation failures persist as `failed` or `cancelled`, with
   bounded redacted evidence and an actionable session response.
6. Noninteractive `run --plan-only` exits nonzero for a terminal provider
   failure while the TUI session remains recoverable.
7. Provider failure and fallback paths create no writable task state.
8. Historical durable command payloads remain readable.
9. Automated tests invoke only fake provider binaries.
10. Required formatting, lint, and workspace test commands pass.
