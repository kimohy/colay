use std::fs;

use chrono::{TimeZone as _, Utc};
use orchestrator_state::{
    CONFIG_SCHEMA_VERSION, ConfigDocument, MigratableConfigDocument, StateError,
};

type TestResult = Result<(), Box<dyn std::error::Error>>;

const LEGACY_V1: &str = r#"# root comment must survive
config_version = 1 # version comment must survive
future_root_field = "keep-me"

[orchestrator] # section comment must survive
enabled = true
state_dir = ".colay"
timezone = "Asia/Seoul"
warning_threshold_percent = 30
handover_threshold_percent = 15
critical_reserve_percent = 15
minimum_progress = 0.05
forecast_alpha = 0.3
minimum_forecast_observations = 3
future_orchestrator_field = "keep-me-too" # unknown comment

[orchestrator.providers.codex]
enabled = true
executable = "codex"
quota_period = "calendar_month"
reset_day = 1
reset_timezone = "Asia/Seoul"
future_provider_field = true
"#;

#[test]
fn plans_and_dry_runs_every_intermediate_version() -> TestResult {
    assert!(ConfigDocument::parse(LEGACY_V1).is_err());
    let legacy = MigratableConfigDocument::parse(LEGACY_V1)?;
    let plan = legacy.plan()?;

    assert_eq!(plan.current_version, 1);
    assert_eq!(plan.target_version, CONFIG_SCHEMA_VERSION);
    assert_eq!(
        plan.steps
            .iter()
            .map(|step| (step.from_version, step.to_version))
            .collect::<Vec<_>>(),
        [(1, 2), (2, 3), (3, 4)]
    );
    assert!(!plan.destructive);

    let preview = legacy.dry_run()?;
    assert_eq!(
        legacy.current_version(),
        1,
        "dry-run must not mutate source"
    );
    assert_eq!(preview.result().initial_version, 1);
    assert_eq!(preview.result().final_version, 4);
    assert!(preview.result().changed);
    assert_eq!(preview.result().applied_steps, plan.steps);
    assert_eq!(preview.migrated().config().config_version, 4);
    assert!(preview.migrated().config().orchestrator.automatic_routing);
    assert!(
        preview
            .migrated()
            .config()
            .orchestrator
            .redaction
            .patterns
            .is_empty()
    );

    let rendered = preview.migrated().document().to_string();
    for preserved in [
        "# root comment must survive",
        "# version comment must survive",
        "# section comment must survive",
        "future_root_field = \"keep-me\"",
        "future_orchestrator_field = \"keep-me-too\" # unknown comment",
        "future_provider_field = true",
    ] {
        assert!(rendered.contains(preserved), "missing `{preserved}`");
    }
    assert!(rendered.contains("automatic_routing = true"));
    assert!(rendered.contains("patterns = []"));
    Ok(())
}

#[test]
fn does_not_replace_fields_already_present_in_legacy_input() -> TestResult {
    let legacy_with_values = LEGACY_V1
        .replace(
            "enabled = true\n",
            "enabled = true\nautomatic_routing = false\n",
        )
        .replace(
            "[orchestrator.providers.codex]",
            "[orchestrator.redaction]\npatterns = [\"COMPANY-[0-9]+\"]\n\n[orchestrator.providers.codex]",
        );
    let legacy = MigratableConfigDocument::parse(&legacy_with_values)?;
    let preview = legacy.dry_run()?;

    assert!(!preview.migrated().config().orchestrator.automatic_routing);
    assert_eq!(
        preview.migrated().config().orchestrator.redaction.patterns,
        ["COMPANY-[0-9]+"]
    );
    assert_eq!(
        preview.migrated().config().orchestrator.state_dir,
        std::path::Path::new(".colay")
    );
    Ok(())
}

#[test]
fn applies_only_the_pending_step_and_preserves_existing_values() -> TestResult {
    let v2 = LEGACY_V1
        .replace("config_version = 1", "config_version = 2")
        .replace(
            "enabled = true\n",
            "enabled = true\nautomatic_routing = false\n",
        );
    let legacy = MigratableConfigDocument::parse(&v2)?;
    let preview = legacy.dry_run()?;

    assert_eq!(
        preview
            .result()
            .applied_steps
            .iter()
            .map(|step| (step.from_version, step.to_version))
            .collect::<Vec<_>>(),
        [(2, 3), (3, 4)]
    );
    assert!(!preview.migrated().config().orchestrator.automatic_routing);
    Ok(())
}

#[test]
fn rejects_future_and_invalid_versions_without_guessing() {
    let future = LEGACY_V1.replace("config_version = 1", "config_version = 5");
    assert!(matches!(
        MigratableConfigDocument::parse(&future),
        Err(StateError::FutureSchema {
            found: 5,
            supported: CONFIG_SCHEMA_VERSION
        })
    ));

    let zero = LEGACY_V1.replace("config_version = 1", "config_version = 0");
    assert!(matches!(
        MigratableConfigDocument::parse(&zero),
        Err(StateError::InvalidConfig(_))
    ));

    let missing = LEGACY_V1.replace("config_version = 1 # version comment must survive\n", "");
    assert!(matches!(
        MigratableConfigDocument::parse(&missing),
        Err(StateError::InvalidConfig(_))
    ));
}

#[test]
fn file_apply_requires_and_verifies_a_sibling_backup() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = fs::canonicalize(temporary.path())?;
    let path = root.join("config.toml");
    fs::write(&path, LEGACY_V1)?;
    let migration = MigratableConfigDocument::load(&path)?;
    let timestamp = Utc
        .with_ymd_and_hms(2026, 7, 18, 12, 30, 0)
        .single()
        .ok_or("valid timestamp")?;

    let applied = migration.apply_to_file(&path, timestamp)?;
    let backup = applied
        .backup_path
        .ok_or("changed config requires backup")?;

    assert_eq!(applied.result.initial_version, 1);
    assert_eq!(applied.result.final_version, 4);
    assert_eq!(fs::read_to_string(&backup)?, LEGACY_V1);
    assert!(backup.file_name().is_some_and(|name| {
        name.to_string_lossy()
            .starts_with("config.toml.backup.20260718T123000")
    }));
    let current = ConfigDocument::load(&path)?;
    assert_eq!(current.config().config_version, CONFIG_SCHEMA_VERSION);
    Ok(())
}

#[test]
fn file_apply_refuses_a_source_changed_after_planning() -> TestResult {
    let temporary = tempfile::tempdir()?;
    let root = fs::canonicalize(temporary.path())?;
    let path = root.join("config.toml");
    fs::write(&path, LEGACY_V1)?;
    let migration = MigratableConfigDocument::load(&path)?;
    fs::write(
        &path,
        LEGACY_V1.replace("enabled = true", "enabled = false"),
    )?;

    let result = migration.apply_to_file(&path, Utc::now());
    assert!(matches!(result, Err(StateError::InvalidConfig(_))));
    let backups = fs::read_dir(&root)?
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().contains(".backup."))
        .count();
    assert_eq!(backups, 0);
    Ok(())
}

#[test]
fn current_config_dry_run_is_idempotent_and_does_not_create_a_backup() -> TestResult {
    let current = LEGACY_V1
        .replace("config_version = 1", "config_version = 4")
        .replace(
            "enabled = true\n",
            "enabled = true\nautomatic_routing = true\n",
        )
        .replace(
            "[orchestrator.providers.codex]",
            "[orchestrator.redaction]\npatterns = []\n\n[orchestrator.providers.codex]",
        );
    let migration = MigratableConfigDocument::parse(&current)?;
    let preview = migration.dry_run()?;

    assert!(!preview.result().changed);
    assert!(preview.result().applied_steps.is_empty());
    assert_eq!(preview.migrated().document().to_string(), current);
    Ok(())
}

#[test]
fn legacy_v1_through_v3_without_state_dir_materialize_the_legacy_path() -> TestResult {
    for version in 1..=3 {
        let legacy_without_state_dir = LEGACY_V1
            .replace("config_version = 1", &format!("config_version = {version}"))
            .replace("state_dir = \".colay\"\n", "");
        let legacy = MigratableConfigDocument::parse(&legacy_without_state_dir)?;
        let preview = legacy.dry_run()?;

        assert!(preview.plan().steps.iter().any(|step| {
            step.from_version == 3
                && step.to_version == 4
                && step.name == "legacy_state_dir_materialization"
        }));
        assert_eq!(
            preview.migrated().config().orchestrator.state_dir,
            std::path::Path::new(".codex/orchestrator")
        );
        assert!(
            preview
                .migrated()
                .document()
                .to_string()
                .contains("state_dir = \".codex/orchestrator\"")
        );
        assert!(!legacy_without_state_dir.contains("state_dir"));
    }

    let current_without_state_dir = LEGACY_V1
        .replace("config_version = 1", "config_version = 4")
        .replace("state_dir = \".colay\"\n", "");
    let current = ConfigDocument::parse(&current_without_state_dir)?;
    assert_eq!(
        current.config().orchestrator.state_dir,
        std::path::Path::new(".colay")
    );
    Ok(())
}
