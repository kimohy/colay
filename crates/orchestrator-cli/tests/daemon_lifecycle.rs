#![cfg(feature = "test-fixtures")]

use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, ExitStatus, Output, Stdio},
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context as _, Result, bail};
use serde_json::Value;

struct CliFixture {
    _temp: tempfile::TempDir,
    temp_root: PathBuf,
    repository: PathBuf,
    colay_home: PathBuf,
}

impl CliFixture {
    fn new() -> Result<Self> {
        let temp = tempfile::tempdir()?;
        let temp_root = fs::canonicalize(temp.path())?;
        let repository = temp_root.join("repository");
        let colay_home = temp_root.join("home/.colay");
        fs::create_dir_all(&repository)?;
        Ok(Self {
            _temp: temp,
            temp_root,
            repository,
            colay_home,
        })
    }

    fn colay<const N: usize>(&self, args: [&str; N]) -> Result<Output> {
        self.colay_with_env(&args, &[])
    }

    fn colay_with_env(&self, args: &[&str], extra_env: &[(&str, &str)]) -> Result<Output> {
        self.colay_from_with_env(&self.repository, args, extra_env)
    }

    fn colay_from(&self, repository: &Path, args: &[&str]) -> Result<Output> {
        self.colay_from_with_env(repository, args, &[])
    }

    fn colay_from_with_env(
        &self,
        repository: &Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<Output> {
        #[cfg(windows)]
        let system_root = system_root()?;
        #[cfg(not(windows))]
        let system_root = system_root();
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary has no parent directory")?;
        let mut command = Command::new(&executable);
        command
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", executable_parent)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp_root)
            .env("TMP", &self.temp_root);
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command.output().context("failed to invoke colay")
    }

    fn configure_slow_fake_codex(&self, delay_ms: u64) -> Result<()> {
        let executable = self.slow_fake_codex(delay_ms)?;
        self.configure_fake_codex_executable(&executable)
    }

    fn slow_fake_codex(&self, delay_ms: u64) -> Result<PathBuf> {
        let source = fake_provider_binary();
        let extension = source.extension().and_then(|value| value.to_str());
        let file_name = extension.map_or_else(
            || format!("fake-provider-probe-delay-{delay_ms}"),
            |extension| format!("fake-provider-probe-delay-{delay_ms}.{extension}"),
        );
        let executable = self.temp_root.join(file_name);
        fs::copy(source, &executable)?;
        Ok(executable)
    }

    fn configure_fake_codex_executable(&self, executable: &Path) -> Result<()> {
        fs::create_dir_all(&self.colay_home)?;
        fs::write(
            self.colay_home.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(executable)
            ),
        )?;
        Ok(())
    }

    fn configure_repository_fake_codex(repository: &Path, executable: &Path) -> Result<()> {
        let config_dir = repository.join(".colay");
        fs::create_dir_all(&config_dir)?;
        fs::write(
            config_dir.join("config.toml"),
            format!(
                "config_version = 4\n[orchestrator.providers.codex]\nexecutable = {}\n",
                toml_path(executable)
            ),
        )?;
        Ok(())
    }

    fn json(&self, args: &[&str]) -> Result<Value> {
        #[cfg(windows)]
        let system_root = system_root()?;
        #[cfg(not(windows))]
        let system_root = system_root();
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary has no parent directory")?;
        let output = Command::new(&executable)
            .arg("--json")
            .args(args)
            .current_dir(&self.repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("PATH", executable_parent)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp_root)
            .env("TMP", &self.temp_root)
            .output()?;
        if !output.status.success() {
            bail!("colay failed: {}", String::from_utf8_lossy(&output.stderr));
        }
        serde_json::from_slice(&output.stdout).context("colay did not emit JSON")
    }

    fn invoke_without_capture(&self, args: &[&str]) -> Result<ExitStatus> {
        self.invoke_with_env_without_capture(args, &[])
    }

    fn invoke_with_env_without_capture(
        &self,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<ExitStatus> {
        self.invoke_from_with_env_without_capture(&self.repository, args, extra_env)
    }

    fn invoke_from_without_capture(&self, repository: &Path, args: &[&str]) -> Result<ExitStatus> {
        self.invoke_from_with_env_without_capture(repository, args, &[])
    }

    fn invoke_from_with_env_without_capture(
        &self,
        repository: &Path,
        args: &[&str],
        extra_env: &[(&str, &str)],
    ) -> Result<ExitStatus> {
        #[cfg(windows)]
        let system_root = system_root()?;
        #[cfg(not(windows))]
        let system_root = system_root();
        let executable = PathBuf::from(env!("CARGO_BIN_EXE_colay"));
        let executable_parent = executable
            .parent()
            .context("colay binary has no parent directory")?;
        let mut command = Command::new(&executable);
        command
            .args(args)
            .current_dir(repository)
            .env_clear()
            .env("COLAY_HOME", &self.colay_home)
            .env("COLAY_TEST_FAKE_PROVIDERS_ONLY", "1")
            .env("PATH", executable_parent)
            .env("PATHEXT", ".EXE;.CMD")
            .env("SystemRoot", system_root)
            .env("TEMP", &self.temp_root)
            .env("TMP", &self.temp_root)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        for (name, value) in extra_env {
            command.env(name, value);
        }
        command
            .status()
            .context("failed to invoke colay without capture")
    }

    fn wait_for_state(&self, expected: &str, timeout: Duration) -> Result<Value> {
        let started = Instant::now();
        loop {
            let status = self.json(&["daemon", "status"])?;
            if status["data"]["status"]["state"] == expected {
                return Ok(status);
            }
            if started.elapsed() >= timeout {
                bail!("daemon did not reach {expected}: {status}");
            }
            thread::sleep(Duration::from_millis(25));
        }
    }

    fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
        let started = Instant::now();
        while !path.exists() {
            if started.elapsed() >= timeout {
                bail!("path did not appear before timeout: {}", path.display());
            }
            thread::sleep(Duration::from_millis(25));
        }
        Ok(())
    }
}

fn fake_provider_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_colay-e2e-fake-provider"))
}

fn toml_path(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.to_string_lossy()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

fn fake_probe_marker(executable: &Path) -> Result<PathBuf> {
    let mut marker = executable.to_path_buf();
    let mut file_name = marker
        .file_name()
        .context("fake provider path has no file name")?
        .to_os_string();
    file_name.push(".probe-started");
    marker.set_file_name(file_name);
    Ok(marker)
}

impl Drop for CliFixture {
    fn drop(&mut self) {
        let _ = self.colay(["daemon", "stop"]);
    }
}

#[cfg(windows)]
fn system_root() -> Result<PathBuf> {
    env::var_os("SystemRoot")
        .map(PathBuf::from)
        .context("SystemRoot must be set for Windows subprocess tests")
}

#[cfg(not(windows))]
fn system_root() -> PathBuf {
    PathBuf::from("/")
}

#[test]
fn daemon_start_status_stop_and_idempotent_start() -> Result<()> {
    let fixture = CliFixture::new()?;
    let absent = fixture.json(&["daemon", "status"])?;
    assert_eq!(absent["data"]["status"]["state"], "stopped");
    assert!(!fixture.repository.join(".colay").exists());

    let initialized = fixture.colay(["init"])?;
    assert!(
        initialized.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );
    assert!(
        fixture
            .invoke_without_capture(&["daemon", "start"])?
            .success()
    );
    let online = fixture.wait_for_state("online", Duration::from_secs(5))?;
    let instance_id = online["data"]["status"]["instance"]["instance_id"].clone();

    let repeated = fixture.json(&["daemon", "start"])?;
    assert_eq!(repeated["command"], "daemon_start");
    assert_eq!(
        repeated["data"]["status"]["instance"]["instance_id"],
        instance_id
    );
    assert!(
        fixture
            .invoke_without_capture(&["daemon", "restart"])?
            .success()
    );
    let restarted = fixture.wait_for_state("online", Duration::from_secs(5))?;
    assert_ne!(
        restarted["data"]["status"]["instance"]["instance_id"],
        instance_id
    );
    let stopped = fixture.json(&["daemon", "stop"])?;
    assert_eq!(stopped["command"], "daemon_stop");
    fixture.wait_for_state("stopped", Duration::from_secs(10))?;
    Ok(())
}

#[test]
fn daemon_help_hides_internal_serve_action() -> Result<()> {
    let fixture = CliFixture::new()?;
    let root = fixture.colay(["--help"])?;
    let root = String::from_utf8(root.stdout)?;
    assert!(root.contains("daemon"));
    let daemon = fixture.colay(["daemon", "--help"])?;
    let daemon = String::from_utf8(daemon.stdout)?;
    for action in ["start", "status", "stop", "restart"] {
        assert!(daemon.contains(action));
    }
    assert!(!daemon.contains("serve"));
    Ok(())
}

#[test]
fn slow_fake_provider_probe_does_not_make_start_fail() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_slow_fake_codex(6_000)?;
    let initialized = fixture.colay(["init"])?;
    assert!(
        initialized.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&initialized.stderr)
    );

    let started = fixture.invoke_without_capture(&["daemon", "start"])?;

    assert!(started.success());
    let status = fixture.json(&["daemon", "status"])?;
    assert_eq!(status["data"]["status"]["state"], "online");
    Ok(())
}

#[test]
fn restart_cancels_slow_secondary_workspace_activation() -> Result<()> {
    let fixture = CliFixture::new()?;
    fixture.configure_fake_codex_executable(&fake_provider_binary())?;
    let initialized = fixture.colay(["init"])?;
    assert!(initialized.status.success());
    assert!(
        fixture
            .invoke_without_capture(&["daemon", "start"])?
            .success()
    );

    let second_repository = fixture.temp_root.join("repository-two");
    fs::create_dir_all(&second_repository)?;
    let slow_fake = fixture.slow_fake_codex(15_000)?;
    let probe_marker = fake_probe_marker(&slow_fake)?;
    CliFixture::configure_repository_fake_codex(&second_repository, &slow_fake)?;
    let activation = fixture.colay_from(&second_repository, &["status"])?;
    assert!(
        activation.status.success(),
        "secondary workspace activation failed: {}",
        String::from_utf8_lossy(&activation.stderr)
    );
    CliFixture::wait_for_path(&probe_marker, Duration::from_secs(5))?;

    CliFixture::configure_repository_fake_codex(&second_repository, &fake_provider_binary())?;
    let restarted =
        fixture.invoke_from_without_capture(&second_repository, &["daemon", "restart"])?;
    assert!(
        restarted.success(),
        "restart waited for the cancelled activation"
    );
    fixture.wait_for_state("online", Duration::from_secs(5))?;
    Ok(())
}
