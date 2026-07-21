use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    CheckpointId, GraphRevisionId, IntegrationApplicationId, IntegrationBatchId, RepoPath,
    SchemaVersion, SessionId, TaskId, VerificationId, canonical_sha256, repo_paths_overlap,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationSource {
    pub task_id: TaskId,
    pub checkpoint_id: CheckpointId,
    pub verification_id: VerificationId,
    pub base_revision: String,
    pub diff_sha256: String,
    pub changed_files: Vec<RepoPath>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum IntegrationBlocker {
    MissingEvidence {
        task_id: TaskId,
        detail: String,
    },
    VerificationFailed {
        task_id: TaskId,
    },
    StaleBase {
        task_id: TaskId,
        found: String,
    },
    SourceChanged {
        task_id: TaskId,
    },
    PathOverlap {
        left: TaskId,
        right: TaskId,
        path: RepoPath,
    },
    PatchFailed {
        task_id: TaskId,
        detail: String,
    },
}

#[derive(Serialize)]
struct PreviewSeal<'a> {
    schema_version: &'a SchemaVersion,
    batch_id: IntegrationBatchId,
    session_id: SessionId,
    graph_revision_id: GraphRevisionId,
    base_revision: &'a str,
    sources: &'a [IntegrationSource],
    blockers: &'a [IntegrationBlocker],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationPreview {
    pub schema_version: SchemaVersion,
    pub batch_id: IntegrationBatchId,
    pub session_id: SessionId,
    pub graph_revision_id: GraphRevisionId,
    pub base_revision: String,
    pub sources: Vec<IntegrationSource>,
    pub blockers: Vec<IntegrationBlocker>,
    pub created_at: DateTime<Utc>,
    pub preview_hash: String,
}

impl IntegrationPreview {
    /// Seals a deterministic preview. `sources` must already be in dependency order.
    ///
    /// # Errors
    ///
    /// Returns an integration error for malformed source evidence or a seal failure.
    pub fn seal(
        batch_id: IntegrationBatchId,
        session_id: SessionId,
        graph_revision_id: GraphRevisionId,
        base_revision: String,
        sources: Vec<IntegrationSource>,
        mut blockers: Vec<IntegrationBlocker>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, IntegrationError> {
        validate_object_id(&base_revision)?;
        validate_sources(&sources)?;
        blockers.extend(overlap_blockers(&sources));
        let mut preview = Self {
            schema_version: SchemaVersion::v1(),
            batch_id,
            session_id,
            graph_revision_id,
            base_revision,
            sources,
            blockers,
            created_at,
            preview_hash: String::new(),
        };
        preview.preview_hash = preview.compute_hash()?;
        Ok(preview)
    }

    #[must_use]
    pub fn is_approvable(&self) -> bool {
        !self.sources.is_empty() && self.blockers.is_empty() && self.verify_integrity()
    }

    #[must_use]
    pub fn verify_integrity(&self) -> bool {
        self.compute_hash()
            .is_ok_and(|hash| hash == self.preview_hash)
    }

    fn compute_hash(&self) -> Result<String, IntegrationError> {
        Ok(canonical_sha256(&PreviewSeal {
            schema_version: &self.schema_version,
            batch_id: self.batch_id,
            session_id: self.session_id,
            graph_revision_id: self.graph_revision_id,
            base_revision: &self.base_revision,
            sources: &self.sources,
            blockers: &self.blockers,
        })?)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationApproval {
    pub batch_id: IntegrationBatchId,
    pub preview_hash: String,
    pub approved_by: String,
    pub approved_at: DateTime<Utc>,
}

impl IntegrationApproval {
    /// Validates this approval against one exact current preview.
    ///
    /// # Errors
    ///
    /// Returns an integration error for blank identity, wrong hash/batch, blockers,
    /// or failed preview integrity.
    pub fn validate_for(&self, preview: &IntegrationPreview) -> Result<(), IntegrationError> {
        if self.approved_by.trim().is_empty() {
            return Err(IntegrationError::BlankApprover);
        }
        if self.batch_id != preview.batch_id
            || self.preview_hash != preview.preview_hash
            || !preview.is_approvable()
        {
            return Err(IntegrationError::ApprovalMismatch);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntegrationApplication {
    pub application_id: IntegrationApplicationId,
    pub batch_id: IntegrationBatchId,
    pub preview_hash: String,
    pub integration_worktree: String,
    pub integration_branch: String,
    pub resulting_tree: Option<String>,
    pub succeeded: bool,
    pub detail_redacted: String,
    pub completed_at: DateTime<Utc>,
}

fn validate_sources(sources: &[IntegrationSource]) -> Result<(), IntegrationError> {
    let mut seen = std::collections::BTreeSet::new();
    for source in sources {
        if !seen.insert(source.task_id) {
            return Err(IntegrationError::DuplicateTask(source.task_id));
        }
        validate_object_id(&source.base_revision)?;
        validate_sha256(&source.diff_sha256)?;
    }
    Ok(())
}

fn overlap_blockers(sources: &[IntegrationSource]) -> Vec<IntegrationBlocker> {
    let mut blockers = Vec::new();
    for (index, left) in sources.iter().enumerate() {
        for right in &sources[index + 1..] {
            if let Some(path) = left.changed_files.iter().find(|left_path| {
                right
                    .changed_files
                    .iter()
                    .any(|right_path| repo_paths_overlap(left_path, right_path))
            }) {
                blockers.push(IntegrationBlocker::PathOverlap {
                    left: left.task_id,
                    right: right.task_id,
                    path: path.clone(),
                });
            }
        }
    }
    blockers
}

fn validate_object_id(value: &str) -> Result<(), IntegrationError> {
    if (40..=64).contains(&value.len()) && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(IntegrationError::InvalidObjectId)
    }
}

fn validate_sha256(value: &str) -> Result<(), IntegrationError> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(IntegrationError::InvalidHash)
    }
}

#[derive(Debug, Error)]
pub enum IntegrationError {
    #[error("integration object ID is invalid")]
    InvalidObjectId,
    #[error("integration SHA-256 value is invalid")]
    InvalidHash,
    #[error("integration source repeats task {0}")]
    DuplicateTask(TaskId),
    #[error("integration approval identity is blank")]
    BlankApprover,
    #[error("integration approval does not match the current approvable preview")]
    ApprovalMismatch,
    #[error(transparent)]
    Integrity(#[from] crate::IntegrityError),
}

#[cfg(test)]
mod tests {
    use chrono::Utc;

    use super::{IntegrationApproval, IntegrationPreview, IntegrationSource};
    use crate::{
        CheckpointId, GraphRevisionId, IntegrationBatchId, RepoPath, SessionId, TaskId,
        VerificationId,
    };

    fn source(path: &str) -> Result<IntegrationSource, Box<dyn std::error::Error>> {
        Ok(IntegrationSource {
            task_id: TaskId::new(),
            checkpoint_id: CheckpointId::new(),
            verification_id: VerificationId::new(),
            base_revision: "a".repeat(40),
            diff_sha256: "b".repeat(64),
            changed_files: vec![RepoPath::try_from(path)?],
        })
    }

    #[test]
    fn preview_hash_is_stable_and_source_mutation_is_detected()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut preview = IntegrationPreview::seal(
            IntegrationBatchId::new(),
            SessionId::new(),
            GraphRevisionId::new(),
            "a".repeat(40),
            vec![source("src/a.rs")?, source("src/b.rs")?],
            Vec::new(),
            Utc::now(),
        )?;
        assert!(preview.is_approvable());
        preview.sources[0].diff_sha256 = "c".repeat(64);
        assert!(!preview.verify_integrity());
        Ok(())
    }

    #[test]
    fn overlap_blocks_approval_and_exact_hash_is_required() -> Result<(), Box<dyn std::error::Error>>
    {
        let preview = IntegrationPreview::seal(
            IntegrationBatchId::new(),
            SessionId::new(),
            GraphRevisionId::new(),
            "a".repeat(40),
            vec![source("src")?, source("src/lib.rs")?],
            Vec::new(),
            Utc::now(),
        )?;
        assert!(!preview.is_approvable());
        let approval = IntegrationApproval {
            batch_id: preview.batch_id,
            preview_hash: preview.preview_hash.clone(),
            approved_by: "operator".to_owned(),
            approved_at: Utc::now(),
        };
        assert!(approval.validate_for(&preview).is_err());
        Ok(())
    }
}
