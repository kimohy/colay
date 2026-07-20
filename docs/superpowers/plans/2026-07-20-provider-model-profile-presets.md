# Provider Model Profile Presets Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give Colay complete built-in provider model presets, automatic effective-model execution, and safe CLI/TUI interfaces for inspecting, overriding, and resetting those presets.

**Architecture:** `orchestrator-state` owns the complete compiled preset matrix and existing layered merging. A focused CLI module owns profile reporting and TOML override mutation, while `app.rs` handles persistence/reload and the TUI remains presentation-only through typed control actions. Existing routing keeps selecting vendor-neutral quality profiles; provider adapters receive the resolved model/effort and append-only worker audit events record what was requested.

**Tech Stack:** Rust 2024, Clap, Serde, `toml_edit`, Ratatui/Crossterm, SQLite-backed audit events, fake provider binaries.

## Global Constraints

- Built-in mappings must exactly match the approved 2026-07-20 matrix in the design spec.
- End users do not receive a per-run model or profile flag; automatic task-quality routing remains authoritative.
- Administrators may override effective presets through layered TOML, `colay profiles`, or the TUI.
- Tests must use `orchestrator-test-support` fake binaries and must never invoke real Codex, Claude, or Gemini inference.
- Provider process arguments remain separated; do not introduce shell interpolation.
- Keep provider wire details out of `orchestrator-domain`.
- Preserve configuration schema version 4 and avoid a database migration.
- Preserve append-only audit semantics, redaction, explicit configuration targets, symlink rejection, private files, and atomic writes.
- Required final verification is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features`.

## File Structure

- Modify `crates/orchestrator-state/src/config.rs`: construct and test compiled provider/profile defaults; enable Claude effort only behind its existing runtime help check.
- Modify `crates/orchestrator-state/src/config_layers.rs`: prove partial layered overrides preserve the rest of the compiled matrix.
- Modify `crates/orchestrator-cli/src/args.rs`: define the `profiles` CLI grammar and typed provider/profile/effort values.
- Create `crates/orchestrator-cli/src/profile_config.rs`: list effective mappings, classify preset versus customized values, and mutate only the selected TOML override table.
- Modify `crates/orchestrator-cli/src/main.rs`: register the focused profile module.
- Modify `crates/orchestrator-cli/src/app.rs`: dispatch profile commands, persist/reload mutations, supply TUI rows, handle TUI actions, and enrich worker-start audit metadata.
- Modify `crates/orchestrator-tui/src/lib.rs`: add profile rows, the `f` profile screen/editor, validation, reset confirmation, and typed actions.
- Modify `crates/orchestrator-test-support/tests/provider_e2e.rs`: prove Claude and Gemini receive separated model arguments and Claude effort is capability-gated.
- Modify `crates/codex-compat/src/invocation.rs`: prove Codex receives the selected model and verified effort through exec arguments.
- Modify `README.md` and `config.example.toml`: document automatic selection, built-ins, CLI/TUI usage, override/reset behavior, and availability limitations.

---

### Task 1: Complete Compiled Preset Matrix

**Files:**
- Modify: `crates/orchestrator-state/src/config.rs:139-302`
- Modify: `crates/orchestrator-state/src/config.rs:1051-1110`
- Modify: `crates/orchestrator-state/src/config_layers.rs:73-128`

**Interfaces:**
- Produces: `RootConfig::default().orchestrator.model_profiles` with all nine entries.
- Produces: `ModelProfileConfig: PartialEq + Eq` for later preset/customized comparisons.
- Preserves: `BTreeMap<String, BTreeMap<String, ModelProfileConfig>>` serialized layout and schema version 4.

- [ ] **Step 1: Write failing compiled-default and layer-merge tests**

Add this assertion helper and test in `config.rs`:

```rust
fn assert_profile(
    config: &RootConfig,
    provider: &str,
    profile: &str,
    model: &str,
    effort: &str,
) {
    let value = &config.orchestrator.model_profiles[provider][profile];
    assert_eq!(value.model, model, "{provider}.{profile} model");
    assert_eq!(value.effort.as_deref(), Some(effort), "{provider}.{profile} effort");
}

#[test]
fn compiled_model_profile_defaults_are_complete_and_current() {
    let config = RootConfig::default();
    for (provider, profile, model, effort) in [
        ("codex", "economy", "gpt-5.6-luna", "low"),
        ("codex", "standard", "gpt-5.6-terra", "medium"),
        ("codex", "premium", "gpt-5.6-sol", "high"),
        ("claude", "economy", "claude-haiku-4-5", "low"),
        ("claude", "standard", "claude-sonnet-5", "medium"),
        ("claude", "premium", "claude-fable-5", "high"),
        ("gemini", "economy", "gemini-3.1-flash-lite", "low"),
        ("gemini", "standard", "gemini-3.5-flash", "medium"),
        ("gemini", "premium", "gemini-3.1-pro-preview", "high"),
    ] {
        assert_profile(&config, provider, profile, model, effort);
    }
    assert_eq!(config.orchestrator.model_profiles.len(), 3);
}
```

Extend `compiled_provider_defaults_are_safe_and_complete` with:

```rust
assert_eq!(
    config
        .orchestrator
        .providers
        .claude
        .as_ref()
        .map(|provider| provider.effort_flag_enabled),
    Some(true)
);
```

Add to `config_layers.rs`:

```rust
#[test]
fn one_profile_override_preserves_the_compiled_matrix()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = LayerFixture::new();
    fixture.write_repository(
        "config_version = 4\n[orchestrator.model_profiles.claude.premium]\nmodel = \"company-fable\"\n",
    );

    let effective = load_effective_config(&fixture.request())?;
    let profiles = &effective.config().orchestrator.model_profiles;
    assert_eq!(profiles["claude"]["premium"].model, "company-fable");
    assert_eq!(profiles["claude"]["premium"].effort.as_deref(), Some("high"));
    assert_eq!(profiles["claude"]["economy"].model, "claude-haiku-4-5");
    assert_eq!(profiles["codex"]["standard"].model, "gpt-5.6-terra");
    assert_eq!(profiles["gemini"]["premium"].model, "gemini-3.1-pro-preview");
    Ok(())
}
```

- [ ] **Step 2: Run the focused tests and verify RED**

Run:

```text
cargo test -p orchestrator-state compiled_model_profile_defaults_are_complete_and_current
cargo test -p orchestrator-state one_profile_override_preserves_the_compiled_matrix
```

Expected: both fail because `model_profiles` is empty; the first may also expose that Claude effort defaults to false.

- [ ] **Step 3: Implement the minimal compiled defaults**

Change `ModelProfileConfig` derives and add focused constructors:

```rust
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelProfileConfig {
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub effort: Option<String>,
}

fn model_profile(model: &str, effort: &str) -> ModelProfileConfig {
    ModelProfileConfig {
        model: model.to_owned(),
        effort: Some(effort.to_owned()),
    }
}

fn provider_profiles(
    economy: &str,
    standard: &str,
    premium: &str,
) -> BTreeMap<String, ModelProfileConfig> {
    [
        ("economy".to_owned(), model_profile(economy, "low")),
        ("standard".to_owned(), model_profile(standard, "medium")),
        ("premium".to_owned(), model_profile(premium, "high")),
    ]
    .into_iter()
    .collect()
}

fn default_model_profiles() -> BTreeMap<String, BTreeMap<String, ModelProfileConfig>> {
    [
        (
            "codex".to_owned(),
            provider_profiles("gpt-5.6-luna", "gpt-5.6-terra", "gpt-5.6-sol"),
        ),
        (
            "claude".to_owned(),
            provider_profiles("claude-haiku-4-5", "claude-sonnet-5", "claude-fable-5"),
        ),
        (
            "gemini".to_owned(),
            provider_profiles(
                "gemini-3.1-flash-lite",
                "gemini-3.5-flash",
                "gemini-3.1-pro-preview",
            ),
        ),
    ]
    .into_iter()
    .collect()
}
```

Use `default_model_profiles()` in `OrchestratorConfig::default()`. Change only the Claude provider constructor:

```rust
fn default_claude_provider() -> ProviderConfig {
    let mut provider = default_provider("claude", "calendar_month", Some(1), 90);
    provider.effort_flag_enabled = true;
    provider
}
```

- [ ] **Step 4: Run state tests and verify GREEN**

Run:

```text
cargo test -p orchestrator-state compiled_model_profile_defaults_are_complete_and_current
cargo test -p orchestrator-state one_profile_override_preserves_the_compiled_matrix
cargo test -p orchestrator-state
```

Expected: all `orchestrator-state` tests pass.

- [ ] **Step 5: Commit the default matrix**

```text
git add crates/orchestrator-state/src/config.rs crates/orchestrator-state/src/config_layers.rs
git commit -m "feat: add built-in provider model profiles"
```

---

### Task 2: Define the CLI Profile Command Contract

**Files:**
- Modify: `crates/orchestrator-cli/src/args.rs:20-175`

**Interfaces:**
- Produces: `Command::Profiles(ProfileArgs)`.
- Produces: `ProfileAction::{Set, Reset}` and typed `ProfileName`/`EffortName` values.
- Consumes: existing `ProviderName` enum.

- [ ] **Step 1: Write failing Clap parser tests**

Add two focused tests in `args.rs`:

```rust
#[test]
fn parses_profile_set_with_versioned_model_and_effort() -> Result<(), clap::Error> {
    let cli = Cli::try_parse_from([
        "colay",
        "profiles",
        "set",
        "claude",
        "premium",
        "--model",
        "claude-fable-5",
        "--effort",
        "high",
    ])?;
    assert!(matches!(
        cli.command,
        Command::Profiles(ProfileArgs {
            action: Some(ProfileAction::Set(ProfileSetArgs {
                provider: ProviderName::Claude,
                profile: ProfileName::Premium,
                model,
                effort: Some(EffortName::High),
            }))
        }) if model == "claude-fable-5"
    ));
    Ok(())
}

#[test]
fn parses_profile_reset_target() -> Result<(), clap::Error> {
    let cli = Cli::try_parse_from(["colay", "profiles", "reset", "gemini", "standard"])?;
    assert!(matches!(
        cli.command,
        Command::Profiles(ProfileArgs {
            action: Some(ProfileAction::Reset(ProfileTargetArgs {
                provider: ProviderName::Gemini,
                profile: ProfileName::Standard,
            }))
        })
    ));
    Ok(())
}
```

- [ ] **Step 2: Run the parser tests and verify RED**

Run:

```text
cargo test -p colay parses_profile_set_with_versioned_model_and_effort
cargo test -p colay parses_profile_reset_target
```

Expected: compilation fails because the profile command types do not exist.

- [ ] **Step 3: Add the typed command grammar**

Add `Profiles(ProfileArgs)` to `Command`, then define:

```rust
#[derive(Clone, Debug, Default, Args)]
pub struct ProfileArgs {
    #[command(subcommand)]
    pub action: Option<ProfileAction>,
}

#[derive(Clone, Debug, Subcommand)]
pub enum ProfileAction {
    /// Override one effective provider profile in the selected writable config layer.
    Set(ProfileSetArgs),
    /// Remove one override and reveal the next lower-precedence value.
    Reset(ProfileTargetArgs),
}

#[derive(Clone, Debug, Args)]
pub struct ProfileSetArgs {
    #[arg(value_enum)]
    pub provider: ProviderName,
    #[arg(value_enum)]
    pub profile: ProfileName,
    #[arg(long)]
    pub model: String,
    #[arg(long, value_enum)]
    pub effort: Option<EffortName>,
}

#[derive(Clone, Debug, Args)]
pub struct ProfileTargetArgs {
    #[arg(value_enum)]
    pub provider: ProviderName,
    #[arg(value_enum)]
    pub profile: ProfileName,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum ProfileName {
    Economy,
    Standard,
    Premium,
}

impl ProfileName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Economy => "economy",
            Self::Standard => "standard",
            Self::Premium => "premium",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum EffortName {
    Low,
    Medium,
    High,
}

impl EffortName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}
```

- [ ] **Step 4: Run parser tests and verify GREEN**

Run `cargo test -p colay args::tests`.

Expected: all argument parser tests pass.

- [ ] **Step 5: Commit the CLI contract**

```text
git add crates/orchestrator-cli/src/args.rs
git commit -m "feat: define profile management commands"
```

---

### Task 3: Build Focused Profile Reporting and TOML Mutation Helpers

**Files:**
- Create: `crates/orchestrator-cli/src/profile_config.rs`
- Modify: `crates/orchestrator-cli/src/main.rs:1-2`

**Interfaces:**
- Consumes: `RootConfig`, `DocumentMut`, provider/profile/effort strings validated by the CLI/TUI boundary.
- Produces: `ProfileReportRow` for CLI/TUI projections.
- Produces: `effective_profile_rows`, `set_profile_override`, and `reset_profile_override`.

- [ ] **Step 1: Create the module with failing behavior tests first**

Register `mod profile_config;` in `main.rs`. In the new module, write tests for these behaviors before implementation:

```rust
#[test]
fn effective_rows_identify_builtin_and_customized_values() -> Result<()> {
    let defaults = RootConfig::default();
    let mut effective = defaults.clone();
    effective.orchestrator.model_profiles["claude"]["premium"].model =
        "company-fable".to_owned();

    let rows = effective_profile_rows(&effective, &defaults)?;
    let builtin = rows
        .iter()
        .find(|row| row.provider == "codex" && row.profile == "standard")
        .ok_or_else(|| anyhow!("missing codex standard row"))?;
    let customized = rows
        .iter()
        .find(|row| row.provider == "claude" && row.profile == "premium")
        .ok_or_else(|| anyhow!("missing claude premium row"))?;
    assert_eq!(builtin.source, ProfileSource::Preset);
    assert_eq!(customized.source, ProfileSource::Customized);
    assert_eq!(customized.model, "company-fable");
    assert_eq!(rows.len(), 9);
    Ok(())
}

#[test]
fn set_then_reset_changes_only_the_selected_override() -> Result<()> {
    let mut document = "config_version = 4\n# keep this comment\n"
        .parse::<DocumentMut>()?;
    set_profile_override(
        &mut document,
        "claude",
        "premium",
        "company-fable",
        Some("high"),
    )?;
    set_profile_override(
        &mut document,
        "gemini",
        "standard",
        "company-gemini",
        Some("medium"),
    )?;
    assert!(reset_profile_override(&mut document, "claude", "premium")?);
    let text = document.to_string();
    assert!(text.contains("# keep this comment"));
    assert!(!text.contains("company-fable"));
    assert!(text.contains("company-gemini"));
    Ok(())
}

#[test]
fn profile_override_rejects_blank_model_and_invalid_effort() -> Result<()> {
    let mut document = "config_version = 4\n".parse::<DocumentMut>()?;
    assert!(set_profile_override(&mut document, "codex", "standard", "  ", None).is_err());
    assert!(
        set_profile_override(
            &mut document,
            "codex",
            "standard",
            "gpt-5.6-terra",
            Some("maximum"),
        )
        .is_err()
    );
    Ok(())
}
```

- [ ] **Step 2: Run the module tests and verify RED**

Run `cargo test -p colay profile_config::tests`.

Expected: compilation fails because the module interfaces are not implemented.

- [ ] **Step 3: Implement the report types and ordered projection**

Implement these public module interfaces:

```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProfileSource {
    Preset,
    Customized,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProfileReportRow {
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub effort: Option<String>,
    pub description: String,
    pub source: ProfileSource,
}

pub fn effective_profile_rows(
    effective: &RootConfig,
    defaults: &RootConfig,
) -> Result<Vec<ProfileReportRow>> {
    let mut rows = Vec::with_capacity(9);
    for provider in ["codex", "claude", "gemini"] {
        for profile in ["economy", "standard", "premium"] {
            let value = profile_value(effective, provider, profile)?;
            let default = profile_value(defaults, provider, profile)?;
            rows.push(ProfileReportRow {
                provider: provider.to_owned(),
                profile: profile.to_owned(),
                model: value.model.clone(),
                effort: value.effort.clone(),
                description: profile_description(profile)?.to_owned(),
                source: if value == default {
                    ProfileSource::Preset
                } else {
                    ProfileSource::Customized
                },
            });
        }
    }
    Ok(rows)
}
```

`profile_value` must return an actionable error naming the missing provider/profile. `profile_description` returns the three approved descriptions and errors for other inputs.

Use these exact helpers:

```rust
fn profile_value<'a>(
    config: &'a RootConfig,
    provider: &str,
    profile: &str,
) -> Result<&'a ModelProfileConfig> {
    config
        .orchestrator
        .model_profiles
        .get(provider)
        .and_then(|profiles| profiles.get(profile))
        .ok_or_else(|| anyhow!("{provider} {profile} model profile is not configured"))
}

fn profile_description(profile: &str) -> Result<&'static str> {
    match profile {
        "economy" => Ok("fast and cost-efficient simple work"),
        "standard" => Ok("everyday development work"),
        "premium" => Ok("complex work requiring the highest quality"),
        _ => bail!("unknown model profile `{profile}`"),
    }
}
```

- [ ] **Step 4: Implement narrow TOML set/reset helpers**

Implement `set_profile_override` by creating only `orchestrator.model_profiles.<provider>.<profile>`, inserting `model`, and inserting `effort` only when supplied. Implement `reset_profile_override` by removing `model` and `effort` only from that profile and pruning the now-empty profile/provider/model-profile tables. Both functions call private validators:

```rust
fn validate_target(provider: &str, profile: &str) -> Result<()> {
    if !["codex", "claude", "gemini"].contains(&provider) {
        bail!("unknown approved provider `{provider}`");
    }
    if !["economy", "standard", "premium"].contains(&profile) {
        bail!("unknown model profile `{profile}`");
    }
    Ok(())
}

fn validate_model(model: &str) -> Result<&str> {
    let model = model.trim();
    if model.is_empty() {
        bail!("model must not be blank");
    }
    Ok(model)
}

fn validate_effort(effort: Option<&str>) -> Result<Option<&str>> {
    match effort {
        None => Ok(None),
        Some(value @ ("low" | "medium" | "high")) => Ok(Some(value)),
        Some(value) => bail!("unsupported reasoning effort `{value}`"),
    }
}
```

Use this table navigation and mutation code; do not serialize and reparse an unrelated full config:

```rust
fn ensure_table<'a>(parent: &'a mut dyn TableLike, key: &str) -> Result<&'a mut dyn TableLike> {
    if !parent.contains_key(key) {
        parent.insert(key, Item::Table(Table::new()));
    }
    parent
        .get_mut(key)
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| anyhow!("configuration override `{key}` must be a table"))
}

pub fn set_profile_override(
    document: &mut DocumentMut,
    provider: &str,
    profile: &str,
    model: &str,
    effort: Option<&str>,
) -> Result<()> {
    validate_target(provider, profile)?;
    let model = validate_model(model)?;
    let effort = validate_effort(effort)?;
    let orchestrator = ensure_table(document.as_table_mut(), "orchestrator")?;
    let profiles = ensure_table(orchestrator, "model_profiles")?;
    let provider_profiles = ensure_table(profiles, provider)?;
    let profile_override = ensure_table(provider_profiles, profile)?;
    profile_override.insert("model", toml_edit::value(model));
    if let Some(effort) = effort {
        profile_override.insert("effort", toml_edit::value(effort));
    }
    Ok(())
}

pub fn reset_profile_override(
    document: &mut DocumentMut,
    provider: &str,
    profile: &str,
) -> Result<bool> {
    validate_target(provider, profile)?;
    let Some(orchestrator) = document
        .get_mut("orchestrator")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let Some(profiles) = orchestrator
        .get_mut("model_profiles")
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let Some(provider_profiles) = profiles
        .get_mut(provider)
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let Some(profile_override) = provider_profiles
        .get_mut(profile)
        .and_then(Item::as_table_like_mut)
    else {
        return Ok(false);
    };
    let removed = profile_override.remove("model").is_some()
        | profile_override.remove("effort").is_some();
    let remove_profile = profile_override.is_empty();
    if remove_profile {
        provider_profiles.remove(profile);
    }
    let remove_provider = provider_profiles.is_empty();
    if remove_provider {
        profiles.remove(provider);
    }
    let remove_profiles = profiles.is_empty();
    if remove_profiles {
        orchestrator.remove("model_profiles");
    }
    Ok(removed)
}
```

- [ ] **Step 5: Run module and CLI tests**

Run:

```text
cargo test -p colay profile_config::tests
cargo test -p colay
```

Expected: all tests pass with comments and unrelated overrides preserved.

- [ ] **Step 6: Commit the focused module**

```text
git add crates/orchestrator-cli/src/main.rs crates/orchestrator-cli/src/profile_config.rs
git commit -m "feat: add profile configuration helpers"
```

---

### Task 4: Wire CLI Listing, Set, Reset, and Reload

**Files:**
- Modify: `crates/orchestrator-cli/src/app.rs:1-180`
- Modify: `crates/orchestrator-cli/src/app.rs:498-610`
- Modify: `crates/orchestrator-cli/src/app.rs:6630-6810`

**Interfaces:**
- Consumes: Task 2 `ProfileAction` types and Task 3 helper functions.
- Produces: `profiles`, `profile_updated`, and `profile_reset` output envelopes.
- Preserves: existing explicit edit-target selection and atomic/private-file behavior.

- [ ] **Step 1: Write failing CLI persistence tests**

Import the new handlers into the `app.rs` test module and add:

```rust
#[test]
fn profile_set_persists_one_override_and_reloads_effective_config() -> Result<()> {
    let (_temporary, root) = canonical_tempdir()?;
    let environment = ConfigEnvironment::isolated();
    let runtime = load_config_runtime(&root, None, environment.clone())?;

    set_model_profile(
        &root,
        None,
        environment,
        &runtime,
        ProviderName::Claude,
        ProfileName::Premium,
        "company-fable",
        Some(EffortName::High),
        true,
    )?;

    let reloaded = load_config_runtime(&root, None, ConfigEnvironment::isolated())?;
    let profiles = &reloaded.effective.config().orchestrator.model_profiles;
    assert_eq!(profiles["claude"]["premium"].model, "company-fable");
    assert_eq!(profiles["codex"]["standard"].model, "gpt-5.6-terra");
    Ok(())
}

#[test]
fn profile_reset_reveals_the_compiled_preset() -> Result<()> {
    let (_temporary, root) = canonical_tempdir()?;
    let environment = ConfigEnvironment::isolated();
    let runtime = load_config_runtime(&root, None, environment.clone())?;
    set_model_profile(
        &root,
        None,
        environment.clone(),
        &runtime,
        ProviderName::Gemini,
        ProfileName::Standard,
        "company-gemini",
        None,
        true,
    )?;
    let runtime = load_config_runtime(&root, None, environment.clone())?;
    reset_model_profile(
        &root,
        None,
        environment,
        &runtime,
        ProviderName::Gemini,
        ProfileName::Standard,
        true,
    )?;

    let reloaded = load_config_runtime(&root, None, ConfigEnvironment::isolated())?;
    assert_eq!(
        reloaded.effective.config().orchestrator.model_profiles["gemini"]["standard"].model,
        "gemini-3.5-flash"
    );
    Ok(())
}
```

- [ ] **Step 2: Run tests and verify RED**

Run:

```text
cargo test -p colay profile_set_persists_one_override_and_reloads_effective_config
cargo test -p colay profile_reset_reveals_the_compiled_preset
```

Expected: compilation fails because the application handlers do not exist.

- [ ] **Step 3: Add command dispatch and listing**

Import `ProfileAction`, `ProfileName`, `EffortName`, `ProfileSetArgs`, and `ProfileTargetArgs`. Dispatch:

```rust
Command::Profiles(arguments) => match arguments.action {
    Some(ProfileAction::Set(arguments)) => set_model_profile(
        &repository,
        cli.config.as_deref(),
        environment,
        &runtime,
        arguments.provider,
        arguments.profile,
        &arguments.model,
        arguments.effort,
        cli.json,
    ),
    Some(ProfileAction::Reset(arguments)) => reset_model_profile(
        &repository,
        cli.config.as_deref(),
        environment,
        &runtime,
        arguments.provider,
        arguments.profile,
        cli.json,
    ),
    None => profiles(&runtime.effective, cli.json),
},
```

Implement listing as:

```rust
fn profiles(effective: &EffectiveConfig, json_output: bool) -> Result<()> {
    let defaults = RootConfig::default();
    let rows = effective_profile_rows(effective.config(), &defaults)?;
    emit(json_output, "profiles", &rows)
}
```

- [ ] **Step 4: Implement set/reset persistence and post-write verification**

Use these handlers and shared lookup:

```rust
fn selected_profile_row(
    config: &RootConfig,
    provider: ProviderName,
    profile: ProfileName,
) -> Result<ProfileReportRow> {
    let provider_id = ProviderId::from(provider);
    effective_profile_rows(config, &RootConfig::default())?
        .into_iter()
        .find(|row| {
            row.provider == provider_id.as_str() && row.profile == profile.as_str()
        })
        .ok_or_else(|| anyhow!("effective profile disappeared after configuration reload"))
}

#[allow(clippy::too_many_arguments)]
fn set_model_profile(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
    runtime: &ConfigRuntime,
    provider: ProviderName,
    profile: ProfileName,
    model: &str,
    effort: Option<EffortName>,
    json_output: bool,
) -> Result<()> {
    let mut document = load_edit_document(&runtime.explicit_edit_path)?;
    let provider_id = ProviderId::from(provider);
    set_profile_override(
        &mut document,
        provider_id.as_str(),
        profile.as_str(),
        model,
        effort.map(EffortName::as_str),
    )?;
    save_override_atomic(&document, &runtime.explicit_edit_path)?;
    let reloaded = load_config_runtime(repository, cli_config, environment)?;
    let row = selected_profile_row(reloaded.effective.config(), provider, profile)?;
    if row.model != model.trim() {
        bail!("model profile override did not survive effective configuration reload");
    }
    if effort.is_some_and(|value| row.effort.as_deref() != Some(value.as_str())) {
        bail!("model profile effort did not survive effective configuration reload");
    }
    emit(json_output, "profile_updated", &row)
}

fn reset_model_profile(
    repository: &Path,
    cli_config: Option<&Path>,
    environment: ConfigEnvironment,
    runtime: &ConfigRuntime,
    provider: ProviderName,
    profile: ProfileName,
    json_output: bool,
) -> Result<()> {
    let mut document = load_edit_document(&runtime.explicit_edit_path)?;
    let provider_id = ProviderId::from(provider);
    if !reset_profile_override(&mut document, provider_id.as_str(), profile.as_str())? {
        bail!("selected writable layer has no override for this model profile");
    }
    save_override_atomic(&document, &runtime.explicit_edit_path)?;
    let reloaded = load_config_runtime(repository, cli_config, environment)?;
    let row = selected_profile_row(reloaded.effective.config(), provider, profile)?;
    emit(json_output, "profile_reset", &row)
}
```

- [ ] **Step 5: Run focused and complete CLI tests**

Run:

```text
cargo test -p colay profile_set_persists_one_override_and_reloads_effective_config
cargo test -p colay profile_reset_reveals_the_compiled_preset
cargo test -p colay
```

Expected: all tests pass and only `.colay/config.toml` is created in each isolated fixture.

- [ ] **Step 6: Commit the CLI behavior**

```text
git add crates/orchestrator-cli/src/app.rs
git commit -m "feat: manage effective model profiles from cli"
```

---

### Task 5: Add the TUI Profile Matrix and Editor

**Files:**
- Modify: `crates/orchestrator-tui/src/lib.rs:21-150`
- Modify: `crates/orchestrator-tui/src/lib.rs:244-646`
- Modify: `crates/orchestrator-tui/src/lib.rs:676-813`
- Modify: `crates/orchestrator-tui/src/lib.rs:980-1315`
- Modify: `crates/orchestrator-cli/src/app.rs:4862-5105`

**Interfaces:**
- Consumes: Task 3 `ProfileReportRow` projected into presentation-safe `ModelProfileRow` values.
- Produces: `ControlAction::SetModelProfile` and `ControlAction::ResetModelProfile`.
- Preserves: TUI crate has no filesystem/config-path access.

- [ ] **Step 1: Write failing TUI interaction tests**

Add one test for saving and one for reset confirmation:

```rust
#[test]
fn profile_editor_emits_validated_model_and_effort() -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = DashboardSnapshot {
        model_profiles: vec![ModelProfileRow {
            provider: "claude".to_owned(),
            profile: "premium".to_owned(),
            model: "claude-fable-5".to_owned(),
            effort: "high".to_owned(),
            description: "complex work requiring the highest quality".to_owned(),
            customized: false,
        }],
        ..DashboardSnapshot::default()
    };
    let mut state = InteractionState::default();
    action_for_key(KeyCode::Char('f'), &snapshot, &mut state)?;
    action_for_key(KeyCode::Down, &snapshot, &mut state)?;
    action_for_key(KeyCode::Enter, &snapshot, &mut state)?;
    action_for_key(KeyCode::End, &snapshot, &mut state)?;
    action_for_key(KeyCode::Char('x'), &snapshot, &mut state)?;
    action_for_key(KeyCode::Down, &snapshot, &mut state)?;
    action_for_key(KeyCode::Down, &snapshot, &mut state)?;
    assert_eq!(
        action_for_key(KeyCode::Enter, &snapshot, &mut state)?,
        Some(ControlAction::SetModelProfile {
            provider: "claude".to_owned(),
            profile: "premium".to_owned(),
            model: "claude-fable-5x".to_owned(),
            effort: "high".to_owned(),
        })
    );
    Ok(())
}

#[test]
fn profile_reset_requires_explicit_confirmation() -> Result<(), Box<dyn std::error::Error>> {
    let snapshot = DashboardSnapshot {
        model_profiles: vec![ModelProfileRow {
            provider: "gemini".to_owned(),
            profile: "standard".to_owned(),
            model: "company-gemini".to_owned(),
            effort: "medium".to_owned(),
            description: "everyday development work".to_owned(),
            customized: true,
        }],
        ..DashboardSnapshot::default()
    };
    let mut state = InteractionState::default();
    action_for_key(KeyCode::Char('f'), &snapshot, &mut state)?;
    action_for_key(KeyCode::Down, &snapshot, &mut state)?;
    assert_eq!(action_for_key(KeyCode::Delete, &snapshot, &mut state)?, None);
    assert!(state.profile_reset_confirmation.is_some());
    assert_eq!(
        action_for_key(KeyCode::Char('y'), &snapshot, &mut state)?,
        Some(ControlAction::ResetModelProfile {
            provider: "gemini".to_owned(),
            profile: "standard".to_owned(),
        })
    );
    Ok(())
}
```

- [ ] **Step 2: Run TUI tests and verify RED**

Run `cargo test -p orchestrator-tui profile_`.

Expected: compilation fails because snapshot rows, editor state, and actions are missing.

- [ ] **Step 3: Add presentation types and typed actions**

Extend `DashboardSnapshot` with a defaulted `model_profiles: Vec<ModelProfileRow>`. Add:

```rust
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ModelProfileRow {
    pub provider: String,
    pub profile: String,
    pub model: String,
    pub effort: String,
    pub description: String,
    pub customized: bool,
}
```

Add actions:

```rust
SetModelProfile {
    provider: String,
    profile: String,
    model: String,
    effort: String,
},
ResetModelProfile {
    provider: String,
    profile: String,
},
```

- [ ] **Step 4: Implement the `f` screen, editor, and reset confirmation**

Add interaction state for a selected profile row, editor fields `Model`, `Effort`, and `Confirm`, and an optional reset-confirmation target. Required behavior:

- `f` opens the profile matrix with no implicit selection; arrow keys or digits select a row.
- `Enter` on a selected row opens an editor initialized from that row.
- Model accepts printable non-control characters up to 256 bytes; save trims it and rejects blank text.
- Effort cycles only through `low`, `medium`, and `high` with left/right or space.
- `Enter` on Confirm emits `SetModelProfile`.
- `Delete` on a customized selected row asks `Reset override? y/N`; `y` emits `ResetModelProfile`, while `n` or Escape cancels.
- `Delete` on a preset row shows `selected profile already uses the built-in preset`.
- Escape closes the current nested view without quitting; `q` quits only from the base dashboard.

Use these state shapes and give them priority ahead of the existing usage editor/provider picker in `action_for_key` and `render_interactive`:

```rust
#[derive(Clone, Debug, Default)]
struct ProfileListState {
    selected: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum ProfileEditorField {
    #[default]
    Model,
    Effort,
    Confirm,
}

#[derive(Clone, Debug)]
struct ProfileEditorState {
    provider: String,
    profile: String,
    model: String,
    effort: String,
    field: ProfileEditorField,
}

#[derive(Clone, Debug)]
struct ProfileResetConfirmation {
    provider: String,
    profile: String,
}

#[derive(Clone, Debug, Default)]
struct InteractionState {
    picker: Option<PickerState>,
    usage_editor: Option<UsageEditorState>,
    profile_list: Option<ProfileListState>,
    profile_editor: Option<ProfileEditorState>,
    profile_reset_confirmation: Option<ProfileResetConfirmation>,
    feedback: Option<String>,
}
```

The base-key branch is:

```rust
KeyCode::Char('f') => {
    if snapshot.model_profiles.is_empty() {
        state.feedback = Some("no configured model profiles".to_owned());
    } else {
        state.profile_list = Some(ProfileListState::default());
        state.feedback = Some("select a provider profile explicitly".to_owned());
    }
    None
}
```

`profile_editor_action` constructs the action only on the Confirm row:

```rust
let model = editor.model.trim();
if model.is_empty() {
    state.feedback = Some("model must not be blank".to_owned());
    return None;
}
if !matches!(editor.effort.as_str(), "low" | "medium" | "high") {
    state.feedback = Some("effort must be low, medium, or high".to_owned());
    return None;
}
return Some(ControlAction::SetModelProfile {
    provider: editor.provider.clone(),
    profile: editor.profile.clone(),
    model: model.to_owned(),
    effort: editor.effort.clone(),
});
```

Render the approved compact matrix with this row format, add the three profile explanations below it, and add `f:profiles` to the help line:

```rust
let marker = if list.selected == Some(index) { ">" } else { " " };
let source = if row.customized { "custom" } else { "preset" };
Line::from(format!(
    "{marker} {:<7} {:<8} {:<28} {:<6} [{source}]",
    row.provider, row.profile, row.model, row.effort,
))
```

Keep the existing five-panel dashboard unchanged when the profile view is closed.

- [ ] **Step 5: Project effective rows and handle TUI actions in `app.rs`**

Build `snapshot.model_profiles` from `effective_profile_rows(config, &RootConfig::default())` with this projection:

```rust
model_profiles: effective_profile_rows(config, &RootConfig::default())?
    .into_iter()
    .map(|row| orchestrator_tui::ModelProfileRow {
        provider: row.provider,
        profile: row.profile,
        model: row.model,
        effort: row.effort.unwrap_or_default(),
        description: row.description,
        customized: row.source == ProfileSource::Customized,
    })
    .collect(),
```

Use exact parsing helpers before calling the Task 4 persistence functions:

```rust
fn parse_profile_name(value: &str) -> Result<ProfileName> {
    match value {
        "economy" => Ok(ProfileName::Economy),
        "standard" => Ok(ProfileName::Standard),
        "premium" => Ok(ProfileName::Premium),
        _ => bail!("unknown model profile `{value}`"),
    }
}

fn parse_effort_name(value: &str) -> Result<EffortName> {
    match value {
        "low" => Ok(EffortName::Low),
        "medium" => Ok(EffortName::Medium),
        "high" => Ok(EffortName::High),
        _ => bail!("unsupported reasoning effort `{value}`"),
    }
}
```

`SetModelProfile` parses all strings and calls `set_model_profile` with `Some(effort)`; `ResetModelProfile` parses provider/profile and calls `reset_model_profile`. Do not let TUI strings bypass these validators.

- [ ] **Step 6: Run TUI and CLI tests**

Run:

```text
cargo test -p orchestrator-tui
cargo test -p colay
```

Expected: all tests pass, including the existing terminal guard, usage editor, provider picker, and five-panel render tests.

- [ ] **Step 7: Commit the TUI interface**

```text
git add crates/orchestrator-tui/src/lib.rs crates/orchestrator-cli/src/app.rs
git commit -m "feat: edit model profiles from tui"
```

---

### Task 6: Verify Provider Arguments and Audit Effective Settings

**Files:**
- Modify: `crates/orchestrator-providers/src/claude.rs:30-72`
- Modify: `crates/orchestrator-policy/src/routing.rs:419-435`
- Modify: `crates/orchestrator-test-support/tests/provider_e2e.rs:18-230`
- Modify: `crates/codex-compat/src/invocation.rs:207-240`
- Modify: `crates/orchestrator-cli/src/app.rs:1988-2050`
- Modify: `crates/orchestrator-cli/src/app.rs:6630-7420`

**Interfaces:**
- Consumes: resolved `WorkerRequest.model`, `.profile`, and `.reasoning_effort`.
- Produces: separated official CLI arguments and `WorkerStarted` audit payload fields `model` and `reasoning_effort`.

- [ ] **Step 1: Write a failing Claude capability-gate test**

In `claude.rs`, write the pure-probe test before extracting the production seam:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    const REQUIRED_HELP: &str =
        "--print --output-format stream-json --permission-mode plan acceptEdits --resume";

    #[test]
    fn effort_requires_both_admin_enablement_and_cli_advertisement() {
        let enabled_and_advertised = capabilities_from_probe(
            true,
            "Claude Code 5",
            true,
            &format!("{REQUIRED_HELP} --effort"),
            true,
        );
        let disabled = capabilities_from_probe(
            true,
            "Claude Code 5",
            true,
            &format!("{REQUIRED_HELP} --effort"),
            false,
        );
        let not_advertised = capabilities_from_probe(
            true,
            "Claude Code 5",
            true,
            REQUIRED_HELP,
            true,
        );

        assert_eq!(
            enabled_and_advertised.reasoning_effort,
            CapabilitySupport::Advertised
        );
        assert_eq!(disabled.reasoning_effort, CapabilitySupport::Unsupported);
        assert_eq!(
            not_advertised.reasoning_effort,
            CapabilitySupport::Unsupported
        );
    }
}
```

- [ ] **Step 2: Run the Claude gate test and verify RED**

Run `cargo test -p orchestrator-providers effort_requires_both_admin_enablement_and_cli_advertisement`.

Expected: compilation fails because `capabilities_from_probe` does not exist.

- [ ] **Step 3: Extract and use the pure Claude probe projection**

Implement this helper next to `advertised`/`verified`:

```rust
fn capabilities_from_probe(
    version_succeeded: bool,
    version_text: &str,
    help_succeeded: bool,
    help_text: &str,
    effort_flag_enabled: bool,
) -> ProviderCapabilities {
    let mut result = ProviderCapabilities::unsupported(ProviderId::Claude);
    result.version = version_succeeded.then(|| version_text.trim().to_owned());
    result.non_interactive = advertised(help_succeeded && help_text.contains("--print"));
    result.structured_output = advertised(
        help_text.contains("--output-format")
            && (help_text.contains("stream-json") || help_text.contains("stream_json")),
    );
    result.read_only = verified(
        help_succeeded
            && help_text.contains("--permission-mode")
            && help_text.contains("plan"),
    );
    result.writable = advertised(help_text.contains("acceptEdits"));
    result.session_resume = advertised(help_text.contains("--resume"));
    result.reasoning_effort =
        advertised(effort_flag_enabled && help_succeeded && help_text.contains("--effort"));
    result.evidence = vec!["claude --version".to_owned(), "claude --help".to_owned()];
    result
}
```

Have `detected_capabilities` gather stdout/stderr, then return this helper's result. Preserve the current two probes and their error propagation.

- [ ] **Step 4: Run the Claude gate test and verify GREEN**

Run `cargo test -p orchestrator-providers effort_requires_both_admin_enablement_and_cli_advertisement`.

Expected: pass for enabled-and-advertised, disabled, and not-advertised cases.

- [ ] **Step 5: Add provider-argument contract tests**

First add this routing contract test next to the existing quality-floor coverage. It is expected to pass and freezes the automatic profile/effort mapping that the preset lookup consumes:

```rust
#[test]
fn quality_tiers_select_matching_profiles_and_efforts() {
    for (quality, profile, effort) in [
        (QualityTier::Economy, ModelProfile::Economy, ReasoningEffort::Low),
        (
            QualityTier::Standard,
            ModelProfile::Standard,
            ReasoningEffort::Medium,
        ),
        (
            QualityTier::Premium,
            ModelProfile::Premium,
            ReasoningEffort::High,
        ),
    ] {
        assert_eq!(profile_for_quality(quality), profile);
        assert_eq!(effort_for_profile(profile), effort);
    }
}
```

Then add the provider argument tests below.

Replace the empty-model-only coverage with separate assertions that retain the existing omission test and add explicit model coverage:

```rust
#[test]
fn prepared_argv_passes_provider_model_ids_as_separate_arguments()
-> Result<(), Box<dyn std::error::Error>> {
    let shared = runtime();
    let claude = ClaudeAdapter::new(
        ClaudeAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Claude),
            effort_flag_enabled: true,
        },
        shared.clone(),
    );
    let gemini = GeminiAdapter::new(
        GeminiAdapterConfig {
            executable: fake_binary(),
            usage_probe: UsageProbeConfig::ManualOrLedger,
            usage_scope: scope(ProviderId::Gemini),
        },
        shared,
    );
    let mut claude_request = request(ProviderId::Claude, "secret task")?;
    claude_request.model = Some("claude-sonnet-5".to_owned());
    let mut gemini_request = request(ProviderId::Gemini, "secret task")?;
    gemini_request.model = Some("gemini-3.5-flash".to_owned());

    assert!(
        claude
            .prepare(&claude_request)?
            .args_lossy()
            .windows(2)
            .any(|pair| pair == ["--model", "claude-sonnet-5"])
    );
    assert!(
        gemini
            .prepare(&gemini_request)?
            .args_lossy()
            .windows(2)
            .any(|pair| pair == ["--model", "gemini-3.5-flash"])
    );
    Ok(())
}
```

Add a Codex compatibility test using verified model/effort capabilities:

```rust
#[test]
fn exec_passes_selected_model_and_verified_effort() -> Result<(), CompatibilityError> {
    let request = CodexRequest {
        working_directory: PathBuf::from("repo"),
        prompt: "do the task".to_owned(),
        model: Some("gpt-5.6-terra".to_owned()),
        effort: Some(ReasoningEffort::Medium),
        sandbox: CodexSandbox::ReadOnly,
        resume_session: None,
        output_schema: None,
    };
    let capabilities = CodexCapabilities {
        exec: CapabilitySupport::Verified,
        jsonl_output: CapabilitySupport::Verified,
        read_only_sandbox: CapabilitySupport::Verified,
        exec_reasoning_effort: CapabilitySupport::Verified,
        ..CodexCapabilities::default()
    };
    let invocation = CodexInvocation::exec("codex", &request, &capabilities)?;
    assert!(invocation.args.windows(2).any(|pair| pair == ["--model", "gpt-5.6-terra"]));
    assert!(invocation.args.iter().any(|arg| arg.contains("model_reasoning_effort=\"medium\"")));
    Ok(())
}
```

The model argument is unconditional for a non-empty model; reasoning effort is gated by `exec_reasoning_effort`.

- [ ] **Step 6: Run provider contract tests and establish the existing baseline**

Run:

```text
cargo test -p orchestrator-test-support --test provider_e2e prepared_argv_passes_provider_model_ids_as_separate_arguments
cargo test -p codex-compat exec_passes_selected_model_and_verified_effort
cargo test -p orchestrator-policy quality_tiers_select_matching_profiles_and_efforts
```

Expected: both pass, documenting the existing separated-argument adapter contract. The production behavior added in this task is the audit payload, whose test is written and observed failing next.

- [ ] **Step 7: Write a failing audit-payload unit test**

Extract a pure `worker_started_payload(&WorkerRequest) -> Value` seam and test the wished-for payload first:

```rust
#[test]
fn worker_started_audit_records_effective_model_profile_and_effort() -> Result<()> {
    let request = WorkerRequest {
        schema_version: SchemaVersion::v1(),
        task_id: orchestrator_domain::TaskId::new(),
        attempt_id: AttemptId::new(),
        provider: ProviderId::Claude,
        objective: "audit selection".to_owned(),
        prompt: "do work".to_owned(),
        constraints: Vec::new(),
        acceptance_criteria: Vec::new(),
        workspace_root: std::env::current_dir()?,
        sandbox: SandboxMode::WorkspaceWrite,
        profile: ModelProfile::Premium,
        model: Some("claude-fable-5".to_owned()),
        reasoning_effort: Some(orchestrator_domain::ReasoningEffort::High),
        timeout_seconds: 60,
        max_output_bytes: 1024,
        resume_session_id: None,
        handover_payload: None,
    };
    let payload = worker_started_payload(&request);
    assert_eq!(payload["model"], "claude-fable-5");
    assert_eq!(payload["profile"], "premium");
    assert_eq!(payload["reasoning_effort"], "high");
    Ok(())
}
```

- [ ] **Step 8: Run the audit test and verify RED**

Run `cargo test -p colay worker_started_audit_records_effective_model_profile_and_effort`.

Expected: compilation fails because `worker_started_payload` does not exist.

- [ ] **Step 9: Use one pure payload builder in the production event path**

Implement:

```rust
fn worker_started_payload(request: &WorkerRequest) -> Value {
    json!({
        "attempt_id": request.attempt_id,
        "provider": request.provider,
        "sandbox": request.sandbox,
        "profile": request.profile,
        "model": request.model.as_deref(),
        "reasoning_effort": request.reasoning_effort,
        "session_resume_requested": request.resume_session_id.is_some(),
    })
}
```

Replace the inline `WorkerStarted` JSON with this helper. Do not log prompts, credentials, or provider session IDs.

- [ ] **Step 10: Run provider, compatibility, and CLI tests**

Run:

```text
cargo test -p orchestrator-test-support --test provider_e2e
cargo test -p orchestrator-providers
cargo test -p orchestrator-policy quality_tiers_select_matching_profiles_and_efforts
cargo test -p codex-compat
cargo test -p colay worker_started_audit_records_effective_model_profile_and_effort
```

Expected: all tests pass. The pure Claude probe test proves effort support remains unusable unless both the administrator setting and installed CLI help enable it.

- [ ] **Step 11: Commit invocation and audit evidence**

```text
git add crates/orchestrator-providers/src/claude.rs crates/orchestrator-policy/src/routing.rs crates/orchestrator-test-support/tests/provider_e2e.rs crates/codex-compat/src/invocation.rs crates/orchestrator-cli/src/app.rs
git commit -m "test: verify effective provider model selection"
```

---

### Task 7: Document Presets and Run Full Verification

**Files:**
- Modify: `README.md:28-80`
- Modify: `config.example.toml:1-10`

**Interfaces:**
- Documents: built-in matrix, automatic quality selection, CLI/TUI controls, partial overrides, reset semantics, and provider availability limits.

- [ ] **Step 1: Add a failing documentation contract test**

Add a focused test to `crates/orchestrator-cli/src/app.rs` so the shipped docs cannot silently lose the new public interface:

```rust
#[test]
fn shipped_docs_describe_profile_management_and_current_presets() {
    let readme = include_str!("../../../README.md");
    let example = include_str!("../../../config.example.toml");
    for required in [
        "colay profiles",
        "colay profiles set",
        "colay profiles reset",
        "claude-fable-5",
        "gemini-3.5-flash",
        "gpt-5.6-sol",
        "f:profiles",
    ] {
        assert!(readme.contains(required), "README is missing {required}");
    }
    assert!(example.contains("orchestrator.model_profiles.claude.premium"));
    assert!(example.contains("claude-fable-5"));
}
```

- [ ] **Step 2: Run the documentation test and verify RED**

Run `cargo test -p colay shipped_docs_describe_profile_management_and_current_presets`.

Expected: failure because the README and example config do not contain the new interface or mappings.

- [ ] **Step 3: Update README and example configuration**

Add the commands to the command summary and a concise `Model profiles` subsection containing the exact 3x3 matrix. State that task analysis chooses the profile automatically, model IDs are current as of 2026-07-20, official CLI/account entitlement still governs availability, and administrators can use layered TOML or the two interfaces.

Use this concrete README content (adjust only surrounding heading levels to fit the existing document):

```markdown
## Model profiles

Colay analyzes each task and automatically selects `economy`, `standard`, or `premium`; users do not choose a model per run. The built-in mappings below are current as of 2026-07-20:

| Provider | Economy (`low`) | Standard (`medium`) | Premium (`high`) |
| --- | --- | --- | --- |
| Codex | `gpt-5.6-luna` | `gpt-5.6-terra` | `gpt-5.6-sol` |
| Claude | `claude-haiku-4-5` | `claude-sonnet-5` | `claude-fable-5` |
| Gemini | `gemini-3.1-flash-lite` | `gemini-3.5-flash` | `gemini-3.1-pro-preview` |

Inspect effective settings with `colay profiles` or `colay profiles --json`. Administrators can override one entry with `colay profiles set <provider> <profile> --model <id> [--effort low|medium|high]`, and remove the writable-layer override with `colay profiles reset <provider> <profile>`. Reset reveals the compiled or lower-precedence value; it does not delete it. The TUI exposes the same matrix and editor under `f:profiles`.

Layered TOML remains supported for managed deployments. A configured model must also be available to the installed official provider CLI and the authenticated account; a preset does not grant model access.
```

Add commented examples to `config.example.toml`:

```toml
# Colay supplies complete built-in profiles. Override only organization-specific values.
# [orchestrator.model_profiles.claude.premium]
# model = "claude-fable-5"
# effort = "high"
#
# [orchestrator.model_profiles.gemini.standard]
# model = "gemini-3.5-flash"
# effort = "medium"
```

Document that `colay profiles reset <provider> <profile>` removes the writable-layer override rather than deleting compiled or lower-precedence values.

- [ ] **Step 4: Run the documentation test and verify GREEN**

Run `cargo test -p colay shipped_docs_describe_profile_management_and_current_presets`.

Expected: pass.

- [ ] **Step 5: Run formatting and inspect the diff**

Run:

```text
cargo fmt --all
git diff --check
git status --short
```

Expected: no whitespace errors and only the files named in this plan are modified.

- [ ] **Step 6: Run required repository verification**

Run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

Expected: all commands exit 0 with no warnings.

- [ ] **Step 7: Commit documentation and final formatting**

```text
git add README.md config.example.toml crates/orchestrator-cli/src/app.rs
git commit -m "docs: explain provider model profile management"
```

- [ ] **Step 8: Review the final commit range**

Run:

```text
git status --short
git log --oneline 864121f..HEAD
git diff --stat 864121f..HEAD
```

Expected: clean worktree; one focused commit per task; no files outside the plan's file structure.
