#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::{
        fs,
        path::{Path, PathBuf},
    };

    use tempfile::TempDir;

    use crate::{
        CONFIG_SCHEMA_VERSION, ConfigEnvironment, ConfigLayerKind, ConfigRequest,
        load_effective_config,
    };

    struct LayerFixture {
        _temp: TempDir,
        repository: PathBuf,
        global_home: PathBuf,
        environment: PathBuf,
        cli: PathBuf,
    }

    impl LayerFixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(temp.path()).unwrap();
            let repository = root.join("repository");
            fs::create_dir_all(&repository).unwrap();
            Self {
                global_home: root.join("home/.colay"),
                environment: root.join("environment.toml"),
                cli: root.join("cli.toml"),
                _temp: temp,
                repository,
            }
        }

        fn write_global(&self, contents: &str) {
            write_layer(&self.global_home.join("config.toml"), contents);
        }

        fn write_repository(&self, contents: &str) {
            write_layer(&self.repository.join(".colay/config.toml"), contents);
        }

        fn write_environment(&self, contents: &str) {
            write_layer(&self.environment, contents);
        }

        fn write_cli(&self, contents: &str) {
            write_layer(&self.cli, contents);
        }

        fn request(&self) -> ConfigRequest<'_> {
            ConfigRequest {
                repository: &self.repository,
                cli_config: Some(&self.cli),
                environment: ConfigEnvironment {
                    colay_home: Some(self.global_home.clone()),
                    user_home: None,
                    colay_config: Some(self.environment.clone()),
                },
            }
        }
    }

    fn write_layer(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }

    #[test]
    fn empty_layers_use_complete_safe_defaults() {
        let repository = tempfile::tempdir().unwrap();
        let repository_root = fs::canonicalize(repository.path()).unwrap();
        let request = ConfigRequest::new(&repository_root, ConfigEnvironment::isolated());
        let effective = load_effective_config(&request).unwrap();

        assert_eq!(effective.config().config_version, CONFIG_SCHEMA_VERSION);
        assert_eq!(
            effective.config().orchestrator.state_dir,
            PathBuf::from(".colay")
        );
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
        fixture.write_cli("config_version = 4\n[orchestrator]\nmax_parallel_workers = 4\n");

        let effective = load_effective_config(&fixture.request()).unwrap();
        assert_eq!(effective.config().orchestrator.max_parallel_workers, 4);
        assert_eq!(effective.config().orchestrator.default_timeout_minutes, 45);
        assert_eq!(
            effective.config().orchestrator.redaction.patterns,
            ["REPO-[0-9]+"]
        );
        assert_eq!(
            effective
                .sources()
                .iter()
                .map(|source| source.kind)
                .collect::<Vec<_>>(),
            [
                ConfigLayerKind::Global,
                ConfigLayerKind::Repository,
                ConfigLayerKind::Environment,
                ConfigLayerKind::Cli,
            ]
        );
    }

    #[test]
    fn explicit_missing_config_is_an_error() {
        let repository = tempfile::tempdir().unwrap();
        let repository_root = fs::canonicalize(repository.path()).unwrap();
        let missing = repository_root.join("missing.toml");
        let request = ConfigRequest {
            repository: &repository_root,
            cli_config: Some(&missing),
            environment: ConfigEnvironment::isolated(),
        };
        assert!(
            load_effective_config(&request)
                .unwrap_err()
                .to_string()
                .contains("cli")
        );
    }

    #[test]
    fn invalid_optional_layer_fails_closed() {
        let fixture = LayerFixture::new();
        fixture.write_global("config_version = 4\n[orchestrator\n");
        let error = load_effective_config(&fixture.request())
            .unwrap_err()
            .to_string();
        assert!(error.contains("global"));
    }

    #[test]
    fn layer_without_config_version_fails_closed() {
        let fixture = LayerFixture::new();
        fixture.write_global("[orchestrator]\nmax_parallel_workers = 2\n");
        let error = load_effective_config(&fixture.request())
            .unwrap_err()
            .to_string();
        assert!(error.contains("global"));
        assert!(error.contains("config_version"));
    }

    #[test]
    fn future_layer_schema_fails_closed() {
        let fixture = LayerFixture::new();
        fixture.write_global("config_version = 999\n");
        let error = load_effective_config(&fixture.request())
            .unwrap_err()
            .to_string();
        assert!(error.contains("global"));
        assert!(error.contains("999"));
    }

    #[test]
    fn simultaneous_current_and_legacy_repository_files_fail_closed() {
        let fixture = LayerFixture::new();
        fixture.write_repository("config_version = 4\n");
        write_layer(
            &fixture.repository.join(".codex/orchestrator/config.toml"),
            "config_version = 4\n",
        );
        let error = load_effective_config(&fixture.request())
            .unwrap_err()
            .to_string();
        assert!(error.contains("legacy"));
        assert!(error.contains("repository"));
    }
}
use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::Serialize;
use toml_edit::{DocumentMut, InlineTable, Item, Table, Value};

use crate::{
    CONFIG_SCHEMA_VERSION, ConfigDocument, RootConfig, StateError, StateResult,
    reject_symlink_components,
};

const REPOSITORY_CONFIG_PATH: &str = ".colay/config.toml";
const LEGACY_REPOSITORY_CONFIG_PATH: &str = ".codex/orchestrator/config.toml";

#[derive(Clone, Debug, Default)]
pub struct ConfigEnvironment {
    pub colay_home: Option<PathBuf>,
    pub user_home: Option<PathBuf>,
    pub colay_config: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigLayerKind {
    Global,
    Repository,
    Environment,
    Cli,
    LegacyRepository,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConfigSource {
    pub kind: ConfigLayerKind,
    pub path: PathBuf,
}

pub struct ConfigRequest<'a> {
    pub repository: &'a Path,
    pub cli_config: Option<&'a Path>,
    pub environment: ConfigEnvironment,
}

#[derive(Debug)]
pub struct EffectiveConfig {
    document: ConfigDocument,
    sources: Vec<ConfigSource>,
    repository_override: PathBuf,
}

impl ConfigEnvironment {
    #[must_use]
    pub const fn isolated() -> Self {
        Self {
            colay_home: None,
            user_home: None,
            colay_config: None,
        }
    }
}

impl<'a> ConfigRequest<'a> {
    #[must_use]
    pub const fn new(repository: &'a Path, environment: ConfigEnvironment) -> Self {
        Self {
            repository,
            cli_config: None,
            environment,
        }
    }
}

impl EffectiveConfig {
    #[must_use]
    pub fn config(&self) -> &RootConfig {
        self.document.config()
    }

    #[must_use]
    pub const fn document(&self) -> &ConfigDocument {
        &self.document
    }

    #[must_use]
    pub fn sources(&self) -> &[ConfigSource] {
        &self.sources
    }

    #[must_use]
    pub fn repository_override(&self) -> &Path {
        &self.repository_override
    }
}

pub fn load_effective_config(request: &ConfigRequest<'_>) -> StateResult<EffectiveConfig> {
    reject_symlink_components(request.repository)?;
    let repository_override = request.repository.join(REPOSITORY_CONFIG_PATH);
    let legacy_repository = request.repository.join(LEGACY_REPOSITORY_CONFIG_PATH);
    let mut document = default_document()?;
    let mut sources = Vec::new();

    if let Some(global_home) = global_home(&request.environment) {
        load_optional_layer(
            ConfigLayerKind::Global,
            &global_home.join("config.toml"),
            &mut document,
            &mut sources,
        )?;
    }

    match (
        source_exists(ConfigLayerKind::Repository, &repository_override)?,
        source_exists(ConfigLayerKind::LegacyRepository, &legacy_repository)?,
    ) {
        (true, true) => {
            if !request.cli_config.is_some_and(|path| {
                matches_repository_config(path, &repository_override, &legacy_repository)
            }) {
                return Err(StateError::InvalidConfig(format!(
                    "repository and legacy repository config layers both exist ({} and {}); select one explicitly with --config",
                    repository_override.display(),
                    legacy_repository.display()
                )));
            }
        }
        (true, false) => load_optional_layer(
            ConfigLayerKind::Repository,
            &repository_override,
            &mut document,
            &mut sources,
        )?,
        (false, true) => load_optional_layer(
            ConfigLayerKind::LegacyRepository,
            &legacy_repository,
            &mut document,
            &mut sources,
        )?,
        (false, false) => {}
    }

    if let Some(path) = request.environment.colay_config.as_deref() {
        load_required_layer(
            ConfigLayerKind::Environment,
            path,
            &mut document,
            &mut sources,
        )?;
    }
    if let Some(path) = request.cli_config {
        load_required_layer(ConfigLayerKind::Cli, path, &mut document, &mut sources)?;
    }

    let document = ConfigDocument::parse(&document.to_string())?;
    Ok(EffectiveConfig {
        document,
        sources,
        repository_override,
    })
}

fn default_document() -> StateResult<DocumentMut> {
    let serialized = toml_edit::ser::to_string(&RootConfig::default()).map_err(|error| {
        StateError::InvalidConfig(format!("cannot serialize defaults: {error}"))
    })?;
    ConfigDocument::parse(&serialized)?;
    serialized.parse::<DocumentMut>().map_err(Into::into)
}

fn global_home(environment: &ConfigEnvironment) -> Option<PathBuf> {
    environment.colay_home.clone().or_else(|| {
        environment
            .user_home
            .as_ref()
            .map(|home| home.join(".colay"))
    })
}

fn path_exists(path: &Path) -> StateResult<bool> {
    reject_symlink_components(path)?;
    match fs::metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(StateError::io(path, error)),
    }
}

fn matches_repository_config(path: &Path, current: &Path, legacy: &Path) -> bool {
    path == current || path == legacy
}

fn load_optional_layer(
    kind: ConfigLayerKind,
    path: &Path,
    target: &mut DocumentMut,
    sources: &mut Vec<ConfigSource>,
) -> StateResult<()> {
    if source_exists(kind, path)? {
        load_layer(kind, path, target, sources)
    } else {
        Ok(())
    }
}

fn load_required_layer(
    kind: ConfigLayerKind,
    path: &Path,
    target: &mut DocumentMut,
    sources: &mut Vec<ConfigSource>,
) -> StateResult<()> {
    if !source_exists(kind, path)? {
        return Err(layer_error(
            kind,
            path,
            "explicit config path does not exist",
        ));
    }
    load_layer(kind, path, target, sources)
}

fn load_layer(
    kind: ConfigLayerKind,
    path: &Path,
    target: &mut DocumentMut,
    sources: &mut Vec<ConfigSource>,
) -> StateResult<()> {
    reject_symlink_components(path).map_err(|error| layer_error(kind, path, error))?;
    let contents = fs::read_to_string(path)
        .map_err(|error| layer_error(kind, path, StateError::io(path, error)))?;
    let overlay = contents
        .parse::<DocumentMut>()
        .map_err(|error| layer_error(kind, path, error))?;
    validate_layer_version(kind, path, &overlay)?;
    merge_table(target.as_table_mut(), overlay.as_table());
    sources.push(ConfigSource {
        kind,
        path: path.to_path_buf(),
    });
    Ok(())
}

fn source_exists(kind: ConfigLayerKind, path: &Path) -> StateResult<bool> {
    path_exists(path).map_err(|error| layer_error(kind, path, error))
}

fn validate_layer_version(
    kind: ConfigLayerKind,
    path: &Path,
    document: &DocumentMut,
) -> StateResult<()> {
    let version = document
        .get("config_version")
        .and_then(Item::as_integer)
        .ok_or_else(|| layer_error(kind, path, "config_version must be a positive integer"))?;
    let version = u32::try_from(version)
        .map_err(|_| layer_error(kind, path, "config_version must be a positive integer"))?;
    if version != CONFIG_SCHEMA_VERSION {
        return Err(layer_error(
            kind,
            path,
            format!("unsupported config_version {version}; expected {CONFIG_SCHEMA_VERSION}"),
        ));
    }
    Ok(())
}

fn merge_table(target: &mut Table, overlay: &Table) {
    for (key, overlay_item) in overlay {
        let Some(target_item) = target.get_mut(key) else {
            target.insert(key, overlay_item.clone());
            continue;
        };
        let overlay_table = overlay_item
            .as_table()
            .cloned()
            .or_else(|| overlay_item.as_inline_table().map(table_from_inline));
        let Some(overlay_table) = overlay_table else {
            *target_item = overlay_item.clone();
            continue;
        };
        if target_item.as_table().is_none() {
            if let Some(inline_table) = target_item.as_inline_table().cloned() {
                *target_item = Item::Table(table_from_inline(&inline_table));
            } else {
                *target_item = overlay_item.clone();
                continue;
            }
        }
        if let Some(target_table) = target_item.as_table_mut() {
            merge_table(target_table, &overlay_table);
        } else {
            *target_item = overlay_item.clone();
        }
    }
}

fn table_from_inline(inline: &InlineTable) -> Table {
    let mut table = Table::new();
    for (key, value) in inline {
        table.insert(key, item_from_value(value));
    }
    table
}

fn item_from_value(value: &Value) -> Item {
    if let Some(inline) = value.as_inline_table() {
        Item::Table(table_from_inline(inline))
    } else {
        Item::Value(value.clone())
    }
}

fn layer_error(kind: ConfigLayerKind, path: &Path, error: impl std::fmt::Display) -> StateError {
    StateError::InvalidConfig(format!(
        "{} config layer {}: {error}",
        layer_name(kind),
        path.display()
    ))
}

const fn layer_name(kind: ConfigLayerKind) -> &'static str {
    match kind {
        ConfigLayerKind::Global => "global",
        ConfigLayerKind::Repository => "repository",
        ConfigLayerKind::Environment => "environment",
        ConfigLayerKind::Cli => "cli",
        ConfigLayerKind::LegacyRepository => "legacy repository",
    }
}
