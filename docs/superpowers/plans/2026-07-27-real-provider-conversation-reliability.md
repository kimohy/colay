# Real-Provider Conversation Reliability Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make read-only Codex work outside Git, honor typed provider preferences with preflight-only fallback, and persist terminal provider failures as actionable failed attempts with nonzero CLI exit status.

**Architecture:** Provider preference crosses the durable command boundary as typed optional data, and the daemon selects the actual eligible provider before it creates the attempt. Codex adds its public Git-check bypass only for capability-supported read-only exec. Provider failures are reduced to vendor-neutral diagnostics, finalized as failed or cancelled attempts, appended to the durable session, and returned as failed commands.

**Tech Stack:** Rust 2024 workspace, Tokio, Serde, Rusqlite, Clap CLI, official provider CLI adapters, `orchestrator-test-support` fake binaries.

## Global Constraints

- Tests and CI must use `orchestrator-test-support` fake binaries and must not invoke real Codex, Claude, Gemini, or Agy inference.
- Use Rust `Command` with separated executable and arguments; do not introduce shell interpolation.
- Keep provider wire types inside provider/compatibility crates; `orchestrator-domain` remains vendor-neutral and I/O-free.
- Preserve append-only audit semantics, redaction, schema versions, explicit approval gates, and worktree isolation.
- Runtime fallback is forbidden after a provider process starts; fallback is allowed only for preflight ineligibility.
- Writable Codex tasks never receive `--skip-git-repo-check`.
- Persisted diagnostic evidence is redacted and capped at 16 KiB.
- Missing usage remains unknown and raw provider quota units are never compared.
- Required final verification is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

## File Map

- `crates/codex-compat/src/capability.rs`: observe the public Codex Git-check bypass capability.
- `crates/codex-compat/src/invocation.rs`: emit the bypass for read-only exec only.
- `crates/codex-compat/tests/contracts.rs`: guard fixture and generic probe contracts.
- `crates/orchestrator-domain/src/session.rs`: carry optional provider preference in durable message and turn payloads.
- `crates/orchestrator-engine/src/conversation.rs`: bind an already-selected provider to each conversation request and classify terminal failure evidence.
- `crates/orchestrator-cli/src/app.rs`: submit typed provider preference from `run --provider`.
- `crates/orchestrator-cli/src/task_planner.rs`: expose eligible conversation providers in configured priority order.
- `crates/orchestrator-cli/src/conversation_orchestrator.rs`: execute the exact provider selected by the daemon.
- `crates/orchestrator-cli/src/daemon.rs`: activate planning services with the ordered eligible provider list.
- `crates/orchestrator-daemon/src/commands.rs`: copy provider preference into the derived durable conversation command.
- `crates/orchestrator-daemon/src/planning.rs`: supply eligible candidates to the conversation handler.
- `crates/orchestrator-daemon/src/conversation.rs`: select before attempt creation, record fallback notice, and separate successful from failed finalization.
- `crates/orchestrator-state/src/conversations.rs`: atomically finalize failed/cancelled attempts with an outcome and redacted error.
- `crates/orchestrator-state/tests/conversations.rs`: verify terminal persistence and idempotency.
- `crates/orchestrator-daemon/tests/conversation_flow.rs`: verify provider selection, fallback, failure state, message recovery, and zero writable state.
- `crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs`: verify the official CLI conversation adapter uses the selected fake provider and never runtime-fails over.
- `crates/orchestrator-cli/tests/global_plan_first.rs`: verify noninteractive CLI exit semantics and durable provider identity.
- `docs/qa/wsl-nightly-error-tracker.md`: close `WSL-014` through `WSL-016` only after source and WSL verification.

---

### Task 1: Capability-gated read-only Codex Git-check bypass

**Files:**
- Modify: `crates/codex-compat/src/capability.rs`
- Modify: `crates/codex-compat/src/invocation.rs`
- Modify: `crates/codex-compat/tests/contracts.rs`
- Test: inline unit tests in `crates/codex-compat/src/invocation.rs`

**Interfaces:**
- Consumes: successful public `codex exec --help` probe text.
- Produces: `CodexCapabilities::skip_git_repo_check: CapabilitySupport` and read-only argv containing `--skip-git-repo-check` when available.

- [ ] **Step 1: Write failing capability and invocation tests**

Add an invocation unit test that creates advertised exec/json/read-only/skip-Git capabilities and asserts the read-only argv contains the flag exactly once. Add a second test with `CodexSandbox::WorkspaceWrite` and verified workspace-write support that asserts the flag is absent. Add a probe contract assertion using an `ExecHelp` output that contains `--skip-git-repo-check`:

```rust
assert_eq!(
    report.capabilities.skip_git_repo_check,
    CapabilitySupport::Advertised
);
```

- [ ] **Step 2: Run the focused tests and confirm RED**

Run:

```text
cargo test -p codex-compat invocation::tests --lib
cargo test -p codex-compat --test contracts
```

Expected: compilation fails because `CodexCapabilities` has no `skip_git_repo_check` field, proving the new contract is not implemented.

- [ ] **Step 3: Implement the minimum capability and argv change**

Add the vendor-specific capability with an explicit default:

```rust
pub skip_git_repo_check: CapabilitySupport,
```

In `CapabilityProbe::evaluate`, derive it only from successful exec help:

```rust
capabilities.skip_git_repo_check = support_if(
    exec_help
        .as_deref()
        .is_some_and(|text| text.contains("--skip-git-repo-check")),
);
```

Include it in capability evidence. In `CodexInvocation::exec`, insert the flag before output/sandbox options only when the request is read-only and the capability is available:

```rust
if request.sandbox == CodexSandbox::ReadOnly
    && capabilities.skip_git_repo_check.is_available()
{
    args.push("--skip-git-repo-check".to_owned());
}
```

Do not change `WorkspaceWrite` behavior.

- [ ] **Step 4: Run the focused tests and confirm GREEN**

Run the same two commands. Expected: all Codex compatibility unit and contract tests pass with no real provider process.

- [ ] **Step 5: Commit the isolated Codex fix**

```text
git add crates/codex-compat/src/capability.rs crates/codex-compat/src/invocation.rs crates/codex-compat/tests/contracts.rs
git commit -m "fix: allow read-only Codex outside Git"
```

---

### Task 2: Typed provider preference and preflight-only fallback

**Files:**
- Modify: `crates/orchestrator-domain/src/session.rs`
- Modify: `crates/orchestrator-domain/tests/conversation_contract.rs`
- Modify: `crates/orchestrator-engine/src/conversation.rs`
- Modify: `crates/orchestrator-engine/tests/conversation_collector.rs`
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/task_planner.rs`
- Modify: `crates/orchestrator-cli/src/conversation_orchestrator.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: `crates/orchestrator-daemon/src/commands.rs`
- Modify: `crates/orchestrator-daemon/src/planning.rs`
- Modify: `crates/orchestrator-daemon/src/conversation.rs`
- Test: `crates/orchestrator-daemon/tests/conversation_flow.rs`
- Test: `crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs`

**Interfaces:**
- Produces: `AppendMessageCommandPayload::requested_provider: Option<ProviderId>` and `RequestConversationTurnCommandPayload::requested_provider: Option<ProviderId>` with `#[serde(default)]`.
- Produces: `ConversationRequest::provider: ProviderId` as the actual provider selected before attempt creation.
- Produces: `OfficialCliTaskPlanner::conversation_providers() -> Vec<ProviderId>` ordered by descending configured priority with deterministic `ProviderId` tie breaking.
- Produces: daemon-local `ConversationProviderSelection { requested, selected, used_fallback }`.
- Consumes: the existing evidenced capability map and configured provider priorities.

- [ ] **Step 1: Write failing payload compatibility tests**

Add domain tests asserting a historical JSON object without `requested_provider` deserializes to `None`, and a new payload round-trips `Some(ProviderId::Claude)` for both append and turn payloads:

```rust
let legacy: AppendMessageCommandPayload = serde_json::from_value(json!({
    "message_id": MessageId::new(),
    "content": "inspect"
}))?;
assert_eq!(legacy.requested_provider, None);
```

- [ ] **Step 2: Run the domain test and confirm RED**

Run:

```text
cargo test -p orchestrator-domain --test conversation_contract
```

Expected: compilation fails because the typed fields do not exist.

- [ ] **Step 3: Add backward-compatible typed payload fields**

Add `#[serde(default)] pub requested_provider: Option<ProviderId>` to both payload structs. Update every existing struct literal in domain, daemon, CLI, and tests to use `None`, except `run_conversation`, which uses the CLI `requested_provider`. Change `conversation_command` to copy the field rather than parse message content.

- [ ] **Step 4: Run the domain test and confirm GREEN**

Run the domain command again. Expected: new and legacy payload tests pass.

- [ ] **Step 5: Write failing provider-selection tests**

Add daemon unit/integration coverage for a candidate list `[Codex, Claude, Gemini]`:

```rust
assert_eq!(
    select_conversation_provider(Some(ProviderId::Claude), &candidates)?.selected,
    ProviderId::Claude
);
assert_eq!(
    select_conversation_provider(Some(ProviderId::Agy), &candidates)?.selected,
    ProviderId::Codex
);
```

The eligible-request test must assert `conversation_attempts.provider_id = 'claude'`. The ineligible-request test must assert `provider_id = 'codex'` and the response starts with a deterministic fallback notice. Extend the fake-provider conversation test so the selected request reaches the Claude adapter even though Codex has a higher configured priority.

- [ ] **Step 6: Run focused selection tests and confirm RED**

Run:

```text
cargo test -p orchestrator-daemon --test conversation_flow requested_provider
cargo test -p colay --test chat_conversation_fake_provider requested_provider --features test-fixtures
```

Expected: assertions show Codex is selected unconditionally or the new selection API is missing.

- [ ] **Step 7: Implement ordered candidates and exact execution**

Add `OfficialCliTaskPlanner::conversation_providers()` using the same capability/config filtering as `primary_provider()` and sorting descending by `(priority, provider)`. Add the ordered list to `PlanningServices` in both daemon activation constructors and test fixtures.

Add the selected provider to `ConversationRequest`. Change `request_conversation_turn` to parse the requested provider, select from `services.conversation_providers` before `begin_conversation_attempt`, and persist the selected provider. Change `OfficialCliConversationOrchestrator::converse` from:

```rust
let provider = self.planner.primary_provider();
```

to:

```rust
let provider = request.provider;
if !self.planner.capabilities.contains_key(&provider) {
    return Err(invocation_failure(format!(
        "selected conversation provider {provider} is not eligible"
    )));
}
```

When fallback is used, prepend this deterministic notice to the provider response before persistence:

```text
Requested provider <requested> is unavailable; using <selected> for this read-only turn.
```

Do not add a retry loop around `adapter.start`, `next_event`, or `wait`.

- [ ] **Step 8: Run focused selection tests and confirm GREEN**

Run the Task 2 focused commands plus:

```text
cargo test -p orchestrator-engine --test conversation_collector
```

Expected: typed payload compatibility, selected provider identity, and fallback notice tests pass.

- [ ] **Step 9: Commit provider selection**

```text
git add crates/orchestrator-domain crates/orchestrator-engine crates/orchestrator-cli/src/app.rs crates/orchestrator-cli/src/task_planner.rs crates/orchestrator-cli/src/conversation_orchestrator.rs crates/orchestrator-cli/src/daemon.rs crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs crates/orchestrator-daemon/src crates/orchestrator-daemon/tests/conversation_flow.rs
git commit -m "fix: honor conversation provider preference"
```

---

### Task 3: Failed attempt persistence and actionable command failure

**Files:**
- Modify: `crates/orchestrator-engine/src/conversation.rs`
- Modify: `crates/orchestrator-engine/src/lib.rs`
- Modify: `crates/orchestrator-engine/tests/conversation_collector.rs`
- Modify: `crates/orchestrator-state/src/conversations.rs`
- Modify: `crates/orchestrator-state/src/migrations.rs`
- Modify: `crates/orchestrator-state/tests/conversations.rs`
- Modify: `crates/orchestrator-state/tests/migration_contract.rs`
- Modify: `crates/orchestrator-daemon/src/conversation.rs`
- Modify: `crates/orchestrator-daemon/tests/conversation_flow.rs`
- Modify: `crates/orchestrator-cli/tests/global_plan_first.rs`
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Add: `migrations/0016_conversation_failure_outcomes.sql`

**Interfaces:**
- Produces: `ConversationFailureKind` with `Authentication`, `QuotaOrBilling`, `UnsupportedClientOrAccount`, `Timeout`, `Cancelled`, `Compatibility`, and `ProcessFailure`.
- Produces: `ConversationFailureDiagnostic { kind, response_redacted, evidence_redacted }` and `diagnose_conversation_failure(provider, failure)`.
- Produces: `WorkspaceDatabase::finalize_conversation_failure(attempt_id, status, outcome, error_redacted, completed_at)` accepting only `Failed` or `Cancelled`.
- Consumes: existing `ConversationFailure`, bounded evidence, redactor, and the v16 `conversation_attempts` terminal-outcome constraint.

- [ ] **Step 1: Write failing diagnostic classification tests**

Create table-driven engine tests for redacted evidence containing `token_expired`, `Credit balance is too low`, `UNSUPPORTED_CLIENT`, no-supported-transport compatibility text, timeout, cancellation, and an unknown exit. Assert the vendor-neutral kind and actionable response. Also assert the stored evidence is no longer than `CONVERSATION_MAX_EVIDENCE_BYTES`.

- [ ] **Step 2: Run collector tests and confirm RED**

Run:

```text
cargo test -p orchestrator-engine --test conversation_collector
```

Expected: compilation fails because failure diagnostics do not exist.

- [ ] **Step 3: Implement bounded vendor-neutral diagnostics**

Add the kind and diagnostic types to `orchestrator-engine`, export them from `lib.rs`, and implement one deterministic classifier. It must inspect only the failure variant plus already-redacted reason/evidence. Match case-insensitive operational tokens and prefer structured timeout/cancel/quota variants before fallback string matching. Recovery copy names the selected provider but never includes raw credentials.

- [ ] **Step 4: Run collector tests and confirm GREEN**

Run the collector test again. Expected: every category and bound assertion passes.

- [ ] **Step 5: Write failing state finalization tests**

Add tests that begin an attempt, finalize it with a `NeedsAttention` outcome and status `Failed`, then assert:

```rust
assert_eq!(attempt.status, ConversationAttemptStatus::Failed);
assert_eq!(attempt.outcome, Some(needs_attention));
assert_eq!(attempt.error_redacted.as_deref(), Some("authentication failed"));
```

Repeat for `Cancelled`. Assert idempotent replay returns the identical row, conflicting evidence fails closed, blank errors are rejected, and `Succeeded`/`Running` are rejected as failure targets.

- [ ] **Step 6: Run state tests and confirm RED**

Run:

```text
cargo test -p orchestrator-state --test conversations
```

Expected: compilation fails because `finalize_conversation_failure` does not exist.

- [ ] **Step 6a: Add the approved forward migration for terminal outcomes**

Implementation correction approved on 2026-07-27: the v15 CHECK constraint
requires failed and cancelled attempts to have a null `outcome_json`, so the
specified recovery outcome cannot be persisted without a forward schema
change. Add migration 0016; do not edit migrations 0010 or 0013. Rebuild
`conversation_attempts` with its workspace-partitioned keys, foreign keys,
index, and append-only triggers intact. Backfill deterministic
`needs_attention` outcomes for existing failed and cancelled rows, and test a
v15-to-v16 upgrade for row preservation, valid and invalid terminal
combinations, integrity, and foreign-key enforcement.

- [ ] **Step 7: Implement atomic failed/cancelled finalization**

Validate the outcome and nonblank bounded error, accept only `Failed` or `Cancelled`, and update `status`, `outcome_json`, `error_redacted`, and `completed_at` in one immediate transaction guarded by `status = 'running'`. Preserve exact idempotency for an already identical terminal row. Use the approved v16 constraint above and do not change `finish_conversation_attempt` success behavior.

- [ ] **Step 8: Run state tests and confirm GREEN**

Run the state command again. Expected: all conversation persistence tests pass.

- [ ] **Step 9: Write failing daemon and CLI failure tests**

Use fake provider fixtures for authentication, billing/quota, unsupported client, timeout, cancellation, malformed output, and nonzero exit. For each applicable case assert:

- attempt status is `failed` or `cancelled`, never `succeeded`;
- `error_redacted` is present and bounded;
- a `needs_attention` recovery message is appended;
- the durable request-conversation command is failed;
- tasks, task attempts, worktrees, coordinator leases, and worker leases remain zero; and
- only one fake provider start marker exists.

Add a CLI subprocess test in `global_plan_first.rs` that runs a terminal fake provider failure and asserts a nonzero process exit plus the actionable redacted command outcome.

- [ ] **Step 10: Run daemon/CLI tests and confirm RED**

Run:

```text
cargo test -p orchestrator-daemon --test conversation_flow provider_failure
cargo test -p colay --test global_plan_first provider_failure --features test-fixtures -- --nocapture
```

Expected: attempt status is currently `succeeded` and/or the CLI process exits zero.

- [ ] **Step 11: Implement terminal failure command flow**

In `request_conversation_turn`, branch on collection result. A valid parsed outcome uses `finish_conversation_attempt`. A failure is diagnosed, redacted, converted to `NeedsAttention`, and passed to `finalize_conversation_failure`; then reconcile the response and return a provider failure error so outer command processing records failure. If a stored attempt is already failed/cancelled, reconcile its outcome and return the stored error without invoking the provider. Preserve success replay behavior.

Map `ConversationFailureKind::Cancelled` to attempt status `Cancelled`; map every other kind to `Failed`. Because `wait_for_client_command` already rejects a failed durable command, the noninteractive CLI exits nonzero without a separate process-exit special case.

- [ ] **Step 12: Run focused daemon/CLI tests and confirm GREEN**

Run the Task 3 engine, state, daemon, and CLI commands. Expected: all pass, command failure is actionable, and fake provider start count remains one.

- [ ] **Step 13: Commit failure semantics**

```text
git add crates/orchestrator-engine crates/orchestrator-state/src/conversations.rs crates/orchestrator-state/tests/conversations.rs crates/orchestrator-daemon/src/conversation.rs crates/orchestrator-daemon/tests/conversation_flow.rs crates/orchestrator-cli/tests/global_plan_first.rs
git commit -m "fix: persist conversation provider failures"
```

---

### Task 4: Cross-component regression, documentation closure, and WSL QA

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Modify only if test assertions require contract updates: `fixtures/codex/versions/*/exec-help.txt`
- Verify: complete Rust workspace and WSL isolated QA state

**Interfaces:**
- Consumes: all Task 1 through Task 3 behavior.
- Produces: verified issue status and reproducible WSL evidence without adding real inference to tests or CI.

- [ ] **Step 1: Run formatting and fix only formatter output**

Run:

```text
cargo fmt --all
cargo fmt --all -- --check
```

Expected: the check exits zero.

- [ ] **Step 2: Run focused regression packages**

Run:

```text
cargo test -p codex-compat
cargo test -p orchestrator-domain
cargo test -p orchestrator-engine
cargo test -p orchestrator-state --test conversations
cargo test -p orchestrator-daemon --test conversation_flow
cargo test -p colay --test chat_conversation_fake_provider --features test-fixtures
cargo test -p colay --test global_plan_first --features test-fixtures -- --nocapture
```

Expected: every command exits zero and no test starts a real provider.

- [ ] **Step 3: Run required lint and full suite**

Run:

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: both commands exit zero with zero failed tests.

- [ ] **Step 4: Build a Linux artifact and perform isolated WSL QA**

Build/package using the repository's existing nightly workflow or local Linux-native build path. In a new Linux-native cache directory set an absolute `COLAY_HOME`, initialize/migrate the user-global state, and use only fake provider executables to verify:

- non-Git read-only Codex argv includes `--skip-git-repo-check`;
- requested eligible provider equals attempt provider and fake marker;
- ineligible requested provider falls back before process start and shows notice;
- terminal failure produces nonzero CLI exit, failed/cancelled attempt, actionable redacted message, and one provider start;
- no writable task state is created; and
- `colay daemon stop` releases the isolated daemon.

The manual QA harness may invoke a real provider only if the user separately authorizes it and local account state is valid; it is not required to close the code defects.

- [ ] **Step 5: Update the error tracker with exact evidence**

Change `WSL-014`, `WSL-015`, and `WSL-016` to `fixed` only when their focused tests, full required verification, and WSL isolated QA all pass. Record the commit/nightly identity, exact commands, provider selection evidence, terminal status evidence, CLI exit code, and daemon cleanup result. If any gate fails, leave the corresponding issue open and record the observed failure.

- [ ] **Step 6: Run documentation diff checks**

Run:

```text
git diff --check
git status --short
```

Expected: no whitespace errors and only intended implementation/documentation changes.

- [ ] **Step 7: Commit verified documentation**

```text
git add docs/qa/wsl-nightly-error-tracker.md
git commit -m "docs: close real-provider conversation issues"
```

- [ ] **Step 8: Review the complete branch before handoff**

Run:

```text
git log --oneline origin/main..HEAD
git diff --stat origin/main...HEAD
git status --short
```

Expected: focused commits for design, Codex invocation, provider selection, failure semantics, and verified QA documentation; the worktree is clean.
