# Provider boundary and post-decode redaction design

## Problem

The deployed WSL nightly tracker still has two incomplete provider-conversation gates. `WSL-035`
has component-level byte and event limits, but the fake provider cannot select a plan-only
conversation fixture that actually crosses those limits. `WSL-036` has broad frame, parser,
collector, diagnostic, and persistence redaction coverage, but `scenario:secret` is intercepted by
the conversation fixture dispatcher and silently becomes the ordinary success fixture. The
deployed canary check is therefore vacuous.

Tracing the successful conversation path exposes a second trust-boundary gap. Provider events are
parsed and redacted before their accumulated assistant text is returned. The engine then parses
that text as a second JSON document. An inner value such as `api\u005fkey=...` is not recognizable
to the first textual redaction pass, but the second JSON decode turns it into `api_key=...`.
Successful `ConversationOutcome` response and requirement strings currently proceed directly to
conversation attempts, messages, and requirement revisions without another redaction pass.

`WSL-040` was also remeasured. A stopped-distribution invocation spent roughly 33 seconds starting
the WSL systemd user session, while warm native and npm-wrapper Colay invocations were roughly
0.16--0.20 seconds and did not start a daemon or provider. This is an external WSL host condition,
not a Colay code-path defect, and remains a separately documented environment finding.

## Requirements

- Prove the escaped-secret hypothesis with a failing user-visible CLI/daemon/SQLite test before
  changing production behavior.
- Treat deserialized provider outcomes as untrusted even when their wire bytes were previously
  redacted.
- Redact every string in every `ConversationOutcome` variant, including all requirement lists and
  separated verification executable/argument fields, before any success persistence or response.
- Revalidate the transformed outcome and fail closed through the existing conversation-failure
  path if redaction makes a structured outcome invalid.
- Preserve the vendor-neutral, I/O-free domain crate and keep provider wire types in provider and
  compatibility layers.
- Add deterministic process-backed conversation fixtures for a second-decode secret, byte-limit
  overflow, and event-limit overflow. Tests and CI must use only `orchestrator-test-support` fake
  binaries.
- Exercise Codex, Claude, Gemini, and Agy under the same policy. A provider-specific wire encoding
  may differ, but acceptance semantics must not.
- For each overflow, prove exactly one provider invocation, bounded nonzero failure, no successful
  outcome, no retry, and no task, worktree, or lease rows.
- For the secret canary, prove the fixture was selected with a non-secret positive control and scan
  stdout, stderr, SQLite primary/WAL/SHM/journal bytes, JSONL evidence, and generated artifacts for
  the decoded canary.
- Keep failure evidence bounded by the existing 16 KiB limit and preserve append-only audit,
  schema, approval, and read-only conversation contracts.
- Do not convert the external `WSL-040` cold-start observation into an artificial product timeout
  or eager daemon/provider startup.

## Considered approaches

### External deployed script only

A WSL script could construct large provider output and scan state after each run. This would close
one manual checklist, but it would not make the fake provider scenarios selectable by ordinary
`colay run --plan-only`, would leave CI unable to catch regressions, and would not repair the
post-decode trust boundary. It is insufficient.

### Typed fake scenarios plus post-decode defense

Add a small fixed set of scenario selectors to the existing fake provider and add a structural
redaction pass at the daemon persistence boundary. This exercises the real process, parser,
collector, engine, daemon, and SQLite path without allowing arbitrary test-controlled output. It
also gives deployed QA and CI the same reproducible contract. This is the selected approach.

### Generic environment-driven output generator

An environment variable or file containing arbitrary provider output would be flexible, but it
would create a broad test-only input language, weaken executable provenance reasoning, and make
positive-control and secret-handling mistakes easier. The extra generality is not justified.

## Selected design

### Post-decode outcome redaction

Keep initial process and collector redaction unchanged. Immediately after
`collect_conversation_response_with_evidence` succeeds in the daemon, transform the owned
`ConversationOutcome` with the active `MessageRedactor`:

1. redact `response_redacted` for all four outcome variants;
2. redact `NeedsAttention.evidence_redacted`;
3. for requirement-bearing variants, redact `objective`, every string in `in_scope`,
   `out_of_scope`, `constraints`, `acceptance_criteria`, `risks`, and `open_questions`;
4. redact every verification command executable and each separated argument without joining them;
5. call `ConversationOutcome::validate` again;
6. on validation failure, convert to the existing `ConversationFailure::Validation` flow using
   already-redacted, bounded evidence.

The transformation stays daemon-local because the daemon owns the redactor and persistence trust
boundary. The domain model remains vendor-neutral and I/O-free. No raw decoded outcome is written,
logged, or returned. Provider fallback notices are added only after successful post-decode
redaction and validation.

### Deterministic process-backed fixtures

Extend `orchestrator-test-support` with fixed conversation scenarios selected from the prompt:

- `scenario:decoded-secret` emits a valid structured outcome whose inner JSON contains Unicode
  escapes that become a synthetic `api_key` canary only during the engine's second decode. A
  separate harmless marker proves this exact branch ran.
- `scenario:byte-overflow` emits provider-valid framing that exceeds the configured conversation
  output/frame byte boundary by the smallest practical amount.
- `scenario:event-overflow` emits 4,097 valid incremental events while staying below the byte
  boundary.

Each provider uses its official fake wire shape. The selector maps to one typed scenario and never
accepts arbitrary output. Existing invocation markers provide the one-start/no-retry positive
control. If investigation proves internal limit cancellation is misclassified as user cancellation,
fix precedence at the original lifecycle source and retain a regression assertion for the exact
failure class.

### End-to-end acceptance

Expand the global plan-first fixture so it can configure any of the four fake providers while
retaining an isolated non-Git workspace and one user-global database. Table-driven CLI tests invoke
each provider through `colay run --plan-only` and assert literal outcomes rather than fake method
calls.

The decoded-secret test reads all durable conversation and requirement surfaces, then scans the
entire isolated state/evidence tree and SQLite sidecars as bytes. It asserts that the decoded
canary is absent and the redaction marker is present. Overflow tests assert one invocation,
nonzero bounded failure, explicit limit evidence, and zero rows in all writable task/worktree/lease
tables. Component tests cover the structural mapper and validation-failure conversion so every
outcome field is protected even when a particular provider fixture does not populate it.

## Failure and cleanup behavior

Malformed fixtures, identity mismatch, output/event overflow, post-redaction invalidity, or a
positive-control mismatch fail closed. No scenario can promote the plan-only session, allocate a
task, create a worktree, or retain a lease. Test fixtures stop only their owned daemon and rely on
temporary-directory cleanup; production worktrees are never deleted.

The byte/event tests must not retry automatically. A failure caused by the safety limit is a
bounded provider/compatibility failure, not a successful answer. Evidence remains redacted and no
larger than 16 KiB.

## Verification

The RED phase must demonstrate the decoded canary in a persisted successful outcome on the
unmodified source and demonstrate that the new process-backed scenario selectors are not yet
supported. The GREEN phase runs structural unit tests, fake runtime/parser tests, and global
plan-first tests for all four providers on Windows and Linux-compatible code paths.

Before publication, run `cargo fmt --all -- --check`,
`cargo clippy --workspace --all-targets --all-features -- -D warnings`, and
`cargo test --workspace --all-features`. After PR CI and merge, wait for the exact merged nightly,
clean-install it in WSL, run the fake matrix against the packaged binary, and run bounded read-only
real-provider checks for Codex, Claude, Gemini, and Agy. Authentication, quota, or billing failures
remain external provider results when diagnostics and state hygiene are correct.
