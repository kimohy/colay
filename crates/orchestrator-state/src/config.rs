use std::{
    collections::BTreeMap,
    fs,
    io::Write as _,
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use orchestrator_domain::RepoPath;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;
use toml_edit::{Array, DocumentMut, Item, Table, Value, value};

use crate::{
    StateError, StateResult, ensure_private_directory, ensure_private_file,
    reject_symlink_components,
};

pub const CONFIG_SCHEMA_VERSION: u32 = 4;

const MINIMUM_CONFIG_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigMigrationStep {
    pub from_version: u32,
    pub to_version: u32,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigMigrationPlan {
    pub current_version: u32,
    pub target_version: u32,
    pub steps: Vec<ConfigMigrationStep>,
    pub destructive: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigMigrationResult {
    pub initial_version: u32,
    pub final_version: u32,
    pub applied_steps: Vec<ConfigMigrationStep>,
    pub changed: bool,
}

#[derive(Clone, Debug)]
pub struct ConfigMigrationPreview {
    plan: ConfigMigrationPlan,
    result: ConfigMigrationResult,
    migrated: ConfigDocument,
}

impl ConfigMigrationPreview {
    #[must_use]
    pub const fn plan(&self) -> &ConfigMigrationPlan {
        &self.plan
    }

    #[must_use]
    pub const fn result(&self) -> &ConfigMigrationResult {
        &self.result
    }

    #[must_use]
    pub const fn migrated(&self) -> &ConfigDocument {
        &self.migrated
    }
}

#[derive(Clone, Debug)]
pub struct ConfigMigrationApplyResult {
    pub result: ConfigMigrationResult,
    pub backup_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RootConfig {
    pub config_version: u32,
    pub orchestrator: OrchestratorConfig,
    #[serde(default)]
    pub features: FeatureConfig,
}

impl Default for RootConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_SCHEMA_VERSION,
            orchestrator: OrchestratorConfig::default(),
            features: FeatureConfig::default(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct OrchestratorConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub automatic_routing: bool,
    #[serde(default = "default_state_dir")]
    pub state_dir: PathBuf,
    #[serde(default = "default_timezone")]
    pub timezone: String,
    #[serde(default = "default_one")]
    pub max_parallel_workers: u32,
    #[serde(default = "default_timeout")]
    pub default_timeout_minutes: u64,
    #[serde(default = "default_retry")]
    pub max_retries: u32,
    #[serde(default = "default_warning")]
    pub warning_threshold_percent: f64,
    #[serde(default = "default_handover")]
    pub handover_threshold_percent: f64,
    #[serde(default = "default_reserve")]
    pub critical_reserve_percent: f64,
    #[serde(default = "default_review_score")]
    pub require_review_from_difficulty: u8,
    #[serde(default = "default_minimum_progress")]
    pub minimum_progress: f64,
    #[serde(default = "default_daily_grace")]
    pub daily_grace_minutes: u64,
    #[serde(default = "default_monthly_grace")]
    pub monthly_grace_minutes: u64,
    #[serde(default = "default_alpha")]
    pub forecast_alpha: f64,
    #[serde(default = "default_observations")]
    pub minimum_forecast_observations: u32,
    pub providers: ProviderConfigs,
    #[serde(default)]
    pub model_profiles: BTreeMap<String, BTreeMap<String, ModelProfileConfig>>,
    /// Optional per-provider writable worker limits. Missing providers inherit the global limit.
    #[serde(default)]
    pub provider_parallel_limits: BTreeMap<String, u32>,
    /// Administrator-defined patterns for organization-specific secret formats.
    /// Exact credential values are deliberately not accepted by persisted config.
    #[serde(default)]
    pub redaction: RedactionSettings,
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
            model_profiles: default_model_profiles(),
            provider_parallel_limits: BTreeMap::new(),
            redaction: RedactionSettings::default(),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RedactionSettings {
    #[serde(default)]
    pub patterns: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfigs {
    #[serde(default)]
    pub gemini: Option<ProviderConfig>,
    #[serde(default)]
    pub codex: Option<ProviderConfig>,
    #[serde(default)]
    pub claude: Option<ProviderConfig>,
}

impl Default for ProviderConfigs {
    fn default() -> Self {
        Self {
            gemini: Some(default_gemini_provider()),
            codex: Some(default_codex_provider()),
            claude: Some(default_claude_provider()),
        }
    }
}

impl ProviderConfigs {
    fn iter(&self) -> impl Iterator<Item = (&'static str, &ProviderConfig)> {
        [
            ("gemini", self.gemini.as_ref()),
            ("codex", self.codex.as_ref()),
            ("claude", self.claude.as_ref()),
        ]
        .into_iter()
        .filter_map(|(name, value)| value.map(|config| (name, config)))
    }
}

fn default_gemini_provider() -> ProviderConfig {
    default_provider("gemini", "calendar_day", None, 70)
}

fn default_codex_provider() -> ProviderConfig {
    default_provider("codex", "calendar_month", Some(1), 100)
}

fn default_claude_provider() -> ProviderConfig {
    let mut provider = default_provider("claude", "calendar_month", Some(1), 90);
    provider.effort_flag_enabled = true;
    provider
}

fn default_provider(
    executable: &str,
    quota_period: &str,
    reset_day: Option<u8>,
    priority: i32,
) -> ProviderConfig {
    ProviderConfig {
        enabled: true,
        executable: executable.to_owned(),
        quota_period: quota_period.to_owned(),
        quota_limit: None,
        quota_unit: default_usage_unit(),
        quota_scope: None,
        quota_units_per_work_unit: None,
        ledger_units_per_execution: None,
        reset_day,
        reset_timezone: "UTC".to_owned(),
        rolling_anchor: None,
        rolling_period_seconds: None,
        custom_started_at: None,
        custom_resets_at: None,
        priority,
        effort_flag_enabled: false,
        usage_probe: UsageProbeConfig::ManualOrLedger,
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ProviderConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    pub executable: String,
    pub quota_period: String,
    #[serde(default)]
    pub quota_limit: Option<f64>,
    #[serde(default = "default_usage_unit")]
    pub quota_unit: String,
    #[serde(default)]
    pub quota_scope: Option<String>,
    /// Optional administrator calibration for comparing provider-local quota units with
    /// the analyzer's vendor-neutral work units. No default is inferred.
    #[serde(default)]
    pub quota_units_per_work_unit: Option<f64>,
    /// Optional administrator calibration used when a CLI emits no structured usage event.
    /// Each completed provider process contributes this many provider-local quota units.
    #[serde(default)]
    pub ledger_units_per_execution: Option<f64>,
    #[serde(default)]
    pub reset_day: Option<u8>,
    pub reset_timezone: String,
    #[serde(default)]
    pub rolling_anchor: Option<DateTime<Utc>>,
    #[serde(default)]
    pub rolling_period_seconds: Option<u64>,
    #[serde(default)]
    pub custom_started_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub custom_resets_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub priority: i32,
    /// Administrator assertion that the installed provider CLI contract supports an
    /// explicit reasoning-effort flag. Defaults to false for graceful degradation.
    #[serde(default)]
    pub effort_flag_enabled: bool,
    #[serde(default)]
    pub usage_probe: UsageProbeConfig,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum UsageProbeConfig {
    Command {
        executable: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default = "default_json")]
        format: String,
    },
    #[default]
    ManualOrLedger,
}

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

#[derive(Clone, Debug, Deserialize, Serialize)]
#[allow(clippy::struct_excessive_bools)]
pub struct FeatureConfig {
    #[serde(default = "default_true")]
    pub orchestrator: bool,
    #[serde(default = "default_true")]
    pub orchestrator_tui: bool,
    #[serde(default = "default_true")]
    pub codex_app_server_adapter: bool,
    #[serde(default = "default_true")]
    pub codex_exec_fallback: bool,
}

impl Default for FeatureConfig {
    fn default() -> Self {
        Self {
            orchestrator: true,
            orchestrator_tui: true,
            codex_app_server_adapter: true,
            codex_exec_fallback: true,
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("{field}: {message}")]
pub struct ConfigValidationError {
    pub field: String,
    pub message: String,
}

/// A syntactically valid config document that may use an older supported schema.
///
/// Unlike [`ConfigDocument`], this type does not deserialize legacy input into the
/// current typed config before migration. It exists solely to plan, preview, and
/// backup-first apply the explicit sequential migration catalog.
#[derive(Clone, Debug)]
pub struct MigratableConfigDocument {
    document: DocumentMut,
    original: String,
    current_version: u32,
}

impl MigratableConfigDocument {
    pub fn parse(input: &str) -> StateResult<Self> {
        let document = input.parse::<DocumentMut>()?;
        let current_version = config_version(&document)?;
        validate_migratable_version(current_version)?;
        Ok(Self {
            document,
            original: input.to_owned(),
            current_version,
        })
    }

    pub fn load(path: &Path) -> StateResult<Self> {
        reject_symlink_components(path)?;
        let input = fs::read_to_string(path).map_err(|error| StateError::io(path, error))?;
        Self::parse(&input)
    }

    #[must_use]
    pub const fn current_version(&self) -> u32 {
        self.current_version
    }

    pub fn plan(&self) -> StateResult<ConfigMigrationPlan> {
        config_migration_plan(self.current_version)
    }

    /// Applies the catalog to an in-memory clone and validates the final document
    /// using the same strict current-schema parser used at normal startup.
    pub fn dry_run(&self) -> StateResult<ConfigMigrationPreview> {
        let plan = self.plan()?;
        let mut candidate = self.document.clone();
        let mut current_version = self.current_version;
        let mut applied_steps = Vec::with_capacity(plan.steps.len());

        for step in &plan.steps {
            let expected = current_version.checked_add(1).ok_or_else(|| {
                StateError::InvalidConfig("config schema version overflow".to_owned())
            })?;
            if step.from_version != current_version || step.to_version != expected {
                return Err(StateError::MigrationGap {
                    version: step.to_version,
                    expected,
                });
            }
            apply_config_migration(&mut candidate, step)?;
            let actual = config_version(&candidate)?;
            if actual != expected {
                return Err(StateError::MigrationGap {
                    version: actual,
                    expected,
                });
            }
            current_version = actual;
            applied_steps.push(step.clone());
        }

        let migrated = ConfigDocument::parse(&candidate.to_string())?;
        let result = ConfigMigrationResult {
            initial_version: self.current_version,
            final_version: current_version,
            changed: !applied_steps.is_empty(),
            applied_steps,
        };
        Ok(ConfigMigrationPreview {
            plan,
            result,
            migrated,
        })
    }

    /// Applies a migration only after proving the file still matches the loaded
    /// source and creating a verified immutable sibling backup. The final write
    /// delegates to [`ConfigDocument::save_atomic`].
    pub fn apply_to_file(
        &self,
        path: &Path,
        timestamp: DateTime<Utc>,
    ) -> StateResult<ConfigMigrationApplyResult> {
        reject_symlink_components(path)?;
        let preview = self.dry_run()?;
        let current = fs::read_to_string(path).map_err(|error| StateError::io(path, error))?;
        if current != self.original {
            return Err(StateError::InvalidConfig(format!(
                "config changed after migration planning: {}",
                path.display()
            )));
        }
        if !preview.result.changed {
            return Ok(ConfigMigrationApplyResult {
                result: preview.result,
                backup_path: None,
            });
        }

        let backup = ConfigDocument::backup_before_migration(path, timestamp)?;
        let backup_contents =
            fs::read_to_string(&backup).map_err(|error| StateError::io(&backup, error))?;
        if backup_contents != self.original {
            return Err(StateError::InvalidConfig(format!(
                "config migration backup does not match source: {}",
                backup.display()
            )));
        }

        preview.migrated.save_atomic(path)?;
        let persisted = ConfigDocument::load(path)?;
        if persisted.config.config_version != preview.result.final_version {
            return Err(StateError::InvalidConfig(format!(
                "persisted config version {} differs from migration result {}",
                persisted.config.config_version, preview.result.final_version
            )));
        }

        Ok(ConfigMigrationApplyResult {
            result: preview.result,
            backup_path: Some(backup),
        })
    }
}

#[derive(Clone, Debug)]
pub struct ConfigDocument {
    document: DocumentMut,
    config: RootConfig,
}

impl ConfigDocument {
    pub fn parse(input: &str) -> StateResult<Self> {
        let document = input.parse::<DocumentMut>()?;
        let config = toml_edit::de::from_str::<RootConfig>(input)?;
        validate(&config).map_err(|errors| {
            StateError::InvalidConfig(
                errors
                    .iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;
        Ok(Self { document, config })
    }

    pub fn load(path: &Path) -> StateResult<Self> {
        reject_symlink_components(path)?;
        let input = fs::read_to_string(path).map_err(|error| StateError::io(path, error))?;
        Self::parse(&input)
    }

    #[must_use]
    pub fn config(&self) -> &RootConfig {
        &self.config
    }

    #[must_use]
    pub fn document(&self) -> &DocumentMut {
        &self.document
    }

    /// Updates a known scalar without reconstructing the document, so comments and
    /// unknown fields survive config migrations and administrative edits.
    pub fn set_enabled(&mut self, enabled: bool) -> StateResult<()> {
        let mut candidate = self.document.clone();
        candidate["orchestrator"]["enabled"] = toml_edit::value(enabled);
        self.replace_document(candidate)
    }

    pub fn set_automatic_routing(&mut self, enabled: bool) -> StateResult<()> {
        let mut candidate = self.document.clone();
        candidate["orchestrator"]["automatic_routing"] = toml_edit::value(enabled);
        self.replace_document(candidate)
    }

    pub fn set_provider_enabled(&mut self, provider: &str, enabled: bool) -> StateResult<()> {
        if !matches!(provider, "gemini" | "codex" | "claude") {
            return Err(StateError::InvalidConfig(format!(
                "unknown provider `{provider}`"
            )));
        }
        let mut candidate = self.document.clone();
        candidate["orchestrator"]["providers"][provider]["enabled"] = toml_edit::value(enabled);
        self.replace_document(candidate)
    }

    pub fn save_atomic(&self, path: &Path) -> StateResult<()> {
        let parent = path.parent().ok_or_else(|| {
            StateError::InvalidConfig(format!("config path has no parent: {}", path.display()))
        })?;
        ensure_private_directory(parent)?;
        reject_symlink_components(path)?;
        let mut temporary =
            NamedTempFile::new_in(parent).map_err(|error| StateError::io(parent, error))?;
        temporary
            .write_all(self.document.to_string().as_bytes())
            .map_err(|error| StateError::io(temporary.path(), error))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|error| StateError::io(temporary.path(), error))?;
        temporary
            .persist(path)
            .map_err(|error| StateError::io(path, error.error))?;
        ensure_private_file(path)?;
        sync_directory(parent)
    }

    /// Creates the immutable sibling backup required before a config migration.
    pub fn backup_before_migration(path: &Path, timestamp: DateTime<Utc>) -> StateResult<PathBuf> {
        reject_symlink_components(path)?;
        let file_name = path.file_name().ok_or_else(|| {
            StateError::InvalidConfig(format!("config path has no file name: {}", path.display()))
        })?;
        let mut backup_name = file_name.to_os_string();
        backup_name.push(format!(".backup.{}", timestamp.format("%Y%m%dT%H%M%S%.fZ")));
        let backup = path.with_file_name(backup_name);
        reject_symlink_components(&backup)?;
        let mut source = fs::File::open(path).map_err(|error| StateError::io(path, error))?;
        let mut destination = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&backup)
            .map_err(|error| StateError::io(&backup, error))?;
        std::io::copy(&mut source, &mut destination)
            .map_err(|error| StateError::io(&backup, error))?;
        destination
            .sync_all()
            .map_err(|error| StateError::io(&backup, error))?;
        ensure_private_file(&backup)?;
        if let Some(parent) = backup.parent() {
            sync_directory(parent)?;
        }
        Ok(backup)
    }

    fn replace_document(&mut self, candidate: DocumentMut) -> StateResult<()> {
        let input = candidate.to_string();
        let updated = Self::parse(&input)?;
        self.document = candidate;
        self.config = updated.config;
        Ok(())
    }
}

fn config_migration_plan(current_version: u32) -> StateResult<ConfigMigrationPlan> {
    validate_migratable_version(current_version)?;
    let mut steps = Vec::new();
    let mut from_version = current_version;
    while from_version < CONFIG_SCHEMA_VERSION {
        let to_version = from_version.checked_add(1).ok_or_else(|| {
            StateError::InvalidConfig("config schema version overflow".to_owned())
        })?;
        let name = match (from_version, to_version) {
            (1, 2) => "automatic_routing_default",
            (2, 3) => "redaction_patterns_default",
            (3, 4) => "legacy_state_dir_materialization",
            _ => {
                return Err(StateError::MigrationGap {
                    version: to_version,
                    expected: from_version + 1,
                });
            }
        };
        steps.push(ConfigMigrationStep {
            from_version,
            to_version,
            name: name.to_owned(),
        });
        from_version = to_version;
    }
    Ok(ConfigMigrationPlan {
        current_version,
        target_version: CONFIG_SCHEMA_VERSION,
        steps,
        destructive: false,
    })
}

fn apply_config_migration(
    document: &mut DocumentMut,
    step: &ConfigMigrationStep,
) -> StateResult<()> {
    match (step.from_version, step.to_version) {
        (1, 2) => migrate_config_v1_to_v2(document)?,
        (2, 3) => migrate_config_v2_to_v3(document)?,
        (3, 4) => migrate_config_v3_to_v4(document)?,
        _ => {
            return Err(StateError::MigrationGap {
                version: step.to_version,
                expected: step.from_version + 1,
            });
        }
    }
    set_config_version(document, step.to_version)
}

fn migrate_config_v1_to_v2(document: &mut DocumentMut) -> StateResult<()> {
    let orchestrator = orchestrator_table_mut(document)?;
    if !orchestrator.contains_key("automatic_routing") {
        orchestrator.insert("automatic_routing", value(true));
    }
    Ok(())
}

fn migrate_config_v2_to_v3(document: &mut DocumentMut) -> StateResult<()> {
    let orchestrator = orchestrator_table_mut(document)?;
    if !orchestrator.contains_key("redaction") {
        orchestrator.insert("redaction", Item::Table(Table::new()));
    }
    let redaction = orchestrator
        .get_mut("redaction")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| {
            StateError::InvalidConfig("orchestrator.redaction must be a table".to_owned())
        })?;
    if !redaction.contains_key("patterns") {
        redaction.insert("patterns", value(Array::new()));
    }
    Ok(())
}

fn migrate_config_v3_to_v4(document: &mut DocumentMut) -> StateResult<()> {
    let orchestrator = orchestrator_table_mut(document)?;
    if !orchestrator.contains_key("state_dir") {
        // Before config v4, an omitted state_dir resolved to this legacy path. Materialize
        // that historical value so upgrading never silently selects the new `.colay` root.
        orchestrator.insert("state_dir", value(".codex/orchestrator"));
    }
    Ok(())
}

fn orchestrator_table_mut(
    document: &mut DocumentMut,
) -> StateResult<&mut dyn toml_edit::TableLike> {
    document
        .get_mut("orchestrator")
        .and_then(Item::as_table_like_mut)
        .ok_or_else(|| StateError::InvalidConfig("orchestrator must be a table".to_owned()))
}

fn set_config_version(document: &mut DocumentMut, version: u32) -> StateResult<()> {
    let value = document
        .get_mut("config_version")
        .and_then(Item::as_value_mut)
        .ok_or_else(|| {
            StateError::InvalidConfig("config_version must be a positive integer".to_owned())
        })?;
    let decor = value.decor().clone();
    let mut replacement = Value::from(i64::from(version));
    *replacement.decor_mut() = decor;
    *value = replacement;
    Ok(())
}

fn config_version(document: &DocumentMut) -> StateResult<u32> {
    let raw = document
        .get("config_version")
        .and_then(Item::as_integer)
        .ok_or_else(|| {
            StateError::InvalidConfig("config_version must be a positive integer".to_owned())
        })?;
    u32::try_from(raw).map_err(|_| {
        StateError::InvalidConfig("config_version must be a positive integer".to_owned())
    })
}

fn validate_migratable_version(version: u32) -> StateResult<()> {
    if version > CONFIG_SCHEMA_VERSION {
        return Err(StateError::FutureSchema {
            found: version,
            supported: CONFIG_SCHEMA_VERSION,
        });
    }
    if version < MINIMUM_CONFIG_SCHEMA_VERSION {
        return Err(StateError::InvalidConfig(format!(
            "config_version {version} is older than the minimum supported version {MINIMUM_CONFIG_SCHEMA_VERSION}"
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
fn validate(config: &RootConfig) -> Result<(), Vec<ConfigValidationError>> {
    let mut errors = Vec::new();
    if config.config_version != CONFIG_SCHEMA_VERSION {
        errors.push(validation_error(
            "config_version",
            format!(
                "expected {CONFIG_SCHEMA_VERSION}, found {}",
                config.config_version
            ),
        ));
    }
    let orchestrator = &config.orchestrator;
    if RepoPath::try_from(orchestrator.state_dir.clone()).is_err() {
        errors.push(validation_error(
            "orchestrator.state_dir",
            "must be a non-empty relative path without `.` or `..` components",
        ));
    }
    validate_timezone("orchestrator.timezone", &orchestrator.timezone, &mut errors);
    if orchestrator.max_parallel_workers == 0 {
        errors.push(validation_error(
            "orchestrator.max_parallel_workers",
            "must be greater than zero",
        ));
    }
    for (provider, limit) in &orchestrator.provider_parallel_limits {
        if !matches!(provider.as_str(), "codex" | "claude" | "gemini") {
            errors.push(validation_error(
                format!("orchestrator.provider_parallel_limits.{provider}"),
                "provider must be codex, claude, or gemini",
            ));
        }
        if *limit == 0 {
            errors.push(validation_error(
                format!("orchestrator.provider_parallel_limits.{provider}"),
                "must be greater than zero",
            ));
        }
    }
    if orchestrator.default_timeout_minutes == 0 {
        errors.push(validation_error(
            "orchestrator.default_timeout_minutes",
            "must be greater than zero",
        ));
    }
    check_percent(
        "orchestrator.warning_threshold_percent",
        orchestrator.warning_threshold_percent,
        &mut errors,
    );
    check_percent(
        "orchestrator.handover_threshold_percent",
        orchestrator.handover_threshold_percent,
        &mut errors,
    );
    check_percent(
        "orchestrator.critical_reserve_percent",
        orchestrator.critical_reserve_percent,
        &mut errors,
    );
    if orchestrator.handover_threshold_percent > orchestrator.warning_threshold_percent {
        errors.push(validation_error(
            "orchestrator.handover_threshold_percent",
            "must not exceed warning_threshold_percent",
        ));
    }
    if !(0.0 < orchestrator.minimum_progress && orchestrator.minimum_progress <= 1.0) {
        errors.push(validation_error(
            "orchestrator.minimum_progress",
            "must be finite and in (0, 1]",
        ));
    }
    if !(0.0 < orchestrator.forecast_alpha && orchestrator.forecast_alpha <= 1.0) {
        errors.push(validation_error(
            "orchestrator.forecast_alpha",
            "must be finite and in (0, 1]",
        ));
    }
    if orchestrator.minimum_forecast_observations == 0 {
        errors.push(validation_error(
            "orchestrator.minimum_forecast_observations",
            "must be greater than zero",
        ));
    }
    if orchestrator.require_review_from_difficulty > 10 {
        errors.push(validation_error(
            "orchestrator.require_review_from_difficulty",
            "must be between 0 and 10",
        ));
    }
    if orchestrator.redaction.patterns.len() > 128 {
        errors.push(validation_error(
            "orchestrator.redaction.patterns",
            "must contain no more than 128 patterns",
        ));
    }
    for (index, pattern) in orchestrator.redaction.patterns.iter().enumerate() {
        let field = format!("orchestrator.redaction.patterns.{index}");
        if pattern.is_empty() || pattern.len() > 4096 || pattern.contains('\0') {
            errors.push(validation_error(
                field,
                "must be non-empty, at most 4096 bytes, and contain no NUL byte",
            ));
            continue;
        }
        if let Err(error) = regex::RegexBuilder::new(pattern)
            .size_limit(1 << 20)
            .build()
        {
            errors.push(validation_error(field, format!("invalid regex: {error}")));
        }
    }

    for (name, provider) in orchestrator.providers.iter() {
        let prefix = format!("orchestrator.providers.{name}");
        if !(0..=100).contains(&provider.priority) {
            errors.push(validation_error(
                format!("{prefix}.priority"),
                "must be between 0 and 100",
            ));
        }
        if provider.executable.trim().is_empty() || provider.executable.contains('\0') {
            errors.push(validation_error(
                format!("{prefix}.executable"),
                "must be non-empty and contain no NUL byte",
            ));
        }
        if provider.quota_period.trim().is_empty() {
            errors.push(validation_error(
                format!("{prefix}.quota_period"),
                "must be non-empty",
            ));
        }
        if !matches!(
            provider.quota_period.as_str(),
            "calendar_day" | "rolling_day" | "calendar_month" | "rolling_month" | "custom"
        ) {
            errors.push(validation_error(
                format!("{prefix}.quota_period"),
                "must be calendar_day, rolling_day, calendar_month, rolling_month, or custom",
            ));
        }
        if provider
            .quota_scope
            .as_ref()
            .is_some_and(|scope| scope.trim().is_empty())
        {
            errors.push(validation_error(
                format!("{prefix}.quota_scope"),
                "must be non-empty when provided",
            ));
        }
        if provider
            .quota_limit
            .is_some_and(|limit| !limit.is_finite() || limit <= 0.0)
        {
            errors.push(validation_error(
                format!("{prefix}.quota_limit"),
                "must be finite and greater than zero",
            ));
        }
        for (field, value) in [
            (
                "quota_units_per_work_unit",
                provider.quota_units_per_work_unit,
            ),
            (
                "ledger_units_per_execution",
                provider.ledger_units_per_execution,
            ),
        ] {
            if value.is_some_and(|value| !value.is_finite() || value <= 0.0) {
                errors.push(validation_error(
                    format!("{prefix}.{field}"),
                    "must be finite and greater than zero when provided",
                ));
            }
        }
        if provider
            .reset_day
            .is_some_and(|day| !(1..=31).contains(&day))
        {
            errors.push(validation_error(
                format!("{prefix}.reset_day"),
                "must be between 1 and 31",
            ));
        }
        let rolling = matches!(
            provider.quota_period.as_str(),
            "rolling_day" | "rolling_month"
        );
        if rolling
            && (provider.rolling_anchor.is_none()
                || provider
                    .rolling_period_seconds
                    .is_none_or(|seconds| seconds == 0))
        {
            errors.push(validation_error(
                format!("{prefix}.rolling_anchor"),
                "rolling periods require rolling_anchor and rolling_period_seconds > 0",
            ));
        }
        if provider.quota_period == "custom" {
            match (provider.custom_started_at, provider.custom_resets_at) {
                (Some(started), Some(resets)) if resets > started => {}
                _ => errors.push(validation_error(
                    format!("{prefix}.custom_started_at"),
                    "custom periods require custom_started_at and a later custom_resets_at",
                )),
            }
        }
        validate_timezone(
            &format!("{prefix}.reset_timezone"),
            &provider.reset_timezone,
            &mut errors,
        );
        if let UsageProbeConfig::Command {
            executable,
            args,
            format,
        } = &provider.usage_probe
        {
            if executable.trim().is_empty() || executable.contains('\0') {
                errors.push(validation_error(
                    format!("{prefix}.usage_probe.executable"),
                    "must be non-empty and contain no NUL byte",
                ));
            }
            if args.iter().any(|argument| argument.contains('\0')) {
                errors.push(validation_error(
                    format!("{prefix}.usage_probe.args"),
                    "arguments must not contain NUL bytes",
                ));
            }
            if format != "json" {
                errors.push(validation_error(
                    format!("{prefix}.usage_probe.format"),
                    "only `json` is supported",
                ));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors)
    }
}

fn check_percent(field: &str, value: f64, errors: &mut Vec<ConfigValidationError>) {
    if !value.is_finite() || !(0.0..=100.0).contains(&value) {
        errors.push(validation_error(field, "must be finite and in [0, 100]"));
    }
}

fn validate_timezone(field: &str, value: &str, errors: &mut Vec<ConfigValidationError>) {
    if value.parse::<Tz>().is_err() {
        errors.push(validation_error(field, "must be a valid IANA timezone"));
    }
}

fn validation_error(field: impl Into<String>, message: impl Into<String>) -> ConfigValidationError {
    ConfigValidationError {
        field: field.into(),
        message: message.into(),
    }
}

#[cfg(windows)]
fn sync_directory(path: &Path) -> StateResult<()> {
    fs::metadata(path)
        .map(|_| ())
        .map_err(|error| StateError::io(path, error))
}

#[cfg(not(windows))]
fn sync_directory(path: &Path) -> StateResult<()> {
    let directory = fs::File::open(path).map_err(|error| StateError::io(path, error))?;
    directory
        .sync_all()
        .map_err(|error| StateError::io(path, error))
}

const fn default_true() -> bool {
    true
}
const fn default_one() -> u32 {
    1
}
const fn default_timeout() -> u64 {
    30
}
const fn default_retry() -> u32 {
    1
}
const fn default_warning() -> f64 {
    30.0
}
const fn default_handover() -> f64 {
    15.0
}
const fn default_reserve() -> f64 {
    15.0
}
const fn default_review_score() -> u8 {
    7
}
const fn default_minimum_progress() -> f64 {
    0.05
}
const fn default_daily_grace() -> u64 {
    60
}
const fn default_monthly_grace() -> u64 {
    1_440
}
const fn default_alpha() -> f64 {
    0.3
}
const fn default_observations() -> u32 {
    3
}
fn default_state_dir() -> PathBuf {
    PathBuf::from(".colay")
}
fn default_timezone() -> String {
    "UTC".to_owned()
}
fn default_usage_unit() -> String {
    "provider_defined".to_owned()
}
fn default_json() -> String {
    "json".to_owned()
}

#[cfg(test)]
mod tests {
    use super::{ConfigDocument, RootConfig, UsageProbeConfig};

    fn assert_profile(
        config: &RootConfig,
        provider: &str,
        profile: &str,
        model: &str,
        effort: &str,
    ) {
        let value = &config.orchestrator.model_profiles[provider][profile];
        assert_eq!(value.model, model, "{provider}.{profile} model");
        assert_eq!(
            value.effort.as_deref(),
            Some(effort),
            "{provider}.{profile} effort"
        );
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

    #[test]
    fn compiled_provider_defaults_are_safe_and_complete() {
        let config = RootConfig::default();
        let cases = [
            ("codex", "codex", "calendar_month", Some(1), 100),
            ("claude", "claude", "calendar_month", Some(1), 90),
            ("gemini", "gemini", "calendar_day", None, 70),
        ];

        for (identity, executable, period, reset_day, priority) in cases {
            let provider = match identity {
                "codex" => config.orchestrator.providers.codex.as_ref(),
                "claude" => config.orchestrator.providers.claude.as_ref(),
                "gemini" => config.orchestrator.providers.gemini.as_ref(),
                _ => unreachable!("provider table contains only compiled identities"),
            }
            .unwrap_or_else(|| panic!("compiled {identity} provider is missing"));

            assert!(provider.enabled, "{identity} must be enabled");
            assert_eq!(provider.executable, executable, "{identity} executable");
            assert_eq!(provider.quota_period, period, "{identity} quota period");
            assert_eq!(provider.reset_day, reset_day, "{identity} reset day");
            assert_eq!(provider.priority, priority, "{identity} priority");
            assert_eq!(provider.reset_timezone, "UTC", "{identity} reset zone");
            assert_eq!(provider.quota_unit, "provider_defined");
            assert!(provider.quota_limit.is_none(), "{identity} quota limit");
            assert!(provider.quota_scope.is_none(), "{identity} quota scope");
            assert!(
                provider.quota_units_per_work_unit.is_none(),
                "{identity} work-unit calibration"
            );
            assert!(
                provider.ledger_units_per_execution.is_none(),
                "{identity} execution calibration"
            );
            assert!(
                provider.rolling_anchor.is_none(),
                "{identity} rolling anchor"
            );
            assert!(
                provider.rolling_period_seconds.is_none(),
                "{identity} rolling period"
            );
            assert!(
                provider.custom_started_at.is_none(),
                "{identity} custom start"
            );
            assert!(
                provider.custom_resets_at.is_none(),
                "{identity} custom reset"
            );
            assert!(matches!(
                provider.usage_probe,
                UsageProbeConfig::ManualOrLedger
            ));
        }

        assert_eq!(
            config
                .orchestrator
                .providers
                .claude
                .as_ref()
                .map(|provider| provider.effort_flag_enabled),
            Some(true)
        );
    }

    const VALID: &str = r#"
config_version = 4
future_root_field = "keep-me"

[orchestrator]
enabled = true
automatic_routing = true
state_dir = ".colay"
timezone = "Asia/Seoul"
warning_threshold_percent = 30
handover_threshold_percent = 15
critical_reserve_percent = 15
minimum_progress = 0.05
forecast_alpha = 0.3
minimum_forecast_observations = 3

[orchestrator.providers.codex]
enabled = true
executable = "codex"
quota_period = "calendar_month"
reset_day = 1
reset_timezone = "Asia/Seoul"
future_provider_field = true

[features]
orchestrator = true
"#;

    #[test]
    fn preserves_unknown_fields_and_comments() {
        let mut config = ConfigDocument::parse(VALID).unwrap_or_else(|error| {
            panic!("valid fixture should parse: {error}");
        });
        config.set_enabled(false).unwrap_or_else(|error| {
            panic!("known update should succeed: {error}");
        });
        let text = config.document().to_string();
        assert!(text.contains("future_root_field = \"keep-me\""));
        assert!(text.contains("future_provider_field = true"));
        assert!(!config.config().orchestrator.enabled);
    }

    #[test]
    fn rejects_unsafe_state_directory() {
        let invalid = VALID.replace(".colay", "../outside");
        assert!(ConfigDocument::parse(&invalid).is_err());
    }

    #[test]
    fn rejects_invalid_threshold_order() {
        let invalid = VALID.replace(
            "handover_threshold_percent = 15",
            "handover_threshold_percent = 40",
        );
        assert!(ConfigDocument::parse(&invalid).is_err());
    }

    #[test]
    fn validates_optional_provider_parallel_limits() {
        let valid = VALID.replace(
            "minimum_forecast_observations = 3",
            "minimum_forecast_observations = 3\nprovider_parallel_limits = { codex = 2, claude = 1 }",
        );
        let document = ConfigDocument::parse(&valid).unwrap_or_else(|error| {
            panic!("valid provider limits should parse: {error}");
        });
        assert_eq!(
            document
                .config()
                .orchestrator
                .provider_parallel_limits
                .get("codex"),
            Some(&2)
        );

        let zero = valid.replace("codex = 2", "codex = 0");
        assert!(ConfigDocument::parse(&zero).is_err());

        let unknown = valid.replace("codex = 2", "other = 2");
        assert!(ConfigDocument::parse(&unknown).is_err());
    }

    #[test]
    fn validates_administrator_redaction_patterns() {
        let valid = VALID.replace(
            "[orchestrator.providers.codex]",
            "[orchestrator.redaction]\npatterns = [\"(?i)COMPANY-SECRET-[A-Z0-9_-]+\"]\n\n[orchestrator.providers.codex]",
        );
        let document = ConfigDocument::parse(&valid).unwrap_or_else(|error| {
            panic!("valid redaction pattern should parse: {error}");
        });
        assert_eq!(
            document.config().orchestrator.redaction.patterns,
            ["(?i)COMPANY-SECRET-[A-Z0-9_-]+"]
        );

        let invalid = VALID.replace(
            "[orchestrator.providers.codex]",
            "[orchestrator.redaction]\npatterns = [\"(unterminated\"]\n\n[orchestrator.providers.codex]",
        );
        assert!(ConfigDocument::parse(&invalid).is_err());
    }
}
