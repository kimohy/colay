# Agy Provider Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the official Antigravity CLI (`agy`) as a first-class Colay provider while retaining the existing Gemini CLI provider.

**Architecture:** Add only the vendor-neutral `Agy` identity to the domain, while keeping all Agy flags and plain-text normalization inside `orchestrator-providers`. A dedicated `AgyText` runtime marker converts a bounded, redacted process-exit frame into the completion event required by existing planner and worker loops without weakening structured-provider contracts.

**Tech Stack:** Rust 2024 workspace, Tokio process runtime, Serde/TOML configuration, SQLite-backed state, Clap CLI, Ratatui TUI, `orchestrator-test-support` fake provider binary.

## Global Constraints

- Preserve `gemini`, `codex`, and `claude` provider identities and all existing schema versions.
- Use Rust `Command` semantics with separated executable and argument values; never add shell interpolation.
- Keep provider wire types in provider/compatibility crates; `orchestrator-domain` remains vendor-neutral and I/O-free.
- Never add identity rotation, quota bypass, usage-page scraping, credential extraction, credit purchasing, unofficial endpoints, or default external telemetry.
- Missing Agy usage remains unknown and is never combined with Gemini usage.
- Writable Agy workers run only in Colay's isolated worktrees; reviewers remain read-only.
- Never pass `--dangerously-skip-permissions`.
- Tests and CI invoke only `orchestrator-test-support` fake binaries and never real Codex, Claude, Gemini, or Agy inference.
- Required final verification is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

### Task 1: Provider identity, configuration, and profile defaults

**Files:**
- Modify: `crates/orchestrator-domain/src/usage.rs`
- Modify: `crates/orchestrator-state/src/config.rs`
- Modify: `crates/orchestrator-cli/src/profile_config.rs`
- Test: unit tests in the three files above

**Interfaces:**
- Produces: `ProviderId::Agy`, serialized/display value `"agy"`, `ProviderConfigs::agy: Option<ProviderConfig>`.
- Produces: built-in Agy profiles consumed by routing, CLI, TUI, planner, and worker construction.

- [ ] **Step 1: Add failing provider identity tests**

Add domain assertions before adding the enum variant:

```rust
#[test]
fn agy_provider_identity_is_stable() {
    assert_eq!(ProviderId::from_str("agy"), Ok(ProviderId::Agy));
    assert_eq!(ProviderId::Agy.as_str(), "agy");
    assert_eq!(serde_json::to_string(&ProviderId::Agy).unwrap(), "\"agy\"");
}
```

Run: `cargo test -p orchestrator-domain agy_provider_identity_is_stable -- --exact`

Expected: FAIL because `ProviderId::Agy` does not exist.

- [ ] **Step 2: Implement the additive provider identity**

Add `Agy` to `ProviderId` and both string matches:

```rust
pub enum ProviderId {
    Gemini,
    Agy,
    Codex,
    Claude,
}

Self::Agy => "agy",
"agy" => Ok(Self::Agy),
```

Run: `cargo test -p orchestrator-domain agy_provider_identity_is_stable -- --exact`

Expected: PASS.

- [ ] **Step 3: Add failing default configuration and profile tests**

Extend configuration tests to assert:

```rust
let config = RootConfig::default();
let agy = config.orchestrator.providers.agy.as_ref().expect("agy default");
assert!(agy.enabled);
assert_eq!(agy.executable, "agy");
assert_eq!(agy.quota_period, "calendar_day");
assert_eq!(agy.priority, 80);
assert_eq!(
    config.orchestrator.model_profiles["agy"]["economy"].model,
    "gemini-3.5-flash-low"
);
assert_eq!(
    config.orchestrator.model_profiles["agy"]["standard"].model,
    "gemini-3.5-flash-medium"
);
assert_eq!(
    config.orchestrator.model_profiles["agy"]["premium"].model,
    "gemini-3.1-pro-high"
);
```

Update `effective_rows_identify_builtin_and_customized_values` to expect 12 rows and include an Agy row.

Run: `cargo test -p orchestrator-state config::tests --lib && cargo test -p orchestrator-cli profile_config::tests --lib`

Expected: FAIL because Agy defaults and rows are absent.

- [ ] **Step 4: Implement configuration and profile defaults**

Add `agy` to `ProviderConfigs`, `Default`, and `iter`, then define:

```rust
fn default_agy_provider() -> ProviderConfig {
    default_provider("agy", "calendar_day", None, 80)
}
```

Add the complete model map:

```rust
(
    "agy".to_owned(),
    provider_profiles(
        "gemini-3.5-flash-low",
        "gemini-3.5-flash-medium",
        "gemini-3.1-pro-high",
    ),
),
```

Accept `agy` in provider-limit and profile-target validation and iterate `codex`, `claude`, `agy`, `gemini` when producing effective rows.

Run: `cargo test -p orchestrator-state config::tests --lib && cargo test -p orchestrator-cli profile_config::tests --lib`

Expected: PASS.

- [ ] **Step 5: Commit the identity and defaults**

```text
git add crates/orchestrator-domain/src/usage.rs crates/orchestrator-state/src/config.rs crates/orchestrator-cli/src/profile_config.rs
git commit -m "feat: add agy provider identity and defaults"
```

---

### Task 2: Agy adapter contract

**Files:**
- Create: `crates/orchestrator-providers/src/agy.rs`
- Modify: `crates/orchestrator-providers/src/adapter.rs`
- Modify: `crates/orchestrator-providers/src/lib.rs`
- Modify: `crates/orchestrator-providers/src/usage_probe.rs`
- Test: unit tests in `crates/orchestrator-providers/src/agy.rs`

**Interfaces:**
- Consumes: `ProviderId::Agy`, `PreparedInvocation`, `SharedRuntime`, `prompt_payload`, and `output_limits`.
- Produces: `AgyAdapter`, `AgyAdapterConfig`, and `StructuredOutput::AgyText`.

- [ ] **Step 1: Write failing adapter preparation tests**

Create `agy.rs` with test-only desired API and assert exact arguments:

```rust
#[test]
fn prepares_read_only_print_invocation() {
    let invocation = adapter().prepare(&request(SandboxMode::ReadOnly)).unwrap();
    assert_eq!(
        invocation.args_lossy(),
        [
            "--print", "--mode", "plan", "--sandbox", "--model",
            "gemini-3.5-flash-medium"
        ]
    );
    assert_eq!(invocation.output, StructuredOutput::AgyText);
    assert!(!invocation.args_lossy().contains(&"--dangerously-skip-permissions".to_owned()));
}

#[test]
fn prepares_writable_and_resume_arguments_separately() {
    let mut request = request(SandboxMode::WorkspaceWrite);
    request.resume_session_id = Some("conversation-7".to_owned());
    let args = adapter().prepare(&request).unwrap().args_lossy();
    assert!(args.windows(2).any(|pair| pair == ["--mode", "accept-edits"]));
    assert!(args.windows(2).any(|pair| pair == ["--conversation", "conversation-7"]));
}
```

Run: `cargo test -p orchestrator-providers agy::tests --lib`

Expected: FAIL because the module, adapter, and output marker do not exist.

- [ ] **Step 2: Implement minimal Agy preparation**

Define:

```rust
#[derive(Debug, Clone)]
pub struct AgyAdapterConfig {
    pub executable: PathBuf,
    pub usage_probe: UsageProbeConfig,
    pub usage_scope: QuotaScope,
}

pub struct AgyAdapter {
    config: AgyAdapterConfig,
    runtime: SharedRuntime,
}
```

Build separated args in this exact order: `--print`, `--mode`, `plan|accept-edits`, `--sandbox`, optional `--model <slug>`, optional `--conversation <id>`. Use `prompt_payload(request)?` as stdin and `StructuredOutput::AgyText`.

Add `AgyText` to `PreparedInvocation::validate` as a non-App-Server output and export the adapter from `lib.rs`.

Run: `cargo test -p orchestrator-providers agy::tests --lib`

Expected: preparation tests PASS; capability tests are not yet present.

- [ ] **Step 3: Write failing capability and unknown-usage tests**

Use a fake `AdapterRuntime` that returns `1.1.4` for `--version` and the observed help flags for `--help`:

```rust
let capabilities = adapter.capabilities().await.unwrap();
assert_eq!(capabilities.provider, ProviderId::Agy);
assert!(capabilities.non_interactive.usable());
assert!(capabilities.read_only.usable());
assert!(capabilities.writable.usable());
assert!(capabilities.session_resume.usable());
assert_eq!(capabilities.structured_output, CapabilitySupport::Degraded);
assert_eq!(capabilities.output_schema, CapabilitySupport::Unsupported);

let usage = adapter.collect_usage().await.unwrap();
assert_eq!(usage[0].provider, ProviderId::Agy);
assert_eq!(usage[0].confidence, UsageConfidence::Unknown);
```

Run: `cargo test -p orchestrator-providers agy::tests --lib`

Expected: FAIL because capability and usage methods are incomplete.

- [ ] **Step 4: Implement probing, start gating, usage, and event parsing**

Probe only `--version` and `--help`. Mark non-interactive from `--print`, read-only from `--mode` plus `plan`, writable from `accept-edits`, resume from `--conversation`, and structured output as `Degraded` only when the plain-text bridge prerequisites are present.

Implement `collect_usage` with the existing configured JSON probe; otherwise return `UsageSnapshot::unknown(ProviderId::Agy, ...)`. Add Agy's default scope to `usage_probe.rs` without comparing it to Gemini.

Normalize stdout to `Message`, stderr to non-lifecycle `Unknown`, and reserve protocol frames for Task 3.

Run: `cargo test -p orchestrator-providers agy::tests --lib`

Expected: PASS.

- [ ] **Step 5: Commit the adapter contract**

```text
git add crates/orchestrator-providers/src/agy.rs crates/orchestrator-providers/src/adapter.rs crates/orchestrator-providers/src/lib.rs crates/orchestrator-providers/src/usage_probe.rs
git commit -m "feat: add agy provider adapter"
```

---

### Task 3: Plain-text completion lifecycle bridge

**Files:**
- Modify: `crates/orchestrator-providers/src/process_runtime.rs`
- Modify: `crates/orchestrator-providers/src/agy.rs`
- Test: unit tests in both files

**Interfaces:**
- Consumes: `StructuredOutput::AgyText`.
- Produces: redacted protocol frame `{ "type": "orchestrator.process_exited", "exit_code": N }` only for AgyText static processes.

- [ ] **Step 1: Write failing Agy exit-frame parsing tests**

```rust
#[tokio::test]
async fn successful_runtime_exit_becomes_completed() {
    let raw = RawEvent {
        channel: RawEventChannel::Protocol,
        sequence: 2,
        bytes: br#"{"type":"orchestrator.process_exited","exit_code":0}"#.to_vec(),
        received_at: Utc::now(),
    };
    assert!(matches!(adapter().parse_event(raw).await.unwrap(), WorkerEvent::Completed { .. }));
}

#[tokio::test]
async fn nonzero_runtime_exit_becomes_error() {
    let raw = RawEvent {
        channel: RawEventChannel::Protocol,
        sequence: 2,
        bytes: br#"{"type":"orchestrator.process_exited","exit_code":17}"#.to_vec(),
        received_at: Utc::now(),
    };
    assert!(matches!(
        adapter().parse_event(raw).await.unwrap(),
        WorkerEvent::Error { code: Some(code), retryable: false, .. }
            if code == "agy_process_exit"
    ));
}
```

Run: `cargo test -p orchestrator-providers agy::tests --lib`

Expected: FAIL because protocol exit frames are not parsed.

- [ ] **Step 2: Implement strict protocol normalization**

Deserialize only the expected type and integer exit code. Return `Completed { summary: None, usage: None }` for zero, `Error { code: Some("agy_process_exit"), ... }` for non-zero, and `ProviderError::MalformedOutput` for any other protocol payload.

Run: `cargo test -p orchestrator-providers agy::tests --lib`

Expected: PASS.

- [ ] **Step 3: Write a failing process-runtime integration test**

Start the compiled fake binary with `StructuredOutput::AgyText`, drain raw events, and assert that stdout arrives before exactly one process-exit protocol frame. Run the fake process with no inference and an exit code selected by its scenario arguments.

Run: `cargo test -p orchestrator-providers agy_text_runtime_emits_exit_protocol --lib`

Expected: FAIL because `drive_static` discards `ProcessEvent::Exited`.

- [ ] **Step 4: Emit the AgyText exit frame without changing other transports**

Pass the active `StructuredOutput` into `drive_static`. On `ProcessEvent::Exited { exit_code, .. }`, emit the bounded redacted protocol JSON only when `output == StructuredOutput::AgyText`, then end the loop. Keep all other output types byte-for-byte on their existing path.

Run: `cargo test -p orchestrator-providers agy_text_runtime_emits_exit_protocol --lib && cargo test -p orchestrator-providers --all-features`

Expected: PASS with existing provider tests unchanged.

- [ ] **Step 5: Commit the lifecycle bridge**

```text
git add crates/orchestrator-providers/src/process_runtime.rs crates/orchestrator-providers/src/agy.rs
git commit -m "feat: bridge agy plain text lifecycle"
```

---

### Task 4: Fake provider and end-to-end adapter coverage

**Files:**
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Modify: `crates/orchestrator-test-support/tests/provider_e2e.rs`
- Modify: `crates/orchestrator-test-support/tests/multi_provider_handover_e2e.rs`

**Interfaces:**
- Consumes: `AgyAdapter`, `AgyText`, and `ProviderId::Agy`.
- Produces: deterministic fake Agy help, plain stdout, completion protocol, crash, timeout, secret-redaction, and handover fixtures.

- [ ] **Step 1: Write a failing Agy fake-provider E2E test**

```rust
#[tokio::test]
async fn agy_plain_text_worker_completes_through_fake_runtime() -> Result<()> {
    let adapter = AgyAdapter::new(
        AgyAdapterConfig {
            executable: fake_provider_path()?,
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Agy),
        },
        fake_runtime(FakeRuntimeScenario::Success)?,
    );
    let handle = adapter.start(request(ProviderId::Agy, "success")?).await?;
    let events = drain(&adapter, &handle).await?;
    assert!(events.iter().any(|event| matches!(event, WorkerEvent::Message { text } if text == "done")));
    assert!(events.iter().any(|event| matches!(event, WorkerEvent::Completed { .. })));
    Ok(())
}
```

Run: `cargo test -p orchestrator-test-support --test provider_e2e agy_plain_text_worker_completes_through_fake_runtime -- --exact`

Expected: FAIL because fake Agy scenarios are missing.

- [ ] **Step 2: Implement deterministic Agy fake behavior**

Make fake help include `--print --mode plan accept-edits --sandbox --model --conversation`. Detect Agy from `--print` plus `--mode` before the Gemini fallback. For Agy success emit `done` as plain stdout followed by the synthetic protocol frame in `FakeAdapterRuntime`; emit strict malformed protocol, crash, timeout, and secret variants for their scenarios.

Extend every exhaustive provider match in `runtime.rs` so Agy planner text and handover acknowledgements remain plain text and are followed by completion.

Run: `cargo test -p orchestrator-test-support --test provider_e2e agy_plain_text_worker_completes_through_fake_runtime -- --exact`

Expected: PASS.

- [ ] **Step 3: Add failure, redaction, and handover tests**

Add these concrete assertions to the provider E2E suite:

```rust
let crash = agy_adapter(FakeRuntimeScenario::ProcessCrash)?;
let handle = crash.start(request(ProviderId::Agy, "scenario:crash")?).await?;
assert_eq!(crash.wait(&handle).await?.exit_code, Some(17));

let usage = agy_adapter(FakeRuntimeScenario::Success)?.collect_usage().await?;
assert_eq!(usage[0].provider, ProviderId::Agy);
assert_eq!(usage[0].confidence, UsageConfidence::Unknown);

let secret = agy_adapter(FakeRuntimeScenario::SecretOutput)?;
let handle = secret.start(request(ProviderId::Agy, "scenario:secret")?).await?;
let events = drain(&secret, &handle).await?;
assert!(events.iter().all(|event| !format!("{event:?}").contains("supersecretvalue")));
```

Extend the handover fixture with `current_worker: ProviderId::Agy` and assert the sealed bundle retains `ProviderId::Agy` while its acknowledgement target remains `ProviderId::Gemini`.

Run: `cargo test -p orchestrator-test-support --all-features`

Expected: PASS.

- [ ] **Step 4: Commit fake-provider integration**

```text
git add crates/orchestrator-test-support/src/runtime.rs crates/orchestrator-test-support/tests/provider_e2e.rs crates/orchestrator-test-support/tests/multi_provider_handover_e2e.rs
git commit -m "test: cover agy provider lifecycle"
```

---

### Task 5: CLI, planner, routing, and TUI integration

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/task_planner.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: `crates/orchestrator-cli/src/chat_tui.rs`
- Modify: `crates/orchestrator-cli/tests/chat_plan_fake_provider.rs`
- Modify: `crates/orchestrator-cli/tests/fake_cli_handover_e2e.rs`
- Modify: `crates/orchestrator-cli/tests/parallel_task_execution.rs`
- Modify: `crates/orchestrator-policy/src/routing.rs`

**Interfaces:**
- Consumes: `ProviderConfigs::agy`, `AgyAdapter`, Agy model profiles.
- Produces: `colay providers`, enable/disable, profiles, planner selection, task execution, routing, handover, daemon recovery, and TUI controls for Agy.

- [ ] **Step 1: Add failing CLI report and routing tests**

Add these assertions to the existing provider/profile report tests:

```rust
let providers = provider_configs(&RootConfig::default().orchestrator).collect::<Vec<_>>();
assert_eq!(providers.len(), 4);
assert!(providers.iter().any(|(provider, config)| {
    *provider == ProviderId::Agy && config.enabled && config.priority == 80
}));

let profiles = effective_profile_rows(&RootConfig::default(), &RootConfig::default())?;
assert_eq!(profiles.iter().filter(|row| row.provider == "agy").count(), 3);
```

Extend `provider_enable_adds_only_the_requested_boolean` to disable `ProviderId::Agy` and assert the written TOML contains `[orchestrator.providers.agy]` with only `enabled = false`; the other three provider entries must remain absent from that override.

In the planner priority test, provide safe capabilities for all four providers and assert `primary_provider() == ProviderId::Codex`; disable Codex and Claude in the cloned config and assert the next selection is `ProviderId::Agy`, ahead of Gemini.

Run: `cargo test -p orchestrator-cli agy --all-features`

Expected: FAIL because CLI wiring does not recognize Agy.

- [ ] **Step 2: Wire Agy through adapter factories and configuration lookups**

In both `app.rs::provider_adapter` and `task_planner.rs::build_provider_adapter`, add:

```rust
ProviderId::Agy => Ok(Box::new(AgyAdapter::new(
    AgyAdapterConfig {
        executable: PathBuf::from(&provider_config.executable),
        usage_probe,
        usage_scope: scope,
    },
    runtime,
))),
```

Add Agy to `provider_configs`, `provider_config`, planner probe order, daemon string parsing, status rows, TUI controls, and profile commands. Preserve the explicit default numeric priorities from configuration rather than hard-coding a new routing shortcut.

Run: `cargo check --workspace --all-targets --all-features`

Expected: compiler identifies only remaining exhaustive four-provider matches; no adapter factory is missing Agy.

- [ ] **Step 3: Complete four-provider routing matches**

In `orchestrator-policy/src/routing.rs`, add `ProviderId::Agy` to every `role_affinity` match with the same initial role score as Gemini. This avoids inventing a provider-quality distinction while default priority 80 still places Agy ahead of Gemini when all other route evidence is equal:

```rust
ProviderId::Gemini | ProviderId::Agy => 15.0,
```

Use the existing Gemini numeric value in each of the six role branches (`15.0`, `18.0`, `15.0`, `18.0`, `25.0`, and `18.0`). Add a routing test with otherwise identical Agy and Gemini candidates and assert the configured Agy priority breaks the tie without combining usage snapshots.

Run: `cargo check --workspace --all-targets --all-features`

Expected: PASS.

- [ ] **Step 4: Run focused CLI and engine integration tests**

Run: `cargo test -p orchestrator-cli --all-features && cargo test -p orchestrator-engine --all-features && cargo test -p orchestrator-daemon --all-features && cargo test -p orchestrator-policy --all-features && cargo test -p orchestrator-state --all-features`

Expected: PASS.

- [ ] **Step 5: Commit application integration**

```text
git add crates/orchestrator-cli crates/orchestrator-policy crates/orchestrator-engine crates/orchestrator-daemon crates/orchestrator-state
git commit -m "feat: integrate agy across colay"
```

---

### Task 6: Documentation and CI safety contract

**Files:**
- Modify: `README.md`
- Modify: `config.example.toml`
- Modify: `docs/architecture.md`
- Modify: `docs/testing.md`
- Modify: `docs/operations.md`
- Modify: `docs/release.md`
- Modify: `.github/workflows/ci.yml`
- Modify: `.github/workflows/release.yml`
- Modify: `.github/workflows/codex-main-compat.yml`
- Modify: `.github/workflows/codex-release-compat.yml`

**Interfaces:**
- Produces: user-facing installation/configuration/profile documentation and an explicit no-real-Agy-inference test contract.

- [ ] **Step 1: Add failing documentation contract checks**

Extend the existing release/documentation contract test, or add a Rust test beside it, to assert that README contains `Agy`, the profile table contains `gemini-3.5-flash-low`, example parallel limits contain `agy = 1`, and testing docs explicitly forbid real Agy inference.

Extend `app.rs::tests::shipped_docs_describe_profile_management_and_current_presets` with:

```rust
for required in ["Agy", "gemini-3.5-flash-low"] {
    assert!(readme.contains(required), "README is missing {required}");
}
assert!(example.contains("agy = 1"));
```

Extend `scripts/release/test/license-contract.test.mjs` with:

```javascript
assert.match(testingGuide, /never invoke real Codex, Claude, Gemini, or Agy inference/);
```

Run: `cargo test -p orchestrator-cli shipped_docs_describe_profile_management_and_current_presets --lib -- --exact && node --test scripts/release/test/license-contract.test.mjs`

Expected: FAIL because the documents describe only three providers.

- [ ] **Step 2: Update user and operator documentation**

Describe Agy as independent from retained Gemini CLI support. Add the three Agy profiles, priority 80, `[orchestrator.providers.agy]`, and `provider_parallel_limits.agy`. Document that Agy uses bounded plain text and unknown usage unless an approved probe/manual override exists.

Update CI wording and credential-empty environment blocks where they enumerate providers; do not add credentials, live probes, or inference commands.

Run: `cargo test -p orchestrator-cli shipped_docs_describe_profile_management_and_current_presets --lib -- --exact && node --test scripts/release/test/license-contract.test.mjs`

Expected: PASS.

- [ ] **Step 3: Commit documentation and policy wording**

```text
git add README.md config.example.toml docs .github/workflows
git commit -m "docs: document agy provider support"
```

---

### Task 7: Full verification and branch handoff

**Files:**
- Verify: all files committed by Tasks 1-6; this task introduces no new behavior

**Interfaces:**
- Produces: formatted, warning-free, fully tested implementation ready for user-selected integration.

- [ ] **Step 1: Format and verify formatting**

Run: `cargo fmt --all`

Run: `cargo fmt --all -- --check`

Expected: exit 0.

- [ ] **Step 2: Run strict Clippy**

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: exit 0 with no warnings.

- [ ] **Step 3: Run the complete workspace test suite**

Run: `cargo test --workspace --all-features`

Expected: exit 0 with zero failed tests and no real provider inference.

- [ ] **Step 4: Audit scope and security invariants**

Run:

```text
git diff --check main...HEAD
git status --short
rg -n "dangerously-skip-permissions|GEMINI_API_KEY|usage.*scrap|shell.*-c" crates docs .github
```

Expected: no whitespace errors, only intended tracked changes, no new dangerous Agy flag, no credential use, no usage scraping, and no shell interpolation.

- [ ] **Step 5: Resolve verification failures in the owning task**

If formatting, Clippy, tests, or the security audit fails, return to the Task 1-6 component that owns the failing file, add or correct its focused regression test first, amend that component with a new focused commit, and rerun Steps 1-4 in full. Do not introduce an unreviewed catch-all implementation change in this verification task.

- [ ] **Step 6: Use the finishing-development-branch workflow**

Invoke `superpowers:finishing-a-development-branch`, present its integration choices, and do not merge, push, or delete the worktree without explicit user direction.
