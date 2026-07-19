# Layered Configuration, Windows Resolution, and Codex 0.144.6 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make Colay run from safe compiled defaults with optional global and repository overrides, resolve Windows provider executables deterministically, and recognize Codex CLI 0.144.6 as the tested recommended contract.

**Architecture:** `orchestrator-state` owns typed defaults, versioned partial configuration layers, deterministic TOML merging, and source-preserving override writes. `orchestrator-process` owns a single injectable executable resolver used by every subprocess and rollback lookup. The CLI composes those services, lazily initializes repository state only for a new run, and exposes source/resolution evidence through non-inference diagnostics.

**Tech Stack:** Rust 2024, `toml_edit`, `serde`, `tokio::process`, `rusqlite`, `tempfile`, committed Codex compatibility fixtures, and `orchestrator-test-support` fake provider binaries.

## Global Constraints

- Preserve configuration schema version 4 and existing backup-first sequential migration behavior.
- Keep SQLite as the system of record and JSONL audit output append-only and hash-chained.
- Keep `orchestrator-domain` vendor-neutral and I/O-free.
- Never add identity rotation, quota bypass, usage-page scraping, credential extraction, credit purchasing, unofficial endpoints, or default external telemetry.
- Missing usage remains unknown; never compare raw provider quota units.
- Provider processes receive separated executable and argv values; never construct interpolated shell command strings.
- Tests and CI use `orchestrator-test-support` fake binaries and committed fixtures; they never invoke real Codex, Claude, or Gemini inference.
- Writable workers remain isolated in worktrees; reviewers remain read-only; do not auto-merge, push, or delete worktrees.
- Final verification must pass `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

---

## File Structure

- Create `crates/orchestrator-state/src/config_layers.rs` for layer discovery inputs, source metadata, partial-document validation, recursive merge, and effective configuration loading.
- Modify `crates/orchestrator-state/src/config.rs` to provide a complete safe `Default` implementation and reusable document validation helpers.
- Modify `crates/orchestrator-state/src/lib.rs` to export the new configuration-layer API.
- Modify `config.example.toml` so initialization materializes only a versioned, commented override example.
- Create `crates/orchestrator-process/src/executable.rs` for platform-injected executable resolution.
- Modify `crates/orchestrator-process/src/runner.rs` so every command resolves once before spawn and reports the selected path.
- Modify `crates/orchestrator-process/src/lib.rs` to export resolver evidence and errors.
- Modify `crates/orchestrator-cli/src/app.rs` to load effective configuration, write only explicit override sources, lazily initialize new run state, report resolver evidence, and remove the duplicate rollback locator.
- Modify `crates/orchestrator-cli/src/args.rs` to document `COLAY_HOME`, `COLAY_CONFIG`, repository overrides, and CLI precedence.
- Create `fixtures/codex/versions/0.144.6/*` and update the catalog, matrix, registry, contract tests, and release documentation.
- Modify `README.md`, `docs/operations.md`, `docs/security.md`, `docs/compatibility.md`, and `docs/release.md` to document final behavior.

---

### Task 1: Safe Typed Defaults and Versioned Partial Configuration Layers

**Files:**
- Create: `crates/orchestrator-state/src/config_layers.rs`
- Modify: `crates/orchestrator-state/src/config.rs`
- Modify: `crates/orchestrator-state/src/lib.rs`
- Test: `crates/orchestrator-state/src/config_layers.rs`

**Interfaces:**
- Consumes: `ConfigDocument::parse`, `RootConfig`, `CONFIG_SCHEMA_VERSION`, `reject_symlink_components`.
- Produces: `ConfigEnvironment`, `ConfigLayerKind`, `ConfigSource`, `ConfigRequest`, `EffectiveConfig`, and `load_effective_config(&ConfigRequest) -> StateResult<EffectiveConfig>`.

- [ ] **Step 1: Add failing tests for complete defaults and merge semantics**

Add tests that express the public API before implementing it:

```rust
#[test]
fn empty_layers_use_complete_safe_defaults() {
    let repository = tempfile::tempdir().unwrap();
    let request = ConfigRequest::new(repository.path(), ConfigEnvironment::isolated());
    let effective = load_effective_config(&request).unwrap();

    assert_eq!(effective.config().config_version, CONFIG_SCHEMA_VERSION);
    assert_eq!(effective.config().orchestrator.state_dir, PathBuf::from(".colay"));
    assert_eq!(effective.config().orchestrator.timezone, "UTC");
    assert!(effective.config().orchestrator.providers.codex.is_some());
    assert!(effective.sources().is_empty());
}

#[test]
fn layers_merge_in_precedence_order_and_arrays_replace() {
    let fixture = LayerFixture::new();
    fixture.write_global(
        "config_version = 4\n[orchestrator]\nmax_parallel_workers = 2\n\
         [orchestrator.redaction]\npatterns = [\"GLOBAL-[0-9]+\"]\n",
    );
    fixture.write_repository(
        "config_version = 4\n[orchestrator]\nmax_parallel_workers = 3\n\
         [orchestrator.redaction]\npatterns = [\"REPO-[0-9]+\"]\n",
    );
    fixture.write_environment(
        "config_version = 4\n[orchestrator]\ndefault_timeout_minutes = 45\n",
    );
    fixture.write_cli(
        "config_version = 4\n[orchestrator]\nmax_parallel_workers = 4\n",
    );

    let effective = load_effective_config(&fixture.request()).unwrap();
    assert_eq!(effective.config().orchestrator.max_parallel_workers, 4);
    assert_eq!(effective.config().orchestrator.default_timeout_minutes, 45);
    assert_eq!(
        effective.config().orchestrator.redaction.patterns,
        ["REPO-[0-9]+"]
    );
    assert_eq!(
        effective.sources().iter().map(|source| source.kind).collect::<Vec<_>>(),
        [
            ConfigLayerKind::Global,
            ConfigLayerKind::Repository,
            ConfigLayerKind::Environment,
            ConfigLayerKind::Cli,
        ]
    );
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```text
cargo test -p orchestrator-state config_layers --all-features
```

Expected: compilation fails because `ConfigRequest`, `ConfigEnvironment`, and `load_effective_config` do not exist.

- [ ] **Step 3: Implement complete typed defaults**

Add `Default` implementations in `config.rs` using the existing conservative values. The provider factories must return these exact identities and quota periods:

```rust
impl Default for RootConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_SCHEMA_VERSION,
            orchestrator: OrchestratorConfig::default(),
            features: FeatureConfig::default(),
        }
    }
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            automatic_routing: true,
            state_dir: PathBuf::from(".colay"),
            timezone: "UTC".to_owned(),
            max_parallel_workers: 1,
            default_timeout_minutes: 30,
            max_retries: 1,
            warning_threshold_percent: 30.0,
            handover_threshold_percent: 15.0,
            critical_reserve_percent: 15.0,
            require_review_from_difficulty: 7,
            minimum_progress: 0.05,
            daily_grace_minutes: 60,
            monthly_grace_minutes: 1_440,
            forecast_alpha: 0.3,
            minimum_forecast_observations: 3,
            providers: ProviderConfigs::default(),
            model_profiles: BTreeMap::new(),
            redaction: RedactionSettings::default(),
        }
    }
}
```

Implement provider defaults with `quota_limit`, calibration values, rolling/custom timestamps, and quota scope left `None`; UTC reset zones; priorities Gemini 70, Codex 100, Claude 90; and `UsageProbeConfig::ManualOrLedger`.

- [ ] **Step 4: Implement layer discovery and deterministic document merging**

Create these exact public types in `config_layers.rs`:

```rust
#[derive(Clone, Debug, Default)]
pub struct ConfigEnvironment {
    pub colay_home: Option<PathBuf>,
    pub user_home: Option<PathBuf>,
    pub colay_config: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigLayerKind { Global, Repository, Environment, Cli, LegacyRepository }

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfigSource { pub kind: ConfigLayerKind, pub path: PathBuf }

pub struct ConfigRequest<'a> {
    pub repository: &'a Path,
    pub cli_config: Option<&'a Path>,
    pub environment: ConfigEnvironment,
}

pub struct EffectiveConfig {
    document: ConfigDocument,
    sources: Vec<ConfigSource>,
    repository_override: PathBuf,
}

impl ConfigEnvironment {
    pub const fn isolated() -> Self {
        Self { colay_home: None, user_home: None, colay_config: None }
    }
}

impl<'a> ConfigRequest<'a> {
    pub const fn new(repository: &'a Path, environment: ConfigEnvironment) -> Self {
        Self { repository, cli_config: None, environment }
    }
}
```

`load_effective_config` must serialize `RootConfig::default()` into a base `DocumentMut`, discover optional global/repository files, require environment/CLI paths when named, validate every source `config_version`, recursively merge tables, replace arrays/scalars, parse the final document through `ConfigDocument::parse`, and return ordered source evidence. Add `config()`, `document()`, `sources()`, and `repository_override()` accessors.

The test module must define `LayerFixture` with a `TempDir` plus stored global,
repository, environment, and CLI paths. Its four `write_*` methods create the
parent directory and call `fs::write`; `request(&self)` returns a
`ConfigRequest` whose `ConfigEnvironment` points at the stored global home and
environment file and whose `cli_config` borrows the stored CLI path. This keeps
every path alive for the duration of `load_effective_config`.

- [ ] **Step 5: Add fail-closed layer tests**

Add tests for a missing explicit path, invalid TOML in an optional file that exists, a missing `config_version`, a future version, and simultaneous current/legacy repository files:

```rust
#[test]
fn explicit_missing_config_is_an_error() {
    let repository = tempfile::tempdir().unwrap();
    let missing = repository.path().join("missing.toml");
    let request = ConfigRequest {
        repository: repository.path(),
        cli_config: Some(&missing),
        environment: ConfigEnvironment::isolated(),
    };
    assert!(load_effective_config(&request).unwrap_err().to_string().contains("cli"));
}

#[test]
fn future_layer_schema_fails_closed() {
    let fixture = LayerFixture::new();
    fixture.write_global("config_version = 999\n");
    let error = load_effective_config(&fixture.request()).unwrap_err().to_string();
    assert!(error.contains("global"));
    assert!(error.contains("999"));
}
```

- [ ] **Step 6: Run focused and crate tests and verify GREEN**

Run:

```text
cargo test -p orchestrator-state config_layers --all-features
cargo test -p orchestrator-state --all-features
```

Expected: all `orchestrator-state` tests pass and no test modifies the real user environment.

- [ ] **Step 7: Commit Task 1**

```text
git add crates/orchestrator-state/src/config.rs crates/orchestrator-state/src/config_layers.rs crates/orchestrator-state/src/lib.rs
git commit -m "feat: add layered configuration defaults"
```

---

### Task 2: CLI Configuration Composition and Minimal Override Writes

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Modify: `crates/orchestrator-cli/src/args.rs`
- Modify: `config.example.toml`
- Test: `crates/orchestrator-cli/src/app.rs`

**Interfaces:**
- Consumes: `ConfigRequest`, `ConfigEnvironment`, `EffectiveConfig`, `load_effective_config` from Task 1.
- Produces: one `ConfigRuntime` per CLI invocation and source-aware administrative override writes.

- [ ] **Step 1: Add failing CLI selection tests**

Replace path-only selection tests with tests for injected environment and precedence:

```rust
#[test]
fn cli_config_is_the_highest_layer() -> Result<()> {
    let root = tempfile::tempdir()?;
    let global = root.path().join("home/config.toml");
    let environment = root.path().join("environment.toml");
    let cli = root.path().join("cli.toml");
    write_layer(&global, 2)?;
    write_layer(&root.path().join(".colay/config.toml"), 3)?;
    write_layer(&environment, 4)?;
    write_layer(&cli, 5)?;

    let runtime = load_config_runtime(
        root.path(),
        Some(&cli),
        ConfigEnvironment {
            colay_home: Some(root.path().join("home")),
            user_home: None,
            colay_config: Some(environment),
        },
    )?;
    assert_eq!(runtime.effective.config().orchestrator.max_parallel_workers, 5);
    Ok(())
}

fn write_layer(path: &Path, workers: u32) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(
        path,
        format!("config_version = 4\n[orchestrator]\nmax_parallel_workers = {workers}\n"),
    )?;
    Ok(())
}
```

- [ ] **Step 2: Run the focused CLI test and verify RED**

Run:

```text
cargo test -p colay cli_config_is_the_highest_layer --all-features
```

Expected: compilation fails because `load_config_runtime` and `ConfigRuntime` do not exist.

- [ ] **Step 3: Load the configuration stack once per invocation**

Create this private CLI composition type:

```rust
struct ConfigRuntime {
    effective: EffectiveConfig,
    explicit_edit_path: PathBuf,
}
```

Build `ConfigEnvironment` from `COLAY_HOME`, `COLAY_CONFIG`, and `USERPROFILE` on Windows or `HOME` on Unix. `run` must resolve the repository first, load `ConfigRuntime` once, and pass `&EffectiveConfig` to read-only/worker functions instead of repeatedly calling `ConfigDocument::load`. Migration and administrative edit functions must receive `explicit_edit_path`, defined as CLI path, else `COLAY_CONFIG`, else the repository override path.

- [ ] **Step 4: Implement minimal versioned override writes**

Replace the full `CONFIG_TEMPLATE` with this minimal persisted document:

```toml
config_version = 4

# Add only values that should override Colay's compiled or user-wide defaults.
# [orchestrator]
# timezone = "Asia/Seoul"
# max_parallel_workers = 2

# [orchestrator.providers.codex]
# executable = "C:\\path\\to\\codex.exe"
```

When `providers enable/disable` targets a missing repository override, parse the minimal document as `DocumentMut`, create `[orchestrator.providers.<name>]`, set only `enabled`, write atomically with the existing private-file protections, then reload the complete effective stack before reporting success. An explicit environment/CLI config is edited instead of creating a repository override.

- [ ] **Step 5: Add tests for minimal init and override preservation**

Add tests that assert `init` output does not contain `quota_limit`, `warning_threshold_percent`, or provider credentials; provider enable adds only the requested boolean; and global comments remain unchanged after a repository edit.

- [ ] **Step 6: Run CLI unit tests and verify GREEN**

Run:

```text
cargo test -p colay --lib --all-features
```

Expected: all CLI unit tests pass and the existing legacy/current conflict test remains green.

- [ ] **Step 7: Commit Task 2**

```text
git add crates/orchestrator-cli/src/app.rs crates/orchestrator-cli/src/args.rs config.example.toml
git commit -m "feat: compose global and repository configuration"
```

---

### Task 3: Safe Lazy Repository State Initialization

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs`
- Create: `crates/orchestrator-cli/tests/default_startup.rs`
- Test: `crates/orchestrator-cli/tests/default_startup.rs`

**Interfaces:**
- Consumes: `EffectiveConfig` from Task 1 and existing `Database`, `EventLog`, and `StatePaths` APIs.
- Produces: `initialize_repository_state(&StatePaths) -> Result<Database>` and non-mutating empty-state behavior for read-only commands.

- [ ] **Step 1: Add failing integration tests for no-config startup**

Use `assert_cmd`-free `std::process::Command` invocation of the compiled `colay` test binary, an isolated `COLAY_HOME`, and a temporary repository:

```rust
#[test]
fn doctor_uses_defaults_without_creating_repository_state() -> Result<()> {
    let fixture = CliFixture::new()?;
    let output = fixture.colay(["--json", "doctor"])?;
    assert!(output.status.success());
    assert!(!fixture.repository.join(".colay").exists());
    let json: serde_json::Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["data"]["checks"][0]["status"], "pass");
    Ok(())
}

#[test]
fn first_plan_only_run_initializes_local_state() -> Result<()> {
    let fixture = CliFixture::new()?;
    let output = fixture.colay(["run", "inspect repository", "--plan-only"])?;
    assert!(output.status.success());
    assert!(fixture.repository.join(".colay/orchestrator.db").is_file());
    assert!(fixture.repository.join(".colay/events.jsonl").is_file());
    Ok(())
}
```

The fixture must prepend only the compiled `colay-e2e-fake-provider` directory to its injected PATH and must not use a real provider name or binary.

Define the integration fixture with these concrete responsibilities:

```rust
struct CliFixture {
    _temp: tempfile::TempDir,
    repository: PathBuf,
    colay_home: PathBuf,
}

impl CliFixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let repository = temp.path().join("repository");
        let colay_home = temp.path().join("home/.colay");
        fs::create_dir_all(&repository)?;
        Ok(Self { _temp: temp, repository, colay_home })
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        Ok(Command::new(env!("CARGO_BIN_EXE_colay"))
            .args(args)
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("PATH", fake_provider_path())
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root())
            .output()?)
    }
}
```

`fake_provider_path()` returns the parent of
`env!("CARGO_BIN_EXE_colay-e2e-fake-provider")`; `system_root()` returns the
test process `SystemRoot` on Windows and `/` on Unix. The fixture never reads
the user's `COLAY_HOME`.

- [ ] **Step 2: Run the integration tests and verify RED**

Run:

```text
cargo test -p colay --test default_startup --features test-fixtures
```

Expected: `doctor` fails because no config file exists and `run` reports that `colay init` is required.

- [ ] **Step 3: Implement one idempotent state initializer**

Add:

```rust
fn initialize_repository_state(state: &StatePaths) -> Result<Database> {
    let database = Database::open(&state.database)?;
    database.migrate_with_backup(&state.backups)?;
    EventLog::open(&state.events)?.reconcile(&database)?;
    Ok(database)
}
```

`initialize` and a new run with no database must call this function. Existing-task commands (`resume`, `pause`, `cancel`, handover, checkpoint, rollback) must continue requiring existing state because creating an empty database cannot satisfy their requested task identity. `status` with no database emits an empty stable result without opening SQLite. `doctor` continues warning that state is absent and creates nothing.

- [ ] **Step 4: Add a no-write regression test for compatibility and status**

Invoke `compatibility` using the fake provider and `status` in a fresh repository, then assert `.colay` remains absent after both commands.

- [ ] **Step 5: Run integration and workspace-adjacent tests and verify GREEN**

Run:

```text
cargo test -p colay --test default_startup --features test-fixtures
cargo test -p colay -p orchestrator-state --all-features
```

Expected: all tests pass and only the run test creates repository state.

- [ ] **Step 6: Commit Task 3**

```text
git add crates/orchestrator-cli/src/app.rs crates/orchestrator-cli/tests/default_startup.rs
git commit -m "feat: initialize repository state on first run"
```

---

### Task 4: Central Cross-Platform Executable Resolver

**Files:**
- Create: `crates/orchestrator-process/src/executable.rs`
- Modify: `crates/orchestrator-process/src/lib.rs`
- Modify: `crates/orchestrator-process/src/runner.rs`
- Modify: `crates/orchestrator-cli/src/app.rs`
- Test: `crates/orchestrator-process/src/executable.rs`
- Test: `crates/orchestrator-test-support/tests/provider_e2e.rs`

**Interfaces:**
- Consumes: `CommandSpec.executable`, `CommandSpec.working_dir`, and the effective PATH/PATHEXT values from `EnvironmentPolicy`.
- Produces: `ExecutablePlatform`, `ExecutableSearch`, `ResolvedExecutable`, `ExecutableResolutionError`, and `resolve_executable`.

- [ ] **Step 1: Add failing pure resolver tests**

The tests must inject platform and PATH rather than mutate the process environment:

```rust
#[test]
fn windows_skips_extensionless_foreign_binary_and_selects_cmd() {
    let fixture = SearchFixture::new();
    fixture.write_bytes("first/codex", b"\x7fELF");
    fixture.write_bytes("second/codex.cmd", b"@echo off\r\n");
    let search = fixture.search(
        ExecutablePlatform::Windows,
        ".COM;.EXE;.BAT;.CMD",
        ["first", "second"],
    );

    let resolved = resolve_executable(Path::new("codex"), &search).unwrap();
    assert_eq!(resolved.path, fixture.path("second/codex.cmd"));
    assert_eq!(resolved.kind, ExecutableKind::CommandScript);
}

#[test]
fn explicit_missing_path_is_not_replaced_from_path() {
    let fixture = SearchFixture::new();
    fixture.write_bytes("bin/codex.exe", b"MZ");
    let explicit = fixture.path("missing/codex.exe");
    let error = resolve_executable(&explicit, &fixture.windows_search()).unwrap_err();
    assert!(matches!(error, ExecutableResolutionError::ExplicitMissing { .. }));
}
```

- [ ] **Step 2: Run resolver tests and verify RED**

Run:

```text
cargo test -p orchestrator-process executable --all-features
```

Expected: compilation fails because the executable resolver types do not exist.

- [ ] **Step 3: Implement deterministic resolution**

Create these public types:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExecutablePlatform { Windows, Unix }

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutableKind { Native, CommandScript }

pub struct ExecutableSearch {
    pub platform: ExecutablePlatform,
    pub path: Vec<PathBuf>,
    pub pathext: Vec<OsString>,
    pub working_directory: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResolvedExecutable {
    pub configured: PathBuf,
    pub path: PathBuf,
    pub kind: ExecutableKind,
}

pub fn resolve_executable(
    configured: &Path,
    search: &ExecutableSearch,
) -> Result<ResolvedExecutable, ExecutableResolutionError>;
```

The private `SearchFixture` used by the tests owns one `TempDir` and implements
`path`, `write_bytes`, `search`, and `windows_search`. `write_bytes` creates
parents and writes only test-controlled bytes. `search` maps the supplied
relative directory names under the owned temp directory and splits the supplied
PATHEXT string on `;`; no process-global PATH is changed.

Windows bare-name resolution must ignore the extensionless candidate, filter PATHEXT case-insensitively to `.exe`, `.com`, `.cmd`, and `.bat`, retain filtered order, and scan each PATH directory in order. Unix resolution must require a regular file with at least one executable mode bit. Explicit paths resolve against `working_directory`, require a regular file, and never fall back to PATH.

- [ ] **Step 4: Resolve once at the process boundary and expose evidence**

Add `resolved_executable: ResolvedExecutable` to `ProcessResult`. Before `Command::new`, derive search values from the command's `EnvironmentPolicy`, call `resolve_executable`, and spawn `resolved_executable.path`. Map resolution errors to a new `ProcessError::Resolve` variant. Keep the configured executable and every argument separate.

Update the non-Windows fake-provider tests to assert native resolution. Add a Windows-only fake `.cmd` wrapper test that forwards fixed test arguments to `fake-provider-cli.exe`; it must never embed user input in the script.

- [ ] **Step 5: Remove the duplicate rollback locator and report doctor evidence**

Delete `locate_configured_executable` from `app.rs`. Rollback destination lookup must call the exported resolver with the same environment/working-directory inputs as provider execution. Doctor provider data must become:

```rust
json!({
    "version": output.stdout.redacted_text.trim(),
    "configured_executable": output.resolved_executable.configured,
    "resolved_executable": output.resolved_executable.path,
    "executable_kind": output.resolved_executable.kind,
})
```

- [ ] **Step 6: Run resolver, provider, and CLI tests and verify GREEN**

Run:

```text
cargo test -p orchestrator-process --all-features
cargo test -p orchestrator-test-support --all-features
cargo test -p colay --all-features
```

Expected: all tests pass using fake binaries; on Windows, the extensionless regression test selects `.cmd` and no test invokes real Codex.

- [ ] **Step 7: Commit Task 4**

```text
git add crates/orchestrator-process/src/executable.rs crates/orchestrator-process/src/lib.rs crates/orchestrator-process/src/runner.rs crates/orchestrator-cli/src/app.rs crates/orchestrator-test-support/tests/provider_e2e.rs
git commit -m "fix: resolve Windows provider executables safely"
```

---

### Task 5: Codex CLI 0.144.6 Exact Compatibility Contract

**Files:**
- Create: `fixtures/codex/versions/0.144.6/manifest.toml`
- Create: `fixtures/codex/versions/0.144.6/version-output.txt`
- Create: `fixtures/codex/versions/0.144.6/root-help.txt`
- Create: `fixtures/codex/versions/0.144.6/exec-help.txt`
- Create: `fixtures/codex/versions/0.144.6/exec-resume-help.txt`
- Create: `fixtures/codex/versions/0.144.6/app-server-help.txt`
- Create: `fixtures/codex/versions/0.144.6/app-server-schema.json`
- Create: `fixtures/codex/versions/0.144.6/jsonl-success.jsonl`
- Create: `fixtures/codex/versions/0.144.6/jsonl-tool-call.jsonl`
- Create: `fixtures/codex/versions/0.144.6/jsonl-error.jsonl`
- Create: `fixtures/codex/versions/0.144.6/quota-error.jsonl`
- Create: `fixtures/codex/versions/0.144.6/resume-events.jsonl`
- Create: `fixtures/codex/versions/0.144.6/malformed-events.jsonl`
- Create: `fixtures/codex/versions/0.144.6/unknown-lifecycle.jsonl`
- Modify: `compatibility/codex-version.toml`
- Modify: `compatibility/codex-matrix.json`
- Modify: `crates/codex-compat/src/registry.rs`
- Modify: `crates/codex-compat/tests/contracts.rs`
- Modify: `docs/release.md`

**Interfaces:**
- Consumes: the existing fixture contract, matrix generator, and `CompatibilityRegistry`.
- Produces: exact supported adapters for 0.144.6 and 0.144.5, with 0.144.6 recommended and pinned to `5d1fbf26c43abc65a203928b2e31561cb039e06d`.

- [ ] **Step 1: Add failing 0.144.6 registry and catalog assertions**

Update the contract expectations before changing the registry:

```rust
#[test]
fn current_and_previous_codex_versions_are_exact() {
    let registry = CompatibilityRegistry::default();
    assert!(matches!(
        registry.select(Some(&Version::new(0, 144, 6))),
        AdapterSelection::Exact { .. }
    ));
    assert!(matches!(
        registry.select(Some(&Version::new(0, 144, 5))),
        AdapterSelection::Exact { .. }
    ));
    assert!(matches!(
        registry.select(Some(&Version::new(0, 144, 4))),
        AdapterSelection::GenericUntested
    ));
}
```

Change catalog assertions to recommended `0.144.6`, tested versions `0.144.6` and `0.144.5`, and the exact 40-character revision above.

- [ ] **Step 2: Run compatibility tests and verify RED**

Run:

```text
cargo test -p codex-compat --all-features
```

Expected: 0.144.6 selects `GenericUntested` and catalog expectations fail.

- [ ] **Step 3: Capture non-inference metadata fixtures and preserve protocol fixtures**

Use the installed 0.144.6 Windows native binary only for these commands:

```text
codex.exe --version
codex.exe --help
codex.exe exec --help
codex.exe exec resume --help
codex.exe app-server --help
codex.exe app-server generate-json-schema --out <temporary-schema-directory>
```

Copy the seven event fixture files from 0.144.5 because the official 0.144.6 release changes bundled model metadata rather than the event protocol. Set `manifest.toml` to version `0.144.6`, revision `5d1fbf26c43abc65a203928b2e31561cb039e06d`, adapter `v0_144_generic`, supported status, and the same verified capability booleans as 0.144.5. Do not run `codex exec` with a prompt.

- [ ] **Step 4: Update registry, catalog, matrix, and release docs**

Change `CompatibilityRegistry::default` so N is `Version::new(0, 144, 6)` and N-1 is `Version::new(0, 144, 5)`. Update `compatibility/codex-version.toml` with:

```toml
supported_min = "0.144.5"
tested_versions = ["0.144.5", "0.144.6"]
recommended = "0.144.6"
pinned_revision = "5d1fbf26c43abc65a203928b2e31561cb039e06d"
```

Register 0.144.6 first, retain 0.144.5, run `python scripts/generate_codex_matrix.py`, and update the release table to tested `0.144.5`, `0.144.6` and recommended `0.144.6`.

- [ ] **Step 5: Run contract and generator checks and verify GREEN**

Run:

```text
python scripts/generate_codex_matrix.py --check
cargo test -p codex-compat --all-features
```

Expected: the generator reports no stale matrix, both exact versions pass all fixture contracts, and 0.144.4 is untested.

- [ ] **Step 6: Commit Task 5**

```text
git add compatibility/codex-version.toml compatibility/codex-matrix.json crates/codex-compat/src/registry.rs crates/codex-compat/tests/contracts.rs fixtures/codex/versions/0.144.6 docs/release.md
git commit -m "feat: validate Codex CLI 0.144.6"
```

---

### Task 6: Documentation and Full Verification

**Files:**
- Modify: `README.md`
- Modify: `docs/operations.md`
- Modify: `docs/security.md`
- Modify: `docs/compatibility.md`
- Modify: `docs/testing.md`
- Test: complete workspace

**Interfaces:**
- Consumes: final behavior from Tasks 1-5.
- Produces: operator-facing configuration precedence, environment, diagnostics, initialization, and compatibility documentation.

- [ ] **Step 1: Update operator documentation**

Document this exact precedence and responsibility split:

```text
compiled defaults
< $COLAY_HOME/config.toml
< <repository>/.colay/config.toml
< $COLAY_CONFIG
< --config
```

State that `COLAY_HOME` defaults to `~/.colay` on Unix and `%USERPROFILE%\.colay` on Windows; configuration files are versioned partial overrides; arrays replace rather than concatenate; explicit missing files fail; read-only commands create nothing; first `run` initializes repository state; `init` writes a minimal override; resolver diagnostics reveal the selected executable; and tests never invoke real provider inference.

- [ ] **Step 2: Run formatting and repair only formatter output**

Run:

```text
cargo fmt --all
cargo fmt --all -- --check
```

Expected: the check exits successfully.

- [ ] **Step 3: Run Clippy and fix every warning at its source**

Run:

```text
cargo clippy --workspace --all-targets --all-features -- -D warnings
```

Expected: exit code 0 with no warnings.

- [ ] **Step 4: Run the complete workspace test suite**

Run:

```text
cargo test --workspace --all-features
```

Expected: every unit, integration, compatibility, migration, fake-provider, and Windows resolver test passes; no real provider inference is invoked.

- [ ] **Step 5: Check generated artifacts and working-tree scope**

Run:

```text
python scripts/generate_codex_matrix.py --check
git diff --check
git status --short
```

Expected: the matrix is current, no whitespace errors exist, and only files named in this plan are modified.

- [ ] **Step 6: Commit Task 6**

```text
git add README.md docs/operations.md docs/security.md docs/compatibility.md docs/testing.md
git commit -m "docs: explain layered configuration and resolution"
```

## Plan Self-Review Results

- Spec coverage: configuration precedence/defaults, source-preserving writes, lazy state, resolver unification, diagnostics, Codex 0.144.6, documentation, and required verification each map to a task.
- Type consistency: Tasks 2 and 3 consume `EffectiveConfig`; Task 4 owns `ResolvedExecutable`; Task 5 does not leak Codex types outside `codex-compat`.
- Scope: no telemetry, credential handling, quota inference, identity behavior, worktree lifecycle, merge, or push behavior is introduced.
