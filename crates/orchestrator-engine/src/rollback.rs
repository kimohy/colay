use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read as _, Write as _},
    path::{Component, Path, PathBuf},
};

use chrono::{DateTime, Utc};
use orchestrator_domain::{SchemaVersion, canonical_sha256};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};

use crate::{EngineError, EngineResult};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackStep {
    pub component: String,
    pub backup_source: PathBuf,
    pub destination: PathBuf,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackRecoveryPlan {
    pub schema_version: SchemaVersion,
    pub target_version: String,
    pub steps: Vec<RollbackStep>,
    #[serde(default)]
    pub backup_sha256: Vec<String>,
    pub preserved_state_paths: Vec<PathBuf>,
    pub created_at: DateTime<Utc>,
    pub integrity_hash: String,
}

impl RollbackRecoveryPlan {
    fn seal(mut self) -> EngineResult<Self> {
        self.integrity_hash.clear();
        self.integrity_hash = canonical_sha256(&self)?;
        Ok(self)
    }

    pub fn verify(&self) -> EngineResult<bool> {
        let mut candidate = self.clone();
        let expected = std::mem::take(&mut candidate.integrity_hash);
        Ok(!expected.is_empty() && canonical_sha256(&candidate)? == expected)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackApproval {
    pub approved_by: String,
    pub approved_at: DateTime<Utc>,
    pub plan_hash: String,
}

impl RollbackApproval {
    #[must_use]
    pub fn for_plan(
        plan: &RollbackRecoveryPlan,
        approved_by: impl Into<String>,
        approved_at: DateTime<Utc>,
    ) -> Self {
        Self {
            approved_by: approved_by.into(),
            approved_at,
            plan_hash: plan.integrity_hash.clone(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RollbackExecutionReport {
    pub recovery_backups: Vec<PathBuf>,
    pub journal_path: PathBuf,
}

#[derive(Clone, Debug)]
pub struct RollbackManager {
    allowed_roots: Vec<PathBuf>,
}

#[derive(Debug)]
struct PreparedStep {
    component: String,
    backup_source: PathBuf,
    destination: PathBuf,
    staged: PathBuf,
    recovery: PathBuf,
    failed_install: PathBuf,
}

#[derive(Clone, Copy, Serialize)]
struct JournalEntry<'a> {
    recorded_at: DateTime<Utc>,
    phase: &'a str,
    step_index: Option<usize>,
    component: Option<&'a str>,
    path: Option<&'a Path>,
    sha256: Option<&'a str>,
    detail: Option<&'a str>,
}

impl RollbackManager {
    pub fn new(allowed_roots: Vec<PathBuf>) -> EngineResult<Self> {
        let mut normalized = Vec::new();
        for path in allowed_roots {
            ensure_no_symlink_components(&path)?;
            let canonical =
                fs::canonicalize(&path).map_err(|error| EngineError::io(&path, error))?;
            if !canonical.is_dir() || is_broad_root(&canonical) {
                return Err(EngineError::Rollback(format!(
                    "broad or non-directory allowed root is forbidden: {}",
                    canonical.display()
                )));
            }
            if !normalized.contains(&canonical) {
                normalized.push(canonical);
            }
        }
        if normalized.is_empty() {
            return Err(EngineError::Rollback(
                "at least one narrow allowed root is required".to_owned(),
            ));
        }
        Ok(Self {
            allowed_roots: normalized,
        })
    }

    pub fn plan(
        &self,
        target_version: impl Into<String>,
        steps: Vec<RollbackStep>,
        preserved_state_paths: &[PathBuf],
        created_at: DateTime<Utc>,
    ) -> EngineResult<RollbackRecoveryPlan> {
        if steps.is_empty() {
            return Err(EngineError::Rollback(
                "rollback has no recovery steps".to_owned(),
            ));
        }
        self.ensure_allowed_roots_intact()?;
        let mut preserved_state_paths = preserved_state_paths
            .iter()
            .map(|path| self.normalize_preserved_path(path))
            .collect::<EngineResult<Vec<_>>>()?;
        preserved_state_paths.sort();
        preserved_state_paths.dedup();
        let mut destinations = BTreeSet::new();
        let mut normalized_steps = Vec::with_capacity(steps.len());
        let mut backup_sha256 = Vec::with_capacity(steps.len());
        for step in steps {
            let backup_source = self.normalize_file(&step.backup_source)?;
            let destination = self.normalize_destination(&step.destination)?;
            if backup_source == destination {
                return Err(EngineError::Rollback(format!(
                    "backup source and destination are identical: {}",
                    destination.display()
                )));
            }
            if !destinations.insert(destination.clone()) {
                return Err(EngineError::Rollback(format!(
                    "rollback destination appears more than once: {}",
                    destination.display()
                )));
            }
            Self::reject_preserved_overlap(&destination, &preserved_state_paths)?;
            backup_sha256.push(file_sha256(&backup_source)?);
            normalized_steps.push(RollbackStep {
                component: step.component,
                backup_source,
                destination,
            });
        }
        RollbackRecoveryPlan {
            schema_version: SchemaVersion::v1(),
            target_version: target_version.into(),
            steps: normalized_steps,
            backup_sha256,
            preserved_state_paths,
            created_at,
            integrity_hash: String::new(),
        }
        .seal()
    }

    /// Applies only an integrity-verified, explicitly approved plan. All sources are
    /// staged before the first destination is changed. Every mutation is journaled and
    /// a failure restores all previously changed destinations in reverse order.
    pub fn apply(
        &self,
        plan: &RollbackRecoveryPlan,
        approval: &RollbackApproval,
    ) -> EngineResult<RollbackExecutionReport> {
        self.apply_transaction(plan, approval, None)
    }

    #[allow(clippy::too_many_lines)]
    fn apply_transaction(
        &self,
        plan: &RollbackRecoveryPlan,
        approval: &RollbackApproval,
        fail_before_step: Option<usize>,
    ) -> EngineResult<RollbackExecutionReport> {
        self.validate_approved_plan(plan, approval)?;
        let token = plan
            .integrity_hash
            .get(..16)
            .ok_or_else(|| EngineError::Rollback("sealed plan hash is malformed".to_owned()))?;
        let prepared = self.prepare_steps(plan, token)?;
        let journal_path =
            self.allowed_roots[0].join(format!(".orchestrator-rollback-{token}.journal.jsonl"));
        Self::reject_preserved_overlap(&journal_path, &plan.preserved_state_paths)?;
        let mut journal = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&journal_path)
            .map_err(|error| EngineError::io(&journal_path, error))?;
        append_journal(
            &mut journal,
            JournalEntry {
                recorded_at: Utc::now(),
                phase: "transaction_started",
                step_index: None,
                component: None,
                path: None,
                sha256: Some(&plan.integrity_hash),
                detail: None,
            },
        )?;

        for (index, step) in prepared.iter().enumerate() {
            if let Err(error) = stage_file(&step.backup_source, &step.staged) {
                let message = format!(
                    "staging failed before any destination mutation: {error}; recovery journal: {}",
                    journal_path.display()
                );
                let _ = append_failure(&mut journal, index, step, &message);
                return Err(EngineError::Rollback(message));
            }
            let digest = match file_sha256(&step.staged) {
                Ok(digest) => digest,
                Err(error) => {
                    let message = format!(
                        "staged artifact could not be hashed: {error}; recovery journal: {}",
                        journal_path.display()
                    );
                    let _ = append_failure(&mut journal, index, step, &message);
                    return Err(EngineError::Rollback(message));
                }
            };
            if digest != plan.backup_sha256[index] {
                let message = format!(
                    "backup content changed after approval for component {}; recovery journal: {}",
                    step.component,
                    journal_path.display()
                );
                let _ = append_failure(&mut journal, index, step, &message);
                return Err(EngineError::Rollback(message));
            }
            if let Err(error) = append_journal(
                &mut journal,
                JournalEntry {
                    recorded_at: Utc::now(),
                    phase: "source_staged",
                    step_index: Some(index),
                    component: Some(&step.component),
                    path: Some(&step.staged),
                    sha256: Some(&digest),
                    detail: None,
                },
            ) {
                return Err(EngineError::Rollback(format!(
                    "staging journal failed: {error}; recovery journal: {}",
                    journal_path.display()
                )));
            }
        }

        let mut touched = Vec::new();
        for (index, step) in prepared.iter().enumerate() {
            if fail_before_step == Some(index) {
                let message = "injected rollback failure".to_owned();
                return Self::fail_and_recover(
                    &prepared,
                    &touched,
                    &mut journal,
                    &journal_path,
                    index,
                    &message,
                );
            }
            if step.destination.exists() {
                if let Err(error) = fs::rename(&step.destination, &step.recovery) {
                    let message = format!("failed to preserve current destination: {error}");
                    return Self::fail_and_recover(
                        &prepared,
                        &touched,
                        &mut journal,
                        &journal_path,
                        index,
                        &message,
                    );
                }
                touched.push(index);
                let recovery_digest = match file_sha256(&step.recovery) {
                    Ok(digest) => digest,
                    Err(error) => {
                        return Self::fail_and_recover(
                            &prepared,
                            &touched,
                            &mut journal,
                            &journal_path,
                            index,
                            &format!("preserved destination could not be hashed: {error}"),
                        );
                    }
                };
                if let Err(error) = append_journal(
                    &mut journal,
                    JournalEntry {
                        recorded_at: Utc::now(),
                        phase: "destination_preserved",
                        step_index: Some(index),
                        component: Some(&step.component),
                        path: Some(&step.recovery),
                        sha256: Some(&recovery_digest),
                        detail: None,
                    },
                ) {
                    return Self::fail_and_recover(
                        &prepared,
                        &touched,
                        &mut journal,
                        &journal_path,
                        index,
                        &error.to_string(),
                    );
                }
            } else {
                touched.push(index);
            }
            if let Err(error) = fs::rename(&step.staged, &step.destination) {
                let message = format!("failed to install staged rollback artifact: {error}");
                return Self::fail_and_recover(
                    &prepared,
                    &touched,
                    &mut journal,
                    &journal_path,
                    index,
                    &message,
                );
            }
            if let Err(error) = append_journal(
                &mut journal,
                JournalEntry {
                    recorded_at: Utc::now(),
                    phase: "rollback_artifact_installed",
                    step_index: Some(index),
                    component: Some(&step.component),
                    path: Some(&step.destination),
                    sha256: None,
                    detail: None,
                },
            ) {
                return Self::fail_and_recover(
                    &prepared,
                    &touched,
                    &mut journal,
                    &journal_path,
                    index,
                    &error.to_string(),
                );
            }
        }
        if let Err(error) = append_journal(
            &mut journal,
            JournalEntry {
                recorded_at: Utc::now(),
                phase: "transaction_completed",
                step_index: None,
                component: None,
                path: None,
                sha256: None,
                detail: None,
            },
        ) {
            let failed_index = prepared.len().saturating_sub(1);
            return Self::fail_and_recover(
                &prepared,
                &touched,
                &mut journal,
                &journal_path,
                failed_index,
                &format!("completion journal failed: {error}"),
            );
        }
        Ok(RollbackExecutionReport {
            recovery_backups: prepared
                .iter()
                .filter(|step| step.recovery.exists())
                .map(|step| step.recovery.clone())
                .collect(),
            journal_path,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn fail_and_recover(
        prepared: &[PreparedStep],
        touched: &[usize],
        journal: &mut File,
        journal_path: &Path,
        failed_index: usize,
        failure: &str,
    ) -> EngineResult<RollbackExecutionReport> {
        let _ = append_failure(journal, failed_index, &prepared[failed_index], failure);
        let mut recovery_failures = Vec::new();
        for index in touched.iter().rev().copied() {
            let step = &prepared[index];
            if step.destination.exists() {
                if let Err(error) = fs::rename(&step.destination, &step.failed_install) {
                    recovery_failures.push(format!(
                        "could not preserve failed install {}: {error}",
                        step.destination.display()
                    ));
                    continue;
                }
                let _ = append_journal(
                    journal,
                    JournalEntry {
                        recorded_at: Utc::now(),
                        phase: "failed_install_preserved",
                        step_index: Some(index),
                        component: Some(&step.component),
                        path: Some(&step.failed_install),
                        sha256: None,
                        detail: None,
                    },
                );
            }
            if step.recovery.exists() {
                if let Err(error) = fs::rename(&step.recovery, &step.destination) {
                    recovery_failures.push(format!(
                        "could not restore {}: {error}",
                        step.destination.display()
                    ));
                } else {
                    let _ = append_journal(
                        journal,
                        JournalEntry {
                            recorded_at: Utc::now(),
                            phase: "destination_restored",
                            step_index: Some(index),
                            component: Some(&step.component),
                            path: Some(&step.destination),
                            sha256: None,
                            detail: None,
                        },
                    );
                }
            }
        }
        let recovery_detail = if recovery_failures.is_empty() {
            "all touched destinations restored"
        } else {
            "one or more destinations require manual recovery"
        };
        let _ = append_journal(
            journal,
            JournalEntry {
                recorded_at: Utc::now(),
                phase: "transaction_recovery_completed",
                step_index: None,
                component: None,
                path: Some(journal_path),
                sha256: None,
                detail: Some(recovery_detail),
            },
        );
        let failures = if recovery_failures.is_empty() {
            String::new()
        } else {
            format!("; recovery failures: {}", recovery_failures.join(" | "))
        };
        Err(EngineError::Rollback(format!(
            "rollback transaction failed: {failure}; recovery journal: {}{failures}",
            journal_path.display()
        )))
    }

    fn validate_approved_plan(
        &self,
        plan: &RollbackRecoveryPlan,
        approval: &RollbackApproval,
    ) -> EngineResult<()> {
        if !plan.verify()?
            || approval.plan_hash != plan.integrity_hash
            || approval.approved_by.trim().is_empty()
        {
            return Err(EngineError::Rollback(
                "explicit approval does not match the sealed plan".to_owned(),
            ));
        }
        self.ensure_allowed_roots_intact()?;
        if plan.backup_sha256.len() != plan.steps.len() {
            return Err(EngineError::Rollback(
                "sealed rollback plan has incomplete backup integrity metadata".to_owned(),
            ));
        }
        for preserved in &plan.preserved_state_paths {
            if self.normalize_preserved_path(preserved)? != *preserved {
                return Err(EngineError::Rollback(format!(
                    "preserved state path changed after approval: {}",
                    preserved.display()
                )));
            }
        }
        for (index, step) in plan.steps.iter().enumerate() {
            if self.normalize_file(&step.backup_source)? != step.backup_source
                || self.normalize_destination(&step.destination)? != step.destination
            {
                return Err(EngineError::Rollback(format!(
                    "rollback path changed after approval for component {}",
                    step.component
                )));
            }
            if file_sha256(&step.backup_source)? != plan.backup_sha256[index] {
                return Err(EngineError::Rollback(format!(
                    "backup content changed after approval for component {}",
                    step.component
                )));
            }
            Self::reject_preserved_overlap(&step.destination, &plan.preserved_state_paths)?;
        }
        Ok(())
    }

    fn prepare_steps(
        &self,
        plan: &RollbackRecoveryPlan,
        token: &str,
    ) -> EngineResult<Vec<PreparedStep>> {
        let mut reserved = plan
            .steps
            .iter()
            .map(|step| step.destination.clone())
            .collect::<BTreeSet<_>>();
        let mut prepared = Vec::with_capacity(plan.steps.len());
        for (index, step) in plan.steps.iter().enumerate() {
            let staged = sibling_path(&step.destination, &format!("staged-{index}-{token}"))?;
            let recovery =
                sibling_path(&step.destination, &format!("pre-rollback-{index}-{token}"))?;
            let failed_install = sibling_path(
                &step.destination,
                &format!("failed-install-{index}-{token}"),
            )?;
            for generated in [&staged, &recovery, &failed_install] {
                self.validate_destination(generated)?;
                Self::reject_preserved_overlap(generated, &plan.preserved_state_paths)?;
                if !reserved.insert(generated.clone()) {
                    return Err(EngineError::Rollback(format!(
                        "rollback operation paths collide at {}",
                        generated.display()
                    )));
                }
            }
            prepared.push(PreparedStep {
                component: step.component.clone(),
                backup_source: step.backup_source.clone(),
                destination: step.destination.clone(),
                staged,
                recovery,
                failed_install,
            });
        }
        Ok(prepared)
    }

    fn normalize_file(&self, path: &Path) -> EngineResult<PathBuf> {
        ensure_no_symlink_components(path)?;
        let canonical = fs::canonicalize(path).map_err(|error| EngineError::io(path, error))?;
        if !canonical.is_file() || !self.is_allowed(&canonical) {
            return Err(EngineError::Rollback(format!(
                "backup is outside an allowed root or not a file: {}",
                path.display()
            )));
        }
        Ok(canonical)
    }

    fn normalize_preserved_path(&self, path: &Path) -> EngineResult<PathBuf> {
        ensure_no_symlink_components(path)?;
        let canonical = fs::canonicalize(path).map_err(|error| EngineError::io(path, error))?;
        if !self.is_allowed(&canonical) {
            return Err(EngineError::Rollback(format!(
                "preserved state is outside an allowed root: {}",
                path.display()
            )));
        }
        Ok(canonical)
    }

    fn normalize_destination(&self, path: &Path) -> EngineResult<PathBuf> {
        self.validate_destination(path)?;
        if path.exists() {
            return fs::canonicalize(path).map_err(|error| EngineError::io(path, error));
        }
        let parent = path.parent().ok_or_else(|| {
            EngineError::Rollback(format!("destination has no parent: {}", path.display()))
        })?;
        let parent = fs::canonicalize(parent).map_err(|error| EngineError::io(parent, error))?;
        let name = path.file_name().ok_or_else(|| {
            EngineError::Rollback(format!("destination has no file name: {}", path.display()))
        })?;
        Ok(parent.join(name))
    }

    fn validate_destination(&self, path: &Path) -> EngineResult<()> {
        ensure_no_symlink_components(path)?;
        if path.exists() && !path.is_file() {
            return Err(EngineError::Rollback(format!(
                "existing rollback destination is not a regular file: {}",
                path.display()
            )));
        }
        let parent = path.parent().ok_or_else(|| {
            EngineError::Rollback(format!("destination has no parent: {}", path.display()))
        })?;
        let parent = fs::canonicalize(parent).map_err(|error| EngineError::io(parent, error))?;
        if self.is_allowed(&parent) {
            Ok(())
        } else {
            Err(EngineError::Rollback(format!(
                "destination is outside an allowed root: {}",
                path.display()
            )))
        }
    }

    fn reject_preserved_overlap(candidate: &Path, preserved: &[PathBuf]) -> EngineResult<()> {
        if let Some(path) = preserved.iter().find(|path| paths_overlap(candidate, path)) {
            return Err(EngineError::Rollback(format!(
                "rollback path {} overlaps preserved state {}",
                candidate.display(),
                path.display()
            )));
        }
        Ok(())
    }

    fn ensure_allowed_roots_intact(&self) -> EngineResult<()> {
        for root in &self.allowed_roots {
            ensure_no_symlink_components(root)?;
            let current = fs::canonicalize(root).map_err(|error| EngineError::io(root, error))?;
            if current != *root || !current.is_dir() {
                return Err(EngineError::Rollback(format!(
                    "allowed root changed after validation: {}",
                    root.display()
                )));
            }
        }
        Ok(())
    }

    fn is_allowed(&self, path: &Path) -> bool {
        self.allowed_roots.iter().any(|root| path.starts_with(root))
    }
}

fn append_failure(
    journal: &mut File,
    index: usize,
    step: &PreparedStep,
    detail: &str,
) -> EngineResult<()> {
    append_journal(
        journal,
        JournalEntry {
            recorded_at: Utc::now(),
            phase: "transaction_failed",
            step_index: Some(index),
            component: Some(&step.component),
            path: Some(&step.destination),
            sha256: None,
            detail: Some(detail),
        },
    )
}

fn append_journal(file: &mut File, entry: JournalEntry<'_>) -> EngineResult<()> {
    let mut bytes = serde_json::to_vec(&entry)?;
    bytes.push(b'\n');
    file.write_all(&bytes)
        .map_err(|error| EngineError::io("rollback journal", error))?;
    file.sync_data()
        .map_err(|error| EngineError::io("rollback journal", error))?;
    Ok(())
}

fn stage_file(source: &Path, destination: &Path) -> EngineResult<()> {
    let mut source_file = File::open(source).map_err(|error| EngineError::io(source, error))?;
    let mut destination_file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(destination)
        .map_err(|error| EngineError::io(destination, error))?;
    std::io::copy(&mut source_file, &mut destination_file)
        .map_err(|error| EngineError::io(destination, error))?;
    destination_file
        .sync_all()
        .map_err(|error| EngineError::io(destination, error))?;
    let permissions = fs::metadata(source)
        .map_err(|error| EngineError::io(source, error))?
        .permissions();
    fs::set_permissions(destination, permissions)
        .map_err(|error| EngineError::io(destination, error))
}

fn file_sha256(path: &Path) -> EngineResult<String> {
    let mut file = File::open(path).map_err(|error| EngineError::io(path, error))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| EngineError::io(path, error))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

fn sibling_path(destination: &Path, suffix: &str) -> EngineResult<PathBuf> {
    let file_name = destination.file_name().ok_or_else(|| {
        EngineError::Rollback(format!(
            "destination has no file name: {}",
            destination.display()
        ))
    })?;
    let mut name = file_name.to_os_string();
    name.push(format!(".orchestrator-{suffix}"));
    let path = destination.with_file_name(name);
    if path.exists() {
        return Err(EngineError::Rollback(format!(
            "rollback staging path already exists: {}",
            path.display()
        )));
    }
    Ok(path)
}

fn ensure_no_symlink_components(path: &Path) -> EngineResult<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => current.push(prefix.as_os_str()),
            Component::RootDir => current.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => return Err(EngineError::UnsafePath(path.to_path_buf())),
            Component::Normal(part) => {
                current.push(part);
                if current.exists()
                    && fs::symlink_metadata(&current)
                        .map_err(|error| EngineError::io(&current, error))?
                        .file_type()
                        .is_symlink()
                {
                    return Err(EngineError::UnsafePath(current));
                }
            }
        }
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn is_broad_root(path: &Path) -> bool {
    path.parent().is_none() || path.components().count() < 3
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{RollbackApproval, RollbackManager, RollbackStep};

    #[test]
    fn approval_must_match_plan_hash() -> Result<(), Box<dyn std::error::Error>> {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let old = directory.path().join("old.bin");
        let current = directory.path().join("current.bin");
        std::fs::write(&old, b"old")?;
        std::fs::write(&current, b"current")?;
        let manager = RollbackManager::new(vec![directory.path().to_path_buf()])?;
        let plan = manager.plan(
            "0.0.9",
            vec![RollbackStep {
                component: "orchestrator".to_owned(),
                backup_source: old,
                destination: current,
            }],
            &[],
            Utc::now(),
        )?;
        let mut approval = RollbackApproval::for_plan(&plan, "admin", Utc::now());
        approval.plan_hash.push('0');
        assert!(manager.apply(&plan, &approval).is_err());
        Ok(())
    }

    #[test]
    fn preserved_state_cannot_be_replaced_or_contain_a_destination()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let state = directory.path().join("state");
        std::fs::create_dir(&state)?;
        let old = directory.path().join("old.bin");
        let task = state.join("task.json");
        std::fs::write(&old, b"old")?;
        std::fs::write(&task, b"state")?;
        let manager = RollbackManager::new(vec![directory.path().to_path_buf()])?;
        let result = manager.plan(
            "0.0.9",
            vec![RollbackStep {
                component: "state".to_owned(),
                backup_source: old,
                destination: task,
            }],
            &[state],
            Utc::now(),
        );
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn successful_apply_keeps_recovery_backup_and_journal() -> Result<(), Box<dyn std::error::Error>>
    {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let old = directory.path().join("old.bin");
        let current = directory.path().join("current.bin");
        std::fs::write(&old, b"old")?;
        std::fs::write(&current, b"current")?;
        let manager = RollbackManager::new(vec![directory.path().to_path_buf()])?;
        let plan = manager.plan(
            "0.0.9",
            vec![RollbackStep {
                component: "orchestrator".to_owned(),
                backup_source: old,
                destination: current.clone(),
            }],
            &[],
            Utc::now(),
        )?;
        let approval = RollbackApproval::for_plan(&plan, "admin", Utc::now());
        let report = manager.apply(&plan, &approval)?;
        assert_eq!(std::fs::read(&current)?, b"old");
        assert_eq!(report.recovery_backups.len(), 1);
        assert_eq!(std::fs::read(&report.recovery_backups[0])?, b"current");
        let journal = std::fs::read_to_string(report.journal_path)?;
        assert!(journal.contains("transaction_completed"));
        Ok(())
    }

    #[test]
    fn approved_backup_content_is_integrity_bound() -> Result<(), Box<dyn std::error::Error>> {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let old = directory.path().join("old.bin");
        let current = directory.path().join("current.bin");
        std::fs::write(&old, b"approved-old")?;
        std::fs::write(&current, b"current")?;
        let manager = RollbackManager::new(vec![directory.path().to_path_buf()])?;
        let plan = manager.plan(
            "0.0.9",
            vec![RollbackStep {
                component: "orchestrator".to_owned(),
                backup_source: old.clone(),
                destination: current.clone(),
            }],
            &[],
            Utc::now(),
        )?;
        let approval = RollbackApproval::for_plan(&plan, "admin", Utc::now());
        std::fs::write(old, b"changed-after-approval")?;

        assert!(manager.apply(&plan, &approval).is_err());
        assert_eq!(std::fs::read(current)?, b"current");
        Ok(())
    }

    #[test]
    fn later_step_failure_restores_all_previously_changed_destinations()
    -> Result<(), Box<dyn std::error::Error>> {
        let directory = crate::test_support::CanonicalTempDir::new()?;
        let old_a = directory.path().join("old-a.bin");
        let old_b = directory.path().join("old-b.bin");
        let current_a = directory.path().join("current-a.bin");
        let current_b = directory.path().join("current-b.bin");
        std::fs::write(&old_a, b"old-a")?;
        std::fs::write(&old_b, b"old-b")?;
        std::fs::write(&current_a, b"current-a")?;
        std::fs::write(&current_b, b"current-b")?;
        let manager = RollbackManager::new(vec![directory.path().to_path_buf()])?;
        let plan = manager.plan(
            "0.0.9",
            vec![
                RollbackStep {
                    component: "a".to_owned(),
                    backup_source: old_a,
                    destination: current_a.clone(),
                },
                RollbackStep {
                    component: "b".to_owned(),
                    backup_source: old_b,
                    destination: current_b.clone(),
                },
            ],
            &[],
            Utc::now(),
        )?;
        let approval = RollbackApproval::for_plan(&plan, "admin", Utc::now());
        let error = manager
            .apply_transaction(&plan, &approval, Some(1))
            .err()
            .ok_or("injected transaction unexpectedly succeeded")?;
        assert_eq!(std::fs::read(&current_a)?, b"current-a");
        assert_eq!(std::fs::read(&current_b)?, b"current-b");
        assert!(error.to_string().contains("recovery journal"));
        let failed_install = std::fs::read_dir(directory.path())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.contains("failed-install-0"))
            })
            .ok_or("failed install evidence was not preserved")?;
        assert_eq!(std::fs::read(failed_install)?, b"old-a");
        let journal = std::fs::read_dir(directory.path())?
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "jsonl")
            })
            .ok_or("rollback journal was not preserved")?;
        assert!(std::fs::read_to_string(journal)?.contains("transaction_recovery_completed"));
        Ok(())
    }
}
