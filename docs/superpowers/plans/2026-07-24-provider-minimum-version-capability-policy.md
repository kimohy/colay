# Provider Minimum-Version and Capability Policy Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Allow Codex, Claude, Gemini, and Agy when a reviewed minimum version or required public capabilities establish eligibility, while preserving explicit negative evidence, read-only fallback, and fresh doctor diagnostics.

**Architecture:** Provider-specific crates parse public version/help/schema evidence into normalized compatibility facts. A provider-neutral policy evaluates read-only and writable operation classes, and the user-global state database caches assessments by executable fingerprint. Normal startup uses a bounded version probe and matching cache; doctor always refreshes all configured providers without inference.

**Tech Stack:** Rust 2024, semver, serde, SHA-256, provider fake binaries and committed public-output fixtures.

## Global Constraints

- Decision order is explicit missing capability, then minimum version, then sufficient observed capabilities.
- Codex minimum version is `0.144.5`.
- Claude, Gemini, and Agy receive a minimum only with a committed fixture-backed parser and reviewed floor; capability eligibility works without one.
- Read-only and writable eligibility are evaluated separately.
- Runtime compatibility suspicion fails only the current attempt and recommends doctor; no automatic downgrade or failover.
- Assessment keys contain provider kind, canonical executable path, file size, mtime, and reported version.
- Doctor refreshes every configured provider; normal startup performs bounded `--version` and cache lookup.
- Provider wire types remain inside compatibility/provider crates; `orchestrator-domain` remains vendor-neutral and I/O-free.
- No inference, credentials, usage scraping, unofficial endpoint, external telemetry, or shell interpolation.

---

### Task 1: Provider-neutral evidence and decision policy

**Files:**
- Create: `crates/orchestrator-policy/src/provider_compatibility.rs`
- Modify: `crates/orchestrator-policy/src/lib.rs`
- Test: `crates/orchestrator-policy/tests/provider_compatibility.rs`

**Interfaces:**
- Produces: `CapabilityEvidence::{Unknown, Advertised, Verified, Degraded, Missing}`.
- Produces: `OperationEligibility`, `CompatibilityDecision`, `ProviderCompatibilityPolicy::evaluate`.

- [ ] **Step 1: Write failing table-driven decision tests**

```rust
#[test]
fn explicit_missing_capability_overrides_acceptable_version() {
    let facts = facts("0.200.0", [("writable_sandbox", CapabilityEvidence::Missing)]);
    let decision = policy().evaluate(&facts);
    assert!(!decision.writable.allowed);
    assert_eq!(decision.writable.reason, EligibilityReason::ExplicitlyMissing);
}

#[test]
fn partial_capability_preserves_read_only_access() {
    let decision = policy().evaluate(&read_only_verified_writable_unknown());
    assert!(decision.read_only.allowed);
    assert!(!decision.writable.allowed);
}
```

- [ ] **Step 2: Run and confirm RED**

Run: `cargo test -p orchestrator-policy --test provider_compatibility -- --nocapture`

Expected: compatibility policy types do not exist.

- [ ] **Step 3: Implement the minimal pure decision algorithm**

Evaluate each operation class by: deny if any required capability is `Missing`; otherwise allow by version when parsed version is at or above the floor; otherwise allow by capability only when every requirement is `Advertised`, `Verified`, or an explicitly accepted `Degraded`; otherwise deny for insufficient evidence.

- [ ] **Step 4: Run and confirm GREEN**

Run: `cargo test -p orchestrator-policy --test provider_compatibility -- --nocapture`

Expected: all decision-table cases pass.

- [ ] **Step 5: Commit**

```text
git add crates/orchestrator-policy
git commit -m "feat: evaluate provider compatibility evidence"
```

### Task 2: Global assessment store

**Files:**
- Create: `migrations/0015_provider_compatibility_cache.sql`
- Modify: `crates/orchestrator-state/src/migrations.rs`
- Create: `crates/orchestrator-state/src/provider_compatibility.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Test: `crates/orchestrator-state/tests/provider_compatibility_cache.rs`

**Interfaces:**
- Produces: `ExecutableFingerprint`, `StoredCompatibilityAssessment` and `Database::{store_compatibility_assessment, load_compatibility_assessment}`.

- [ ] **Step 1: Write failing exact-fingerprint tests**

```rust
#[test]
fn cache_hit_requires_every_fingerprint_field() -> TestResult {
    let fixture = CacheFixture::new()?;
    fixture.store(fixture.fingerprint())?;
    assert!(fixture.load(fixture.fingerprint())?.is_some());
    assert!(fixture.load(fixture.fingerprint().with_size(99))?.is_none());
    assert!(fixture.load(fixture.fingerprint().with_version("0.200.1"))?.is_none());
    Ok(())
}
```

- [ ] **Step 2: Run and confirm RED**

Run: `cargo test -p orchestrator-state --test provider_compatibility_cache -- --nocapture`

Expected: assessment store API does not exist.

- [ ] **Step 3: Add schema and storage implementation**

Migration 15 creates one user-global assessment table with a composite primary key over provider, canonical path, size, mtime, reported version, and probe contract revision. Store normalized facts and decisions as versioned JSON plus assessment time; never add `workspace_id`.

- [ ] **Step 4: Run and confirm GREEN**

Run: `cargo test -p orchestrator-state --test provider_compatibility_cache -- --nocapture`

Expected: exact hit, changed-file miss, and replacement cases pass.

- [ ] **Step 5: Commit**

```text
git add migrations/0015_provider_compatibility_cache.sql crates/orchestrator-state
git commit -m "feat: cache global provider assessments"
```

### Task 3: Fixture-backed provider probes and manifests

**Files:**
- Modify: `crates/codex-compat/src/adapter.rs`
- Modify: `crates/codex-compat/tests/contracts.rs`
- Modify: `crates/codex-compat/fixtures/compatibility/codex-matrix.json`
- Create: `crates/orchestrator-providers/src/compatibility.rs`
- Modify: `crates/orchestrator-providers/src/codex.rs`
- Modify: `crates/orchestrator-providers/src/claude.rs`
- Modify: `crates/orchestrator-providers/src/gemini.rs`
- Modify: `crates/orchestrator-providers/src/agy.rs`
- Modify: `crates/orchestrator-providers/src/lib.rs`
- Test: `crates/orchestrator-providers/tests/provider_compatibility.rs`

**Interfaces:**
- Produces: normalized `ProviderCompatibilityFacts` consumed by the policy crate.
- Produces: Codex manifest floor `0.144.5`; other floors remain `None` until their fixture parser qualifies.

- [ ] **Step 1: Write failing fake-output parser tests**

Add cases for Codex exact/future versions, explicit missing writable schema, Claude/Gemini/Agy capability-only eligibility, malformed version output as unknown, and non-zero help output as unknown rather than missing.

- [ ] **Step 2: Run and confirm RED**

Run: `cargo test -p codex-compat -p orchestrator-providers --all-features -- --nocapture`

Expected: new normalized evidence assertions fail against current exact-version classification.

- [ ] **Step 3: Implement manifests and safe probes**

Use `Command` with separated executable/args. Parse only successful documented public output. A probe failure yields `Unknown`; only recognized successful output may yield `Missing`. Keep all provider wire/output structures in `codex-compat` or `orchestrator-providers`.

- [ ] **Step 4: Run and confirm GREEN**

Run: `cargo test -p codex-compat -p orchestrator-providers --all-features -- --nocapture`

Expected: parser, manifest, and fake process tests pass without inference.

- [ ] **Step 5: Commit**

```text
git add crates/codex-compat crates/orchestrator-providers
git commit -m "feat: probe provider compatibility capabilities"
```

### Task 4: Startup cache and deep doctor integration

**Files:**
- Modify: `crates/orchestrator-engine/src/startup.rs`
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/daemon.rs`
- Modify: `crates/orchestrator-providers/src/official.rs`
- Test: `crates/orchestrator-cli/tests/provider_compatibility_policy.rs`

**Interfaces:**
- Normal startup consumes exact fingerprint cache after bounded version probe.
- Doctor requests fresh deep facts for every configured provider and persists through daemon IPC.

- [ ] **Step 1: Write failing startup/doctor behavior tests**

```rust
#[test]
fn future_codex_above_floor_allows_writable_when_no_capability_is_missing() -> Result<()> {
    let fixture = CompatibilityFixture::codex_version("0.200.0")?;
    let report = fixture.doctor()?;
    assert_eq!(report.codex.status, "compatible_by_version");
    assert!(report.codex.writable);
    Ok(())
}
```

- [ ] **Step 2: Run and confirm RED**

Run: `cargo test -p colay --test provider_compatibility_policy --features test-fixtures -- --nocapture`

Expected: future version remains `untested` and writable is disabled.

- [ ] **Step 3: Wire policy, cache, and doctor**

At startup fingerprint the resolved executable, run bounded version, reuse only an exact cache hit, and skip deep help/schema probes on a hit. Default doctor bypasses cache for all configured providers, stores fresh assessments, and reports evidence source plus separate read-only/writable decisions. Keep `compatibility` as the provider doctor alias.

- [ ] **Step 4: Run integration tests and confirm GREEN**

Run: `cargo test -p colay --test provider_compatibility_policy --features test-fixtures -- --nocapture`

Expected: by-version, by-capability, read-only, explicit-missing, cache-hit, fingerprint-miss, and doctor-refresh cases pass.

- [ ] **Step 5: Commit**

```text
git add crates/orchestrator-engine crates/orchestrator-cli crates/orchestrator-providers
git commit -m "feat: apply hybrid provider compatibility policy"
```

### Task 5: Runtime drift handling and complete verification

**Files:**
- Modify: `crates/orchestrator-daemon/src/execution.rs`
- Modify: `crates/orchestrator-engine/src/execution.rs`
- Modify: `docs/compatibility.md`
- Modify: `docs/operations.md`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Test: `crates/orchestrator-test-support/tests/provider_e2e.rs`

**Interfaces:**
- Produces: compatibility-suspected attempt failure with retained task and doctor next action.

- [ ] **Step 1: Add failing runtime-drift regression**

The fake provider succeeds during probing, then emits a redacted unsupported-schema terminal error during one worker attempt. Assert the attempt fails, task remains resumable, provider routing is unchanged, and output recommends `colay doctor providers`.

- [ ] **Step 2: Run and confirm RED**

Run: `cargo test -p orchestrator-test-support --test provider_e2e --all-features -- --nocapture`

Expected: current error classification lacks the retained-task doctor guidance.

- [ ] **Step 3: Implement bounded current-attempt failure**

Map recognized compatibility-drift evidence to the compatibility error category after redaction, terminate/cancel only the current provider process, close its attempt, retain task recovery state, and append an audit event. Do not mutate configured provider, minimum floor, routing, or cache from the runtime error.

- [ ] **Step 4: Run repository verification gates**

Run: `cargo fmt --all -- --check`

Expected: exit 0.

Run: `cargo clippy --workspace --all-targets --all-features -- -D warnings`

Expected: exit 0 with no warnings.

Run: `cargo test --workspace --all-features`

Expected: exit 0; all provider executions are fake fixtures.

- [ ] **Step 5: Update docs/tracker and commit**

```text
git add crates/orchestrator-daemon crates/orchestrator-engine crates/orchestrator-test-support docs
git commit -m "fix: contain provider compatibility drift"
```

