#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};

use anyhow::{Context as _, Result};
use serde_json::Value;

struct CliFixture {
    temp: tempfile::TempDir,
    repository: PathBuf,
    colay_home: PathBuf,
}

impl CliFixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let repository = temp.path().join("repository");
        let colay_home = temp.path().join("home/.colay");
        fs::create_dir_all(&repository)?;
        Ok(Self {
            temp,
            repository,
            colay_home,
        })
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        Command::new(env!("CARGO_BIN_EXE_colay"))
            .args(args)
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("PATH", fake_provider_path()?)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root()?)
            .env("TEMP", self.temp.path())
            .env("TMP", self.temp.path())
            .output()
            .context("failed to invoke colay")
    }

    fn configure_fake_codex(&self) -> Result<()> {
        fs::create_dir_all(&self.colay_home)?;
        fs::write(
            self.colay_home.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(&fake_provider_binary())
            ),
        )?;
        Ok(())
    }
}

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn fake_provider_path() -> Result<PathBuf> {
    fake_provider_binary()
        .parent()
        .context("fake provider binary has no parent directory")
        .map(Path::to_path_buf)
}

fn system_root() -> Result<PathBuf> {
    #[cfg(windows)]
    {
        env::var_os("SystemRoot")
            .map(PathBuf::from)
            .context("SystemRoot must be set for Windows subprocess tests")
    }

    #[cfg(not(windows))]
    {
        Ok(PathBuf::from("/"))
    }
}

fn toml_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

#[test]
fn doctor_uses_defaults_without_creating_repository_state() -> Result<()> {
    let fixture = CliFixture::new()?;

    let output = fixture.colay(["--json", "doctor"])?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    let json: Value = serde_json::from_slice(&output.stdout)?;
    assert_eq!(json["data"]["checks"][0]["status"], "pass");
    Ok(())
}

#[test]
fn first_plan_only_run_initializes_local_state() -> Result<()> {
    let fixture = CliFixture::new()?;

    let output = fixture.colay(["run", "inspect repository", "--plan-only"])?;

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(fixture.repository.join(".colay/orchestrator.db").is_file());
    assert!(fixture.repository.join(".colay/events.jsonl").is_file());
    Ok(())
}

#[test]
fn compatibility_and_status_do_not_create_repository_state() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_fake_codex()?;

    let compatibility = fixture.colay(["--json", "compatibility"])?;
    assert!(
        compatibility.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&compatibility.stderr)
    );
    let status = fixture.colay(["--json", "status"])?;
    assert!(
        status.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(!fixture.repository.join(".colay").exists());
    Ok(())
}
