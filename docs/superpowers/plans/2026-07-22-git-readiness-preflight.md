# Git Readiness Preflight Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Detect non-Git directories and unborn `HEAD` before direct-task state mutation, return actionable typed diagnostics, and expose the same sealed repository evidence for conversation-first proposal validation.

**Architecture:** Add a read-only Git readiness inspection function to `orchestrator-engine` beside the worktree manager. Direct `colay run` calls it before opening or creating repository state unless `--plan-only` is explicitly selected; worktree creation reuses the returned checks instead of rediscovering readiness after task persistence.

**Tech Stack:** Rust 2024, Tokio, existing `ProcessRunner`, real local Git only for Git boundary tests, and `orchestrator-test-support` fake providers for CLI tests.

## Global Constraints

- Provider wire types remain outside `orchestrator-domain`; the new readiness result is vendor-neutral.
- Every Git process uses `CommandSpec` with separated executable and arguments.
- `run --plan-only` retains its current static assessment compatibility behavior.
- Non-Git and unborn repositories must create no `.colay` state and no task rows for a normal direct run.
- Tests never invoke real Codex, Claude, Gemini, or Agy inference.
- No worktree is created before explicit graph approval in the conversation-first flow.

---

### Task 1: Typed read-only Git readiness inspection

**Files:**
- Modify: `crates/orchestrator-engine/src/error.rs`
- Modify: `crates/orchestrator-engine/src/worktree.rs`
- Modify: `crates/orchestrator-engine/src/lib.rs`

**Interfaces:**
- Produces: `GitRepositoryReadiness { repository_root: PathBuf, base_commit: String }`.
- Produces: `pub async fn inspect_git_repository(path: &Path) -> EngineResult<GitRepositoryReadiness>`.
- Produces: `EngineError::NotGitRepository(PathBuf)` and `EngineError::MissingGitBaseCommit(PathBuf)`.

- [ ] **Step 1: Write failing worktree-module tests**

Add three Tokio tests that use separated `git` arguments:

```rust
#[tokio::test]
async fn readiness_rejects_non_git_directory_without_creating_state() -> TestResult {
    let repository = CanonicalTempDir::new()?;
    let error = inspect_git_repository(repository.path()).await.unwrap_err();
    assert!(matches!(error, EngineError::NotGitRepository(ref path) if path == repository.path()));
    assert!(!repository.path().join(".colay").exists());
    Ok(())
}

#[tokio::test]
async fn readiness_distinguishes_unborn_head() -> TestResult {
    let repository = CanonicalTempDir::new()?;
    run_git(repository.path(), &["init", "--quiet"])?;
    let error = inspect_git_repository(repository.path()).await.unwrap_err();
    assert!(matches!(error, EngineError::MissingGitBaseCommit(ref path) if path == repository.path()));
    Ok(())
}

#[tokio::test]
async fn readiness_seals_root_and_full_head_commit() -> TestResult {
    let repository = initialized_repository()?;
    let expected = git_output(repository.path(), &["rev-parse", "HEAD"])?;
    let readiness = inspect_git_repository(repository.path()).await?;
    assert_eq!(readiness.repository_root, repository.path());
    assert_eq!(readiness.base_commit, expected.trim());
    Ok(())
}
```

- [ ] **Step 2: Run the tests and verify RED**

Run:

```text
cargo test -p orchestrator-engine worktree::tests::readiness_ --all-features
```

Expected: compilation fails because `inspect_git_repository`, `GitRepositoryReadiness`, and the two typed errors do not exist.

- [ ] **Step 3: Implement the minimal readiness contract**

Add typed errors:

```rust
#[error("direct task execution requires a Git repository: {0}")]
NotGitRepository(PathBuf),
#[error("Git repository has no base commit; create an initial commit before task execution: {0}")]
MissingGitBaseCommit(PathBuf),
```

Add the public result and inspection function. It canonicalizes the input, runs `git rev-parse --show-toplevel`, then `git rev-parse --verify HEAD^{commit}`, validates the full hexadecimal object ID, and runs the existing unresolved-operation boundary check. A failed first command maps to `NotGitRepository`; a successful root probe followed by a failed `HEAD^{commit}` maps to `MissingGitBaseCommit`. Process-spawn, timeout, and output-bound failures retain their existing typed process errors.

```rust
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitRepositoryReadiness {
    pub repository_root: PathBuf,
    pub base_commit: String,
}

pub async fn inspect_git_repository(path: &Path) -> EngineResult<GitRepositoryReadiness> {
    let requested = canonicalize_directory(path)?;
    let runner = ProcessRunner;
    let root = git_text_for_readiness(&runner, &requested, ["rev-parse", "--show-toplevel"])
        .await
        .map_err(|error| map_repository_probe(error, &requested))?;
    let repository_root = canonicalize_directory(Path::new(&root))?;
    let base_commit = git_text_for_readiness(
        &runner,
        &repository_root,
        ["rev-parse", "--verify", "HEAD^{commit}"],
    )
    .await
    .map_err(|error| map_head_probe(error, &repository_root))?;
    validate_object_id(&base_commit)?;
    assert_safe_boundary(&runner, &repository_root).await?;
    Ok(GitRepositoryReadiness { repository_root, base_commit })
}
```

- [ ] **Step 4: Reuse inspection in worktree creation**

Before `worktree add`, call `inspect_git_repository(&self.repository_root)` and resolve the requested base against its sealed repository root. Keep support for an explicitly supplied safe full object ID; for `HEAD`, use `readiness.base_commit`. Do not create directories in the inspection function.

- [ ] **Step 5: Run focused tests and verify GREEN**

Run:

```text
cargo test -p orchestrator-engine worktree::tests --all-features
```

Expected: all worktree tests pass, including the three readiness cases.

- [ ] **Step 6: Commit**

```text
git add crates/orchestrator-engine/src/error.rs crates/orchestrator-engine/src/worktree.rs crates/orchestrator-engine/src/lib.rs
git commit -m "fix: add typed Git readiness preflight"
```

### Task 2: Preflight direct execution before state mutation

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/tests/default_startup.rs`

**Interfaces:**
- Consumes: `inspect_git_repository(&Path)` from Task 1.
- Preserves: `run --plan-only` static task assessment compatibility.

- [ ] **Step 1: Write failing CLI regression tests**

Extend the fixture with a host PATH plus `COLAY_TEST_FAKE_PROVIDERS_ONLY=1`, configure the compiled fake Codex executable, and add:

```rust
#[test]
fn direct_run_rejects_non_git_before_state_mutation() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_fake_codex()?;
    let output = fixture.colay(["run", "hello"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires a Git repository"));
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}

#[test]
fn direct_run_rejects_unborn_head_before_state_mutation() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.git(["init", "--quiet"])?;
    fixture.configure_fake_codex()?;
    let output = fixture.colay(["run", "hello"])?;
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("no base commit"));
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}
```

- [ ] **Step 2: Run the CLI tests and verify RED**

Run:

```text
cargo test -p colay --test default_startup direct_run_rejects --all-features
```

Expected: both tests fail because `.colay` and a planned task are created before raw Git failure.

- [ ] **Step 3: Add the pre-mutation gate**

At the beginning of `run_task`, after the enabled check but before `StatePaths` or database initialization, add:

```rust
if !arguments.plan_only {
    inspect_git_repository(repository).await.with_context(|| {
        "direct `colay run` executes a writable task; use conversation-first chat for questions or prepare a committed Git repository"
    })?;
}
```

Do not call this gate for `run --plan-only`; its compatibility contract remains unchanged.

- [ ] **Step 4: Run focused CLI tests and verify GREEN**

Run:

```text
cargo test -p colay --test default_startup --all-features
```

Expected: all default startup tests pass; the two new tests show zero state mutation.

- [ ] **Step 5: Commit**

```text
git add crates/orchestrator-cli/src/app.rs crates/orchestrator-cli/tests/default_startup.rs
git commit -m "fix: preflight Git before direct task persistence"
```

### Task 3: Document command boundary and close the two late-failure regressions

**Files:**
- Modify: `README.md`
- Modify: `docs/operations.md`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

**Interfaces:**
- Documents: `run` is direct writable execution, `run --plan-only` is static compatibility assessment, and conversation-first chat is the pre-task question/interview surface.

- [ ] **Step 1: Update command and recovery documentation**

Document that a normal direct run requires a Git worktree and valid base commit before state initialization. Document the two actionable remedies without recommending `git add .`: move to the intended repository, or create a reviewed initial commit containing only intended files.

- [ ] **Step 2: Update tracker statuses**

Set `WSL-004` and `WSL-005` to `fixed` only after both focused regressions and the Windows-compatible full suite pass. Preserve their original evidence and append the fix commit plus completion evidence.

- [ ] **Step 3: Run required verification**

Run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all commands exit 0 and no test invokes a real provider.

- [ ] **Step 4: Commit**

```text
git add README.md docs/operations.md docs/qa/wsl-nightly-error-tracker.md
git commit -m "docs: explain Git readiness boundary"
```

## Plan Self-Review

- Spec coverage: this plan covers the Git readiness gate and the direct-run late-failure regressions; conversation interview persistence, SQLite polling, daemon cleanup, and renewable lease work remain separate plans.
- Placeholder scan: the plan contains no deferred implementation placeholders.
- Type consistency: Tasks 1 and 2 use the same `inspect_git_repository(&Path) -> EngineResult<GitRepositoryReadiness>` interface and the same typed error variants.
