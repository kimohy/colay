#![cfg(feature = "test-fixtures")]

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use anyhow::{Context as _, Result, bail};
use orchestrator_domain::{
    Checkpoint, EventType, HandoverAcknowledgement, HandoverBundle, ProviderId, TaskEvent, TaskId,
};
use orchestrator_state::{MigrationManager, ensure_private_file};
use rusqlite::Connection;
use serde_json::{Value, json};

fn colay_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay"))
}

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn run_command(executable: &Path, cwd: &Path, args: &[OsString]) -> Result<Output> {
    let output = Command::new(executable)
        .current_dir(cwd)
        .args(args)
        .output()
        .with_context(|| format!("failed to start {}", executable.display()))?;
    if !output.status.success() {
        bail!(
            "{} {:?} failed with {}\nstdout:\n{}\nstderr:\n{}",
            executable.display(),
            args,
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(output)
}

fn git(cwd: &Path, args: &[&str]) -> Result<Output> {
    run_command(
        Path::new("git"),
        cwd,
        &args.iter().map(OsString::from).collect::<Vec<_>>(),
    )
}

fn run_cli(repository: &Path, config: &Path, args: &[OsString]) -> Result<Value> {
    let mut full_args = vec![
        OsString::from("--config"),
        config.as_os_str().to_owned(),
        OsString::from("--json"),
    ];
    full_args.extend_from_slice(args);
    let output = run_command(&colay_binary(), repository, &full_args)?;
    serde_json::from_slice(&output.stdout).context("CLI stdout was not one JSON envelope")
}

fn toml_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

fn install_fake_provider_config(config_path: &Path) -> Result<()> {
    let mut config = fs::read_to_string(config_path)?;
    let fake = toml_path(&fake_provider_binary());
    write!(
        config,
        "\n[orchestrator]\ndefault_timeout_minutes = 1\n\
         \n[orchestrator.providers.gemini]\nexecutable = {fake}\npriority = 0\n\
         \n[orchestrator.providers.codex]\nexecutable = {fake}\n\
         \n[orchestrator.providers.claude]\nexecutable = {fake}\n\
         \n[orchestrator.model_profiles.codex.economy]\nmodel = \"\"\neffort = \"low\"\n\
         \n[orchestrator.model_profiles.claude.economy]\nmodel = \"configured-by-test\"\neffort = \"low\"\n\
         \n[orchestrator.model_profiles.gemini.economy]\nmodel = \"configured-by-test\"\n\
         \n[features]\ncodex_app_server_adapter = false\n"
    )?;
    fs::write(config_path, config)?;
    ensure_private_file(config_path)?;
    Ok(())
}

fn initialize_repository(repository: &Path) -> Result<PathBuf> {
    fs::create_dir_all(repository.join("src"))?;
    fs::write(
        repository.join("Cargo.toml"),
        "[package]\nname = \"handover-fixture\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    fs::write(
        repository.join("src/lib.rs"),
        "pub fn answer() -> u32 { 1 }\n\n#[cfg(test)]\nmod tests {\n    use super::answer;\n\n    #[test]\n    fn returns_answer() {\n        assert_eq!(answer(), 42);\n    }\n}\n",
    )?;
    fs::write(repository.join(".gitignore"), ".colay/\ntarget/\n")?;
    git(repository, &["init"])?;
    git(repository, &["config", "user.name", "Orchestrator E2E"])?;
    git(
        repository,
        &["config", "user.email", "orchestrator-e2e@example.invalid"],
    )?;
    git(
        repository,
        &["add", "Cargo.toml", "src/lib.rs", ".gitignore"],
    )?;
    git(repository, &["commit", "-m", "fixture base"])?;

    let config = repository.join(".colay/config.toml");
    run_cli(
        repository,
        &config,
        &[
            OsString::from("init"),
            OsString::from("--repository"),
            OsString::from("."),
        ],
    )?;
    install_fake_provider_config(&config)?;
    Ok(config)
}

fn write_task_file(repository: &Path) -> Result<PathBuf> {
    let path = repository.join(".colay/task.json");
    let task = json!({
        "schema_version": "1",
        "objective": "Implement `src/lib.rs` safely; scenario:codex-quota",
        "original_request": "Implement the fixture and preserve partial work; scenario:codex-quota",
        "constraints": ["Use only the isolated task worktree"],
        "acceptance_criteria": ["cargo test passes"],
        "allowed_write_paths": ["src/lib.rs", "src/partial.txt"],
        "repository_wide_write_scope": false
    });
    fs::write(&path, serde_json::to_vec_pretty(&task)?)?;
    ensure_private_file(&path)?;
    Ok(path)
}

fn verification_stderr(repository: &Path, output: &Value) -> Result<String> {
    const DIAGNOSTIC_LIMIT: usize = 8 * 1024;
    const TRUNCATED_SUFFIX: &str = "\n...[diagnostics truncated]\n";

    let Some(task_id) = output.pointer("/data/task_id").and_then(Value::as_str) else {
        return Ok(String::new());
    };
    let task_id = task_id
        .parse::<TaskId>()
        .context("run output contained an invalid task_id")?;
    let commands = repository
        .join(".colay/results")
        .join(task_id.to_string())
        .join("commands");
    if !commands.is_dir() {
        return Ok(String::new());
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(&commands)? {
        let path = entry?.path();
        let is_stderr_log = path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.ends_with(".stderr.log"));
        if is_stderr_log && fs::symlink_metadata(&path)?.file_type().is_file() {
            paths.push(path);
        }
    }
    paths.sort();

    let mut diagnostics = String::new();
    for path in paths {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .context("verification stderr artifact had a non-UTF-8 name")?;
        diagnostics.push_str(name);
        diagnostics.push_str(":\n");
        diagnostics.push_str(&fs::read_to_string(path)?);
    }
    if diagnostics.len() > DIAGNOSTIC_LIMIT {
        let mut end = DIAGNOSTIC_LIMIT - TRUNCATED_SUFFIX.len();
        while !diagnostics.is_char_boundary(end) {
            end -= 1;
        }
        diagnostics.truncate(end);
        diagnostics.push_str(TRUNCATED_SUFFIX);
    }
    Ok(diagnostics)
}

#[test]
fn verification_stderr_reports_redacted_command_logs() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let repository = temporary.path().join("repository");
    let task_id = TaskId::new();
    let commands = repository
        .join(".colay/results")
        .join(task_id.to_string())
        .join("commands");
    fs::create_dir_all(&commands)?;
    fs::write(commands.join("b.stderr.log"), "second failure\n")?;
    fs::write(commands.join("a.stderr.log"), "first failure\n")?;
    fs::write(commands.join("a.stdout.log"), "excluded output\n")?;
    let output = json!({"data": {"task_id": task_id.to_string()}});

    let diagnostics = verification_stderr(&repository, &output)?;

    assert!(diagnostics.contains("a.stderr.log:\nfirst failure"));
    assert!(diagnostics.contains("b.stderr.log:\nsecond failure"));
    assert!(diagnostics.find("a.stderr.log") < diagnostics.find("b.stderr.log"));
    assert!(!diagnostics.contains("excluded output"));
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn real_cli_preserves_partial_diff_and_completes_codex_to_claude_handover() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let repository = fs::canonicalize(temporary.path())?.join("repository");
    fs::create_dir_all(&repository)?;
    let config = initialize_repository(&repository)?;
    let task_file = write_task_file(&repository)?;

    let output = run_cli(
        &repository,
        &config,
        &[
            OsString::from("run"),
            OsString::from("--task-file"),
            task_file.as_os_str().to_owned(),
        ],
    )?;
    let verification_stderr = verification_stderr(&repository, &output)?;
    assert_eq!(
        output.get("command").and_then(Value::as_str),
        Some("run_completed"),
        "unexpected CLI output: {}\nverification stderr:\n{}",
        serde_json::to_string_pretty(&output)?,
        verification_stderr
    );
    let data = output
        .get("data")
        .context("run output did not contain data")?;
    let task_id = data
        .get("task_id")
        .and_then(Value::as_str)
        .context("run output did not contain task_id")?;
    let worktree = PathBuf::from(
        data.pointer("/worktree/path")
            .and_then(Value::as_str)
            .context("run output did not contain the worktree path")?,
    );
    assert!(worktree.is_dir());
    assert_eq!(
        fs::read_to_string(worktree.join("src/partial.txt"))?,
        "partial work preserved across handover\n"
    );
    assert!(
        fs::read_to_string(worktree.join("src/lib.rs"))?.contains("answer() -> u32 {\n    42\n}")
    );
    assert!(fs::read_to_string(repository.join("src/lib.rs"))?.contains("answer() -> u32 { 1 }"));
    assert!(!repository.join("src/partial.txt").exists());

    let database_path = repository.join(".colay/orchestrator.db");
    let database = Connection::open(&database_path)?;
    let state: String = database.query_row(
        "SELECT state FROM tasks WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )?;
    assert_eq!(state, "completed");

    let attempts = {
        let mut statement = database.prepare(
            "SELECT provider_id, worker_mode, outcome FROM task_attempts \
             WHERE task_id = ?1 ORDER BY ordinal",
        )?;
        statement
            .query_map([task_id], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?
    };
    assert_eq!(
        attempts,
        vec![
            (
                "codex".to_owned(),
                "workspace_write".to_owned(),
                "quota_exceeded".to_owned(),
            ),
            (
                "claude".to_owned(),
                "read_only".to_owned(),
                "succeeded".to_owned(),
            ),
            (
                "claude".to_owned(),
                "workspace_write".to_owned(),
                "succeeded".to_owned(),
            ),
        ]
    );

    let checkpoint_json: String = database.query_row(
        "SELECT checkpoint_json FROM checkpoints WHERE task_id = ?1",
        [task_id],
        |row| row.get(0),
    )?;
    let checkpoint: Checkpoint = serde_json::from_str(&checkpoint_json)?;
    assert!(checkpoint.verify_integrity()?);
    assert_eq!(checkpoint.current_worker, ProviderId::Codex);
    assert!(
        checkpoint
            .files_changed
            .iter()
            .any(|path| path.to_string() == "src/partial.txt")
    );

    let (bundle_json, acknowledgement_json): (String, String) = database.query_row(
        "SELECT bundle_json, acknowledgement_json FROM handovers WHERE task_id = ?1",
        [task_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let bundle: HandoverBundle = serde_json::from_str(&bundle_json)?;
    let acknowledgement: HandoverAcknowledgement = serde_json::from_str(&acknowledgement_json)?;
    assert!(bundle.verify_integrity()?);
    assert_eq!(bundle.current_worker, ProviderId::Codex);
    assert_eq!(bundle.recommended_next_worker, ProviderId::Claude);
    assert!(acknowledgement.matches(&bundle));

    let events_path = repository.join(".colay/events.jsonl");
    let events = fs::read_to_string(events_path)?
        .lines()
        .map(serde_json::from_str::<TaskEvent>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut previous_hash = None;
    for event in &events {
        assert_eq!(event.previous_hash, previous_hash);
        assert!(event.verify_hash()?);
        previous_hash = Some(event.event_hash.clone());
    }
    for expected in [
        EventType::ProviderExhausted,
        EventType::CheckpointCreated,
        EventType::HandoverStarted,
        EventType::HandoverCompleted,
        EventType::VerificationCompleted,
        EventType::TaskCompleted,
    ] {
        assert!(events.iter().any(|event| event.event_type == expected));
    }

    let porcelain = git(&repository, &["status", "--porcelain"])?;
    assert!(porcelain.stdout.is_empty());
    assert_eq!(
        data.get("cleanup_requires_user_approval"),
        Some(&Value::Bool(true))
    );
    assert_eq!(data.get("automatic_merge"), Some(&Value::Bool(false)));
    assert_eq!(data.get("automatic_push"), Some(&Value::Bool(false)));
    Ok(())
}

#[test]
fn real_cli_applies_an_explicitly_approved_sealed_database_rollback() -> Result<()> {
    let temporary = tempfile::tempdir()?;
    let repository = fs::canonicalize(temporary.path())?.join("repository");
    fs::create_dir_all(&repository)?;
    let config = initialize_repository(&repository)?;
    let state_root = repository.join(".colay");
    let database_path = state_root.join("orchestrator.db");
    let backup_path = state_root.join("backups/orchestrator.db.backup.rollback-e2e");

    let database = Connection::open(&database_path)?;
    MigrationManager::backup(&database, &backup_path)?;
    database.execute(
        "INSERT INTO provider_health(
            health_id, provider_id, status, consecutive_failures, details_json, checked_at
         ) VALUES ('rollback-marker', 'codex', 'healthy', 0, '{}',
                   '2026-07-18T00:00:00Z')",
        [],
    )?;
    drop(database);

    let plan_output = run_cli(
        &repository,
        &config,
        &[
            OsString::from("migrate"),
            OsString::from("rollback"),
            OsString::from("plan"),
            OsString::from("--backup"),
            backup_path.as_os_str().to_owned(),
        ],
    )?;
    assert_eq!(
        plan_output.get("command").and_then(Value::as_str),
        Some("migrate_rollback_plan")
    );
    let plan_hash = plan_output
        .pointer("/data/plan/integrity_hash")
        .and_then(Value::as_str)
        .context("rollback plan omitted its integrity hash")?;

    let apply_output = run_cli(
        &repository,
        &config,
        &[
            OsString::from("migrate"),
            OsString::from("rollback"),
            OsString::from("apply"),
            OsString::from("--plan-hash"),
            OsString::from(plan_hash),
            OsString::from("--approved-by"),
            OsString::from("enterprise-e2e-admin"),
        ],
    )?;
    assert_eq!(
        apply_output.get("command").and_then(Value::as_str),
        Some("migrate_rollback_apply"),
        "unexpected CLI output: {}",
        serde_json::to_string_pretty(&apply_output)?
    );
    assert_eq!(
        apply_output.pointer("/data/audit_event_recorded"),
        Some(&Value::Bool(true))
    );

    let restored = Connection::open(&database_path)?;
    let marker_count: i64 = restored.query_row(
        "SELECT count(*) FROM provider_health WHERE health_id = 'rollback-marker'",
        [],
        |row| row.get(0),
    )?;
    assert_eq!(marker_count, 0);
    drop(restored);
    let recovery = apply_output
        .pointer("/data/execution/recovery_backup_path")
        .and_then(Value::as_str)
        .context("rollback result omitted its recovery backup")?;
    assert!(Path::new(recovery).is_file());

    let events = fs::read_to_string(state_root.join("events.jsonl"))?
        .lines()
        .map(serde_json::from_str::<TaskEvent>)
        .collect::<std::result::Result<Vec<_>, _>>()?;
    assert!(
        events
            .iter()
            .any(|event| event.event_type == EventType::RollbackPlanned)
    );
    assert!(events.iter().any(|event| {
        event.event_type == EventType::MigrationCompleted
            && event.payload.get("operation").and_then(Value::as_str) == Some("rollback")
    }));
    let mut previous_hash = None;
    for event in events {
        assert_eq!(event.previous_hash, previous_hash);
        assert!(event.verify_hash()?);
        previous_hash = Some(event.event_hash);
    }
    Ok(())
}
