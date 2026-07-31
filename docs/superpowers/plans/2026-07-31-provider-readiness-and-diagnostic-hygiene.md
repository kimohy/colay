# Provider Readiness and Diagnostic Hygiene Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Make provider compatibility reports honest about unverified account readiness and keep real-provider failure output concise while retaining bounded, redacted diagnostic evidence.

**Architecture:** Add account readiness only to the CLI diagnostic `ProviderReport`, leaving persisted `ProviderHealth` and routing unchanged. Normalize raw provider diagnostic vocabulary inside `orchestrator-providers`, collect it through a deterministic bounded accumulator in the CLI conversation orchestrator, and keep the daemon's user-facing command error separate from detailed persisted evidence.

**Tech Stack:** Rust, Tokio, Serde/JSON, SQLite-backed conversation state, `orchestrator-test-support` fake provider binaries, Cargo workspace tests, Windows PowerShell, WSL 2 Ubuntu, npm nightly packaging.

**Design:** `docs/superpowers/specs/2026-07-31-provider-readiness-and-diagnostic-hygiene-design.md`

## Global Constraints

- Automated tests and CI must invoke only `orchestrator-test-support` fake binaries.
- Do not inspect, print, copy, or mutate provider credentials or account identifiers.
- Default compatibility and doctor commands must make zero inference requests.
- Keep provider-specific wire vocabulary in `orchestrator-providers`; do not add it to `orchestrator-domain`.
- Do not change SQLite schema versions or the persisted `ProviderHealth` contract.
- Keep writable Git readiness, approval, worktree isolation, redaction, and append-only audit semantics unchanged.
- Do not push, auto-merge, or delete the worktree. Prepare local PR and release evidence for an authorized maintainer.

---

### Task 1: Add a failing account-readiness report contract test

**Files:**
- Modify: `crates/orchestrator-cli/tests/global_doctor.rs`
- Modify: `crates/orchestrator-cli/tests/default_startup.rs`

**Step 1: Assert the additive provider report field**

Extend the fake-provider compatibility test to require every provider report to
retain its current `health.status` and include:

```rust
assert_eq!(provider["account_readiness"]["status"], "unverified");
assert!(provider["account_readiness"]["detail"]
    .as_str()
    .is_some_and(|detail| detail.contains("safe public probes")));
```

Also assert `data.inference_requests == 0` for both `doctor providers` and its
`compatibility` alias. Extend timestamp normalization to normalize
`account_readiness.checked_at` as well as `health.checked_at`.

**Step 2: Assert normal doctor does not pass an unverified provider**

In `global_doctor.rs`, run `colay --json doctor` with fake providers and assert:

```rust
assert_eq!(provider_check["status"], "warn");
assert!(provider_check["detail"]
    .as_str()
    .is_some_and(|detail| detail.contains("account readiness unverified")));
assert_eq!(document["data"]["inference_requests"], 0);
```

**Step 3: Run the targeted tests and confirm failure**

Run:

```text
cargo test -p colay --features test-fixtures --test global_doctor compatibility_is_a_behavioral_alias_of_doctor_providers -- --exact
cargo test -p colay --features test-fixtures --test default_startup future_codex_version_is_writable_by_minimum_version_policy -- --exact
```

Expected: FAIL because `account_readiness` is absent and a healthy fake provider
still maps to a passing doctor check.

---

### Task 2: Implement report-level account readiness

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Test: `crates/orchestrator-cli/tests/global_doctor.rs`
- Test: `crates/orchestrator-cli/tests/default_startup.rs`

**Step 1: Add CLI-local serialized readiness types**

Near `ProviderReport`, introduce a CLI diagnostic type whose serialized shape is:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum AccountReadinessStatus {
    Unverified,
}

#[derive(Clone, Debug, Serialize)]
struct AccountReadinessReport {
    status: AccountReadinessStatus,
    detail: String,
    checked_at: DateTime<Utc>,
}
```

Keep the initial implementation honest by constructing only `Unverified`.
Reserve `ready` and `blocked` as documented future vocabulary rather than adding
unconstructed private variants that would trigger dead-code warnings.

**Step 2: Populate readiness without inference**

Add `account_readiness` to `ProviderReport`. Both successful and failed public
binary probes create an unverified report using a bounded deterministic detail.
Do not call a login, auth, entitlement, account, or inference command.

**Step 3: Change only doctor presentation status**

For the provider check in normal `doctor`, map unverified readiness to
`CheckStatus::Warn` even when binary health is healthy. Preserve the health
object and capabilities unchanged. Use the approved detail:

```text
compatible; account readiness unverified (safe public probes do not make an inference request)
```

Unhealthy/degraded binary detail remains available in the nested provider
report. Do not change routing's use of `ProviderHealth`.

**Step 4: Run targeted tests**

Run:

```text
cargo test -p colay --test global_doctor --features test-fixtures
cargo test -p colay --test default_startup --features test-fixtures
```

Expected: PASS.

**Step 5: Commit**

```text
git add crates/orchestrator-cli/src/app.rs crates/orchestrator-cli/tests/global_doctor.rs crates/orchestrator-cli/tests/default_startup.rs
git commit -m "fix: distinguish provider account readiness"
```

---

### Task 3: Add failing provider diagnostic-normalization tests

**Files:**
- Modify: `crates/orchestrator-providers/src/normalize.rs`
- Modify: `crates/orchestrator-providers/src/lib.rs`

**Step 1: Specify unsafe-advice replacement**

Add unit tests for a public function such as:

```rust
normalize_provider_diagnostic(ProviderId::Agy, input)
```

The output must contain:

```text
provider requested an unsafe permission bypass; Colay did not enable it
```

and must not contain the raw permission-bypass flag.

**Step 2: Specify provider-stack compaction**

Use a Gemini diagnostic fixture containing a primary error followed by repeated
JavaScript `at ...` frames. Assert the primary error is retained, duplicate
frames are removed, and omitted frames receive one deterministic summary.

**Step 3: Specify non-destructive normalization**

Add Claude/Gemini/Agy examples proving authentication, credit/billing, and
unsupported-account phrases survive normalization for engine classification.

**Step 4: Run tests and confirm failure**

Run:

```text
cargo test -p orchestrator-providers normalize::tests::diagnostic -- --nocapture
```

Expected: FAIL because the public normalizer does not exist.

---

### Task 4: Implement provider-owned diagnostic normalization

**Files:**
- Modify: `crates/orchestrator-providers/src/normalize.rs`
- Modify: `crates/orchestrator-providers/src/lib.rs`

**Step 1: Implement a deterministic text normalizer**

Implement and export:

```rust
pub fn normalize_provider_diagnostic(provider: ProviderId, input: &str) -> String
```

It must normalize line endings, drop blank lines, preserve first-seen diagnostic
order, remove exact duplicate lines, replace unsafe permission-bypass advice,
and compact provider implementation stack frames without hiding the primary
error. Use separated Rust string operations only; do not invoke a shell.

**Step 2: Keep classification phrases intact**

Do not translate away authentication, billing/credit/quota, unsupported
client/account, timeout, or cancellation phrases used by the vendor-neutral
engine classifier.

**Step 3: Run provider tests**

Run:

```text
cargo test -p orchestrator-providers
```

Expected: PASS.

**Step 4: Commit**

```text
git add crates/orchestrator-providers/src/normalize.rs crates/orchestrator-providers/src/lib.rs
git commit -m "fix: normalize provider failure diagnostics"
```

---

### Task 5: Add failing bounded evidence-accumulator tests

**Files:**
- Modify: `crates/orchestrator-cli/src/conversation_orchestrator.rs`

**Step 1: Specify duplicate unknown-event aggregation**

Add unit tests for a private `ConversationEvidence` accumulator. Push the same
`gemini.stderr` unknown event multiple times and assert final evidence contains
one summary with the occurrence count, not one line per event.

**Step 2: Specify line and byte bounds**

Assert:

- at most 64 total evidence lines, including a deterministic omission marker;
- at most 2 KiB for a single retained line;
- at most `CONVERSATION_MAX_EVIDENCE_BYTES` total;
- valid UTF-8 after truncating multibyte text; and
- stable first-seen ordering after duplicate removal.

**Step 3: Specify runtime diagnostic normalization**

Push Agy stderr containing the raw unsafe flag and Gemini stderr containing a
long stack. Assert the accumulator output contains safe normalized evidence and
does not contain the raw flag.

**Step 4: Run the tests and confirm failure**

Run:

```text
cargo test -p colay conversation_orchestrator::tests::evidence --features test-fixtures -- --nocapture
```

Expected: FAIL because evidence is still appended directly to a `Vec<String>`.

---

### Task 6: Route conversation evidence through the bounded accumulator

**Files:**
- Modify: `crates/orchestrator-cli/src/conversation_orchestrator.rs`
- Modify if needed: `crates/orchestrator-cli/Cargo.toml`

**Step 1: Implement the accumulator**

Use deterministic collections (`BTreeMap` for unknown-event counts and a
first-seen list/set for diagnostic lines). The accumulator owns constants for 64
lines and 2 KiB per line and enforces the engine's 16 KiB total limit.

**Step 2: Replace direct evidence pushes**

Route capability evidence, quota detail, unknown events, stderr, runtime
truncation, tree-termination error, and lifecycle parse errors through the
accumulator. Send stderr and provider-authored diagnostic strings through
`orchestrator_providers::normalize_provider_diagnostic(provider, ...)` first.

Unknown non-lifecycle events remain nonterminal. Lifecycle-affecting unknown
events still fail closed but use one bounded summary.

**Step 3: Run CLI unit and conversation integration tests**

Run:

```text
cargo test -p colay conversation_orchestrator --features test-fixtures
cargo test -p colay --test chat_conversation_fake_provider --features test-fixtures
```

Expected: PASS.

**Step 4: Commit**

```text
git add crates/orchestrator-cli/src/conversation_orchestrator.rs crates/orchestrator-cli/Cargo.toml Cargo.lock
git commit -m "fix: bound conversation failure evidence"
```

Only stage `Cargo.toml` or `Cargo.lock` if the implementation actually changes
them.

---

### Task 7: Add a fake-provider noisy-failure end-to-end fixture

**Files:**
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Modify: `crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs`

**Step 1: Add a fake diagnostic-noise scenario**

Extend the test-support fake runtime with a prompt-selected scenario such as
`scenario:diagnostic-noise`. It must emit repeated non-lifecycle Gemini stderr
events, a nonzero exit, duplicate JavaScript-style frames, and an Agy-style
unsafe permission-bypass suggestion. This is inert fixture text and is never
executed.

**Step 2: Add a production-path CLI integration test**

Run the fake provider through `OfficialCliConversationOrchestrator` and assert:

- one unknown-event summary is retained;
- raw duplicates are absent;
- the raw unsafe flag is absent;
- the safe Colay replacement is present;
- evidence stays within the line and byte limits; and
- the provider failure remains terminal with no fallback process.

**Step 3: Run the fake-provider suites**

Run:

```text
cargo test -p orchestrator-test-support --test provider_e2e
cargo test -p colay --test chat_conversation_fake_provider --features test-fixtures
```

Expected: PASS.

**Step 4: Commit**

```text
git add crates/orchestrator-test-support/src/runtime.rs crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs
git commit -m "test: cover noisy provider failure evidence"
```

---

### Task 8: Separate concise command errors from persisted evidence

**Files:**
- Modify: `crates/orchestrator-daemon/tests/conversation_flow.rs`
- Modify: `crates/orchestrator-daemon/src/conversation.rs`

**Step 1: Add failing persistence/presentation assertions**

Extend `assert_terminal_provider_failure` and replay assertions so that:

```rust
assert!(error.contains(case.expected_action));
assert!(!error.contains("Evidence:"));
assert!(!error.contains(&case.response.evidence_redacted));
assert!(evidence_redacted.contains(expected_evidence_marker));
```

Use a case with multiline bounded evidence to prove the detail remains in the
stored `NeedsAttention` outcome but not in `error_redacted` or the command
outcome shown to noninteractive callers.

**Step 2: Run the targeted test and confirm failure**

Run:

```text
cargo test -p orchestrator-daemon --features test-fixtures --test conversation_flow provider_failures_are_terminal_actionable_and_preserve_the_session -- --exact --nocapture
```

Expected: FAIL because `failure_error_from_outcome` currently appends
`Evidence: ...`.

**Step 3: Return only the actionable response**

Change `failure_error_from_outcome` to redact and bound
`outcome_response(outcome)` only. Keep `nonblank_failure_evidence` and the
`NeedsAttention.evidence_redacted` persistence path unchanged. Ensure replay
uses the same concise response and does not start a provider.

**Step 4: Run daemon and engine failure suites**

Run:

```text
cargo test -p orchestrator-engine --test conversation_collector
cargo test -p orchestrator-daemon --test conversation_flow --features test-fixtures
```

Expected: PASS.

**Step 5: Commit**

```text
git add crates/orchestrator-daemon/src/conversation.rs crates/orchestrator-daemon/tests/conversation_flow.rs
git commit -m "fix: keep conversation errors concise"
```

---

### Task 9: Run focused cross-layer regression tests

**Files:**
- Test only unless failures reveal an in-scope defect.

**Step 1: Run provider and engine crates**

```text
cargo test -p orchestrator-providers
cargo test -p orchestrator-engine
```

**Step 2: Run relevant CLI and daemon integrations**

```text
cargo test -p colay --test global_doctor --features test-fixtures
cargo test -p colay --test default_startup --features test-fixtures
cargo test -p colay --test chat_conversation_fake_provider --features test-fixtures
cargo test -p orchestrator-daemon --test conversation_flow --features test-fixtures
```

**Step 3: Inspect the diff for architecture violations**

```text
git diff origin/main...HEAD --check
git status --short
rg -n "dangerously-skip-permissions" crates docs
```

Expected: the raw unsafe flag appears only in inert regression fixtures or
assertions and never in production user guidance or constructed provider argv.

---

### Task 10: Run required Windows workspace verification

**Files:**
- Produce: local command logs for the QA handoff.

**Step 1: Format and check**

```text
cargo fmt --all
cargo fmt --all -- --check
```

If formatting changes tracked files, inspect and commit only the in-scope
formatting with the owning implementation task.

**Step 2: Run strict Clippy**

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

**Step 3: Run the full workspace tests**

```text
cargo test --workspace --all-features
```

**Step 4: Build the release candidate and smoke-test Windows**

```text
cargo build --release -p colay
target\release\colay.exe --version
target\release\colay.exe --json compatibility
target\release\colay.exe --json doctor
```

Use a temporary isolated `COLAY_HOME` and fake configured provider executables
for non-inference compatibility checks. Verify no repository-local `.colay`
state is created by read-only diagnostics.

---

### Task 11: Perform isolated WSL release-candidate QA

**Files:**
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Produce outside repository: a timestamped evidence directory under `/home/kimohy/.cache/`

**Step 1: Build the Linux release artifact in WSL**

From the WSL-native source checkout or a read-only source transfer of the
approved commit, build the release candidate. Do not reuse the user's existing
Colay database.

**Step 2: Create isolated install and state roots**

Create timestamped npm prefix, `COLAY_HOME`, and non-Git workspace directories.
Record only tool paths, versions, exit codes, bounded output, database status,
and process cleanup evidence.

**Step 3: Verify readiness reporting**

Run safe `--version`, `compatibility`, `doctor providers`, and `doctor` commands.
Confirm:

- binary health retains its existing value;
- account readiness is unverified;
- provider doctor checks warn rather than imply account readiness;
- inference request count is zero; and
- no repository-local state or writable task state is created.

**Step 4: Verify real-provider failure hygiene**

For installed providers and only with the user's standing authorization, issue
one bounded non-sensitive read-only request per provider. Confirm normal output
is concise, stored evidence is normalized and bounded, the raw unsafe flag is
absent, failures exit nonzero, provider selection is exact, and no task,
worktree, or lease is created. Never print credential values.

**Step 5: Verify cleanup and state integrity**

Stop the isolated daemon and verify no provider/daemon process or socket remains.
Run SQLite integrity and foreign-key checks. Keep the user's pre-existing Colay
state untouched.

**Step 6: Close or update the QA tracker**

Update WSL-017 and WSL-018 with candidate commit, commands, observed result,
evidence location, and status. Do not mark an item closed without direct
evidence.

**Step 7: Commit QA evidence metadata**

```text
git add docs/qa/wsl-nightly-error-tracker.md
git commit -m "docs: verify provider readiness diagnostics"
```

---

### Task 12: Prepare review, PR, and release handoff

**Files:**
- No source changes expected.

**Step 1: Verify branch scope and cleanliness**

```text
git status --short
git log --oneline --decorate origin/main..HEAD
git diff --stat origin/main...HEAD
git diff --check origin/main...HEAD
```

**Step 2: Prepare the PR description**

Summarize WSL-017/018, the additive readiness contract, failure-evidence safety
rules, test evidence, Windows results, WSL real-provider results, schema impact
(`none`), and rollback notes.

**Step 3: Stop at the repository delivery boundary**

Do not push, auto-merge, or delete the worktree. Provide the authorized
maintainer with:

- branch and commit IDs;
- exact push/PR/CI/merge/nightly commands or UI steps;
- required CI checks;
- candidate QA evidence paths; and
- post-publish clean-install WSL commands that confirm the npm nightly contains
  the merged commit.

**Step 4: Post-publish acceptance for the maintainer**

After an authorized maintainer publishes the nightly, clean-install
`@kimohy/colay@nightly` into a new timestamped WSL prefix and repeat the safe
readiness plus bounded provider checks. Confirm the package version/commit,
WSL-017/018 behavior, state integrity, and cleanup before declaring deployment
verified.
