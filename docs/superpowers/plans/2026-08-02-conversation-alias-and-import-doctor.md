# Conversation Alias and Import Doctor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Accept the observed read-only provider `response` compatibility alias without weakening the canonical conversation contract, report completed legacy imports accurately in offline and live-daemon doctor paths, and make Antigravity part of the four-provider QA matrix.

**Architecture:** Normalize provider-owned JSON into the vendor-neutral `ConversationOutcome` at the engine boundary, leaving domain and persisted JSON canonical. Add one read-only state query that validates an exact import ledger and its audit, mapping, and published-file evidence; reuse it from offline doctor snapshots and an optional, backward-compatible live-daemon lookup field. Keep all automated provider execution fake and reserve bounded real provider probes for deployed-nightly WSL QA.

**Tech Stack:** Rust 1.95, Serde/serde_json, rusqlite, Tokio IPC, `orchestrator-test-support` fake provider binaries, Cargo, GitHub Actions, npm nightly packaging, WSL.

## Global Constraints

- Keep `orchestrator-domain` vendor-neutral and I/O-free; do not change `ConversationOutcome` or its strict canonical serialization.
- Accept `response` only while decoding provider output. Persist and emit only `response_redacted`.
- Reject unknown fields, wrong types, missing required fields, and payloads containing both `response` and `response_redacted`.
- Do not add repair retries, provider-specific output-schema transport, identity rotation, quota bypass, scraping, credential extraction, unofficial endpoints, or telemetry.
- Preserve schema versions, append-only audit evidence, redaction, explicit approval gates, and read-only doctor behavior.
- Use Rust `Command` with separated executable and arguments; never add shell interpolation.
- Automated tests and CI use `orchestrator-test-support` fake binaries only. Never invoke real Codex, Claude, Gemini, or Antigravity inference from tests or CI.
- Keep `.task4-target/` untracked and excluded from commits. Do not delete any worktree.
- Required final verification is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

### Task 1: Normalize the provider response alias at the engine boundary

**Files:**
- Modify and test: `crates/orchestrator-engine/src/conversation.rs`

**Interfaces:**
- Add private `ProviderConversationOutcome`, used only by `collect_conversation_response`.
- Preserve the public return type `Result<orchestrator_domain::ConversationOutcome, ConversationFailure>`.

- [ ] **Step 1: Add failing boundary regression tests**

In the existing `#[cfg(test)]` module in `conversation.rs`, construct successful read-only `ConversationResponse` values and assert all four aliases normalize to their canonical variants:

```rust
let output = serde_json::json!({
    "outcome": "answer_complete",
    "response": "Hello! How can I help?"
});
let outcome = collect_conversation_response(
    &request,
    successful_response(&request, serde_json::to_vec(&output)?),
)?;
assert_eq!(
    outcome,
    ConversationOutcome::AnswerComplete {
        response_redacted: "Hello! How can I help?".to_owned(),
    }
);
assert_eq!(
    serde_json::to_value(outcome)?,
    serde_json::json!({
        "outcome": "answer_complete",
        "response_redacted": "Hello! How can I help?"
    })
);
```

Repeat the input assertion for `more_information_needed`, `worktree_task_candidate`, and `needs_attention`, retaining every non-response field required by the matching domain variant.

Add fail-closed cases for:

```rust
serde_json::json!({
    "outcome": "answer_complete",
    "response": "alias",
    "response_redacted": "canonical"
})
```

and for an unknown field, a non-string `response`, and a missing response field. Each must return `ConversationFailure::MalformedOutput`.

- [ ] **Step 2: Verify the tests fail**

Run: `cargo test -p orchestrator-engine --all-features conversation_response_alias -- --nocapture`

Expected: alias inputs fail with `MalformedOutput` because `ConversationOutcome` currently recognizes only `response_redacted`.

- [ ] **Step 3: Implement the private wire enum and conversion**

Add a private strict enum with the same four variants and alias only the provider-owned response field:

```rust
#[derive(Deserialize)]
#[serde(rename_all = "snake_case", tag = "outcome", deny_unknown_fields)]
enum ProviderConversationOutcome {
    AnswerComplete {
        #[serde(alias = "response")]
        response_redacted: String,
    },
    MoreInformationNeeded {
        #[serde(alias = "response")]
        response_redacted: String,
        requirements: orchestrator_domain::RequirementSnapshot,
    },
    WorktreeTaskCandidate {
        #[serde(alias = "response")]
        response_redacted: String,
        requirements: orchestrator_domain::RequirementSnapshot,
    },
    NeedsAttention {
        #[serde(alias = "response")]
        response_redacted: String,
        evidence_redacted: String,
    },
}
```

Implement `From<ProviderConversationOutcome> for ConversationOutcome` with an exhaustive match. In `collect_conversation_response`, deserialize the private enum, convert it, then run the existing domain `validate()` call. Do not derive `Serialize` on the wire type.

Serde treats simultaneous alias and canonical keys as a duplicate field, so the dual-field test must remain rejected without custom repair logic.

- [ ] **Step 4: Verify focused engine behavior**

Run: `cargo test -p orchestrator-engine --all-features conversation_response_alias -- --nocapture`

Run: `cargo test -p orchestrator-engine --all-features conversation -- --nocapture`

Expected: all alias, strict-rejection, canonical-output, and existing conversation tests pass.

- [ ] **Step 5: Commit the boundary change**

```text
fix: normalize provider conversation response alias
```

### Task 2: Supply explicit canonical conversation shapes to providers

**Files:**
- Modify and test: `crates/orchestrator-cli/src/conversation_orchestrator.rs`
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Modify and test: `crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs`

**Interfaces:**
- Extend private `ConversationPrompt` with canonical output shapes.
- Keep the compatibility alias undocumented to providers so new output remains canonical.

- [ ] **Step 1: Add a failing serialized-prompt contract test**

Extract prompt construction into a private `conversation_prompt` helper or inspect the fake runtime's captured request. Assert that the serialized prompt includes this exact field contract and never advertises `response`:

```rust
assert_eq!(
    prompt["canonical_output_contract"],
    serde_json::json!([
        {
            "outcome": "answer_complete",
            "required_fields": ["response_redacted"]
        },
        {
            "outcome": "more_information_needed",
            "required_fields": ["response_redacted", "requirements"]
        },
        {
            "outcome": "worktree_task_candidate",
            "required_fields": ["response_redacted", "requirements"]
        },
        {
            "outcome": "needs_attention",
            "required_fields": ["response_redacted", "evidence_redacted"]
        }
    ])
);
```

The test must compare the final `serde_json::Value` keys and field names so it cannot pass through prose alone.

- [ ] **Step 2: Verify the prompt test fails**

Run: `cargo test -p orchestrator-cli --all-features conversation_prompt_lists_canonical_shapes -- --nocapture`

Expected: the new assertion fails because the prompt currently contains only outcome names and a generic one-object instruction.

- [ ] **Step 3: Add structured canonical shape data**

Replace the ambiguous `allowed_outcomes`-only guidance with typed serializable descriptors:

```rust
#[derive(Serialize)]
struct ConversationOutputContract {
    outcome: &'static str,
    required_fields: &'static [&'static str],
}

const CANONICAL_OUTPUT_CONTRACT: [ConversationOutputContract; 4] = [
    ConversationOutputContract {
        outcome: "answer_complete",
        required_fields: &["response_redacted"],
    },
    ConversationOutputContract {
        outcome: "more_information_needed",
        required_fields: &["response_redacted", "requirements"],
    },
    ConversationOutputContract {
        outcome: "worktree_task_candidate",
        required_fields: &["response_redacted", "requirements"],
    },
    ConversationOutputContract {
        outcome: "needs_attention",
        required_fields: &["response_redacted", "evidence_redacted"],
    },
];
```

Set `required_output` to say that `response_redacted` is the required response key, unknown fields are forbidden, and exactly one JSON object with no fences or prose is allowed. Keep the full `requirements_contract` and read-only constraints.

- [ ] **Step 4: Exercise the alias through a fake provider boundary**

Add a deterministic fake runtime scenario that returns the observed Codex-compatible payload:

```json
{"outcome":"answer_complete","response":"Hello! How can I help?"}
```

In `chat_conversation_fake_provider.rs`, assert the request completes and persisted/session output contains only canonical `response_redacted`. The fake scenario must be selected via test configuration and must not execute an installed provider.

- [ ] **Step 5: Verify the CLI conversation regressions**

Run: `cargo test -p orchestrator-cli --test chat_conversation_fake_provider --all-features -- --nocapture`

Expected: the prompt contract, compatibility alias, strict failure semantics, cancellation, and existing fake-provider tests all pass.

- [ ] **Step 6: Commit the provider prompt and E2E regression**

```text
test: specify canonical conversation output shapes
```

### Task 3: Expose a validated read-only legacy-import completion query

**Files:**
- Modify and test: `crates/orchestrator-state/src/legacy_import.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`

**Interfaces:**
- Add `LegacyImporter::completed_import` returning `StateResult<Option<LegacyImportResult>>`.
- A returned result retains the durable `imported: true`; only an idempotent `apply` result changes it to `false` to mean no rows were newly imported by that call.

- [ ] **Step 1: Add failing state tests for completion semantics**

Using existing legacy-import fixtures, add tests that assert:

```rust
assert_eq!(
    LegacyImporter::completed_import(
        &global,
        workspace_id,
        &plan.source_fingerprint,
        &paths,
    )?,
    None
);
let first = LegacyImporter::apply(&global, workspace_id, &plan, &paths)?;
assert!(first.imported);
let completed = LegacyImporter::completed_import(
    &global,
    workspace_id,
    &plan.source_fingerprint,
    &paths,
)?
.expect("matching import must be discoverable");
assert!(completed.imported);
assert_eq!(completed.source_fingerprint, plan.source_fingerprint);
let replay = LegacyImporter::apply(&global, workspace_id, &plan, &paths)?;
assert!(!replay.imported);
```

Add corruption cases for a mismatched workspace/manifest, missing or changed ID mappings, damaged audit evidence, and missing/mismatched published import. Each completion query must fail closed with `StateError::InvalidRecord` rather than return `None` or `Some`.

Add a different sealed source fingerprint case and assert it returns `None` for that plan.

- [ ] **Step 2: Verify the completion-query tests fail**

Run: `cargo test -p orchestrator-state --all-features legacy_import_completion -- --nocapture`

Expected: compilation fails because `LegacyImporter::completed_import` does not exist.

- [ ] **Step 3: Split durable validation from apply-call semantics**

Implement:

```rust
pub fn completed_import(
    global: &Database,
    target: WorkspaceId,
    source_fingerprint: &str,
    paths: &GlobalStatePaths,
) -> StateResult<Option<LegacyImportResult>> {
    let connection = global.raw_lock()?;
    let result = load_recorded_result_in(&connection, target, source_fingerprint)?;
    if let Some(result) = result.as_ref() {
        validate_published_import(result, target, paths)?;
    }
    Ok(result)
}
```

Extract `load_recorded_result_in(connection, target, source_fingerprint)` so it validates the ledger row against its sealed `result_json`, replayed audit, ID mappings, and anchor without needing a writable connection or a source plan. Keep `load_existing_result_in(connection, target, plan)` as the apply-specific wrapper that additionally compares the sealed result with the plan's manifest and event evidence. Remove `result.imported = false` from both validation helpers, because they validate persisted truth. At each idempotent return path in `apply` and `apply_transaction`, convert a validated existing result with one private helper:

```rust
fn replayed_apply_result(mut result: LegacyImportResult) -> LegacyImportResult {
    result.imported = false;
    result
}
```

Retain all existing fingerprint, target, manifest, timestamp, event evidence, mappings, anchor, and published-path validation. The query must accept a read-only snapshot whose temporary `Database::path()` differs from `GlobalStatePaths::database`; therefore do not reuse the writable `apply` path-equality guard. Do not modify tables or schema versions.

- [ ] **Step 4: Verify state behavior**

Run: `cargo test -p orchestrator-state --all-features legacy_import_completion -- --nocapture`

Run: `cargo test -p orchestrator-state --all-features legacy_import -- --nocapture`

Expected: new query tests and all existing first-apply/idempotent/corruption/source-race tests pass.

- [ ] **Step 5: Commit the state query**

```text
feat: expose validated legacy import completion
```

### Task 4: Make offline doctor ledger-aware and read-only

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify and test: `crates/orchestrator-cli/tests/global_doctor.rs`

**Interfaces:**
- Every successful `legacy_import` doctor check includes both `pending` and `imported` booleans.
- Offline doctor uses only `LegacyImporter::inspect` plus `Database::open_read_only_snapshot`.

- [ ] **Step 1: Add failing offline doctor regressions**

Extend the existing invalid-graph import fixture with four assertions:

```rust
assert_eq!(legacy_check["data"]["pending"], true);
assert_eq!(legacy_check["data"]["imported"], false);
```

before import; then run the supported migration/import path and assert:

```rust
assert_eq!(legacy_check["data"]["pending"], false);
assert_eq!(legacy_check["data"]["imported"], true);
```

For no source, assert `pending:false, imported:false`. For a changed source fingerprint after a prior completed import, assert `pending:true, imported:false`. Snapshot global row counts, source bytes/hash, and published artifact metadata before and after every `doctor` call and require no mutation.

- [ ] **Step 2: Verify the post-import test fails**

Run: `cargo test -p orchestrator-cli --test global_doctor --all-features legacy_import_doctor_reports_completed_import -- --nocapture`

Expected: failure because the current check always reports an inspectable legacy source as pending and omits `imported`.

- [ ] **Step 3: Reorder offline state inspection around the read-only snapshot**

Inspect the repository-local source once, but defer the final `legacy_import` check until after the global snapshot and registration are known. Use a private projection helper with this truth table:

```text
source absent                         => pending=false, imported=false
source present, no matching ledger    => pending=true,  imported=false
source present, validated exact ledger=> pending=false, imported=true
source inspection/ledger corruption   => failed check
```

When the global database does not exist or the workspace is unregistered, an inspectable source remains `pending:true, imported:false`. When a matching registration exists, call `LegacyImporter::completed_import` on the read-only snapshot. Include `source_schema_version`, `source_fingerprint`, and `source_database` only when a source plan exists.

Do not open the global database read-write and do not move the legacy source.

- [ ] **Step 4: Verify all offline doctor cases**

Run: `cargo test -p orchestrator-cli --test global_doctor --all-features legacy_import_doctor -- --nocapture`

Expected: pending, completed, no-source, changed-source, corruption/redaction, schema-eight, and no-mutation doctor tests pass.

- [ ] **Step 5: Commit offline doctor semantics**

```text
fix: distinguish completed legacy imports in doctor
```

### Task 5: Carry optional import completion evidence over live-daemon IPC

**Files:**
- Modify and test: `crates/orchestrator-daemon/src/ipc.rs`
- Modify: `crates/orchestrator-cli/src/ipc_client.rs`
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify and test: `crates/orchestrator-cli/tests/global_doctor.rs`

**Interfaces:**
- Add optional `legacy_source_fingerprint` to `WorkspaceDoctorLookupPayload`.
- Add optional typed `legacy_import` to `WorkspaceDoctorLookup`.
- Preserve IPC schema version 1 and make absent new fields deserialize safely.

- [ ] **Step 1: Add failing IPC compatibility tests**

Add serde tests proving an older payload/response with no new field still decodes, and a live lookup with an exact completed fingerprint reports imported:

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LegacyImportDoctorStatus {
    pub source_fingerprint: String,
    pub pending: bool,
    pub imported: bool,
}
```

Expected response projection:

```rust
Some(LegacyImportDoctorStatus {
    source_fingerprint: fingerprint,
    pending: false,
    imported: true,
})
```

Also test unmatched fingerprint (`pending:true, imported:false`), corrupted matching evidence (IPC failure), and unregistered repository (no inferred completion).

- [ ] **Step 2: Verify live doctor tests fail**

Run: `cargo test -p orchestrator-cli --test global_doctor --all-features live_doctor_reports_completed_legacy_import -- --nocapture`

Expected: current live doctor warns that import readiness is unavailable through IPC.

- [ ] **Step 3: Extend the IPC types without a schema bump**

In the daemon:

```rust
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LegacyImportDoctorStatus {
    pub source_fingerprint: String,
    pub pending: bool,
    pub imported: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceDoctorLookup {
    pub registered: bool,
    pub database: DatabaseHealth,
    pub daemon: orchestrator_state::DaemonStatus,
    pub diagnostics: Option<WorkspaceDoctorDiagnostics>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy_import: Option<LegacyImportDoctorStatus>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceDoctorLookupPayload {
    repository: PathBuf,
    #[serde(default)]
    legacy_source_fingerprint: Option<String>,
}
```

When the daemon finds a registered workspace and receives a fingerprint, call `LegacyImporter::completed_import` with that fingerprint. The state query must validate the exact ledger row, workspace binding, replayed audit, ID mappings, anchor, and published import before reporting completion. Return pending when no exact ledger exists and imported only after full validation. The client remains responsible for read-only source inspection; a source that changes before a later doctor invocation produces a different fingerprint and therefore returns pending rather than a false match.

- [ ] **Step 4: Send the client-inspected fingerprint and project the response**

Change the client signature to:

```rust
pub async fn doctor_lookup(
    repository: &Path,
    legacy_source_fingerprint: Option<&str>,
) -> Result<IpcResponse>
```

Serialize both fields as separated JSON values. In `doctor_state_checks`, inspect the source locally, pass its fingerprint, decode the optional typed status, and emit the same `pending`/`imported` JSON shape as offline doctor.

If an older daemon omits `legacy_import`, return the existing warning for a present source. If the repository is unregistered, also warn instead of reporting imported. No-source remains `pending:false, imported:false`.

- [ ] **Step 5: Verify live IPC and no-mutation behavior**

Run: `cargo test -p orchestrator-cli --test global_doctor --all-features live_doctor -- --nocapture`

Run: `cargo test -p orchestrator-daemon --all-features doctor_lookup -- --nocapture`

Expected: registered completed/unmatched/corrupt cases, older optional-field compatibility, unregistered read-only behavior, and existing deep doctor checks pass.

- [ ] **Step 6: Commit live-daemon doctor support**

```text
feat: report legacy import state through doctor IPC
```

### Task 6: Complete the four-provider fake QA matrix and update the tracker

**Files:**
- Modify and test: `crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs`
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

**Interfaces:**
- Test Codex, Claude, Gemini, and Antigravity through the same read-only conversation contract.
- Record source-fixed status separately from deployed-nightly validation.

- [ ] **Step 1: Add a table-driven four-provider contract test**

Run each provider ID through the fake runtime with canonical output:

```rust
for provider in [
    ProviderId::Codex,
    ProviderId::Claude,
    ProviderId::Gemini,
    ProviderId::Agy,
] {
    let response = orchestrator.converse(request_for(provider)).await?;
    let outcome = collect_conversation_response(&request_for(provider), response)?;
    assert!(matches!(outcome, ConversationOutcome::AnswerComplete { .. }));
}
```

Ensure the fixture creates fake executable/capability evidence for all four providers. Do not rely on PATH-installed real CLIs.

- [ ] **Step 2: Verify the provider matrix**

Run: `cargo test -p orchestrator-cli --test chat_conversation_fake_provider --all-features all_fake_providers -- --nocapture`

Expected: Codex, Claude, Gemini, and Agy pass the same read-only lifecycle and canonical outcome assertions.

- [ ] **Step 3: Record source-level QA evidence**

In `wsl-nightly-error-tracker.md`:

- mark WSL-020 and WSL-021 as `fixed-in-source, deployment-pending`;
- link the focused regression names and eventual commit hashes;
- add Antigravity to the standing provider QA matrix;
- state explicitly that Claude/Gemini/Agy account-unready results are external readiness states, not product defects;
- do not close either issue until deployed-nightly WSL evidence is attached.

- [ ] **Step 4: Commit the provider matrix and tracker update**

```text
test: cover all conversation providers in nightly QA
```

### Task 7: Run required verification and review the branch

**Files:**
- Review every changed file and commit in this plan.

- [ ] **Step 1: Run formatting**

Run: `cargo fmt --all -- --check`

Expected: exit code 0 with no formatting diff.

- [ ] **Step 2: Run full lint verification**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: exit code 0 with no warnings.

- [ ] **Step 3: Run the complete workspace suite**

Run: `cargo test --workspace --all-features`

Expected: exit code 0; every test uses fake providers.

- [ ] **Step 4: Apply the completion-verification skill**

Use `superpowers:verification-before-completion`, inspect fresh command output, inspect `git diff --check`, and confirm `.task4-target/` is not staged.

- [ ] **Step 5: Apply read-only code review**

Use `superpowers:requesting-code-review`. Resolve every material finding with a failing regression first, rerun the focused test, and repeat the three required workspace gates after the last change.

### Task 8: Publish, deploy, and validate the actual nightly in WSL

**Files:**
- Modify after deployed validation: `docs/qa/wsl-nightly-error-tracker.md`

**Precondition:** Tasks 1-7 are complete, the user authorizes branch publication/integration, CI passes, and the nightly artifact contains the merged commit.

- [ ] **Step 1: Finish and publish the development branch**

Use `superpowers:finishing-a-development-branch`, then the GitHub publication workflow selected by the user. Push only the intended commits, open a PR, wait for all required checks, and merge only when they are green.

- [ ] **Step 2: Confirm CD produced the expected nightly**

Record the merge SHA, successful release run, package version, and package integrity metadata. Do not substitute a locally built binary for deployment evidence.

- [ ] **Step 3: Clean-install the nightly in WSL**

Install into a new timestamped prefix, verify `which colay` resolves inside that prefix, and record `colay --version`. Use a new `COLAY_HOME` and a new WSL-native Git repository with a committed baseline. Preserve prior QA directories until evidence is recorded.

- [ ] **Step 4: Validate WSL-020 with bounded real-provider behavior**

Run safe public compatibility/doctor probes for Codex, Claude, Gemini, and Antigravity. Run at most one fixed-token, read-only conversation per account-ready provider. Require the observed Codex `response` payload to complete and require persisted/displayed output to be canonical `response_redacted`.

For Antigravity, run one bounded real inference only when its account is ready. If it is not ready, record the exact redacted readiness category separately and do not classify it as a Colay defect.

- [ ] **Step 5: Validate WSL-021 offline and with a live daemon**

Create or copy a sealed legacy source, run doctor before import, apply the supported migration/import, then run doctor with the daemon stopped and online. Require:

```json
{"pending":false,"imported":true}
```

for the exact completed fingerprint in both paths. Change the source fingerprint and require `pending:true, imported:false`. Recheck SQLite integrity, foreign keys, ledger cardinality, published import hashes, source hash immutability, and zero unintended writes from doctor.

- [ ] **Step 6: Close QA only from deployed evidence**

Update `wsl-nightly-error-tracker.md` with redacted commands, nightly version, merge SHA, CI/release links, provider versions, account-readiness distinctions, and the WSL-020/021 results. Mark each issue closed only if its deployed-nightly assertions passed. Stop the QA daemon and retain the timestamped evidence path.
