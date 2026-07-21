use std::str::FromStr as _;

use chrono::{DateTime, Utc};
use orchestrator_domain::{
    IntegrationApplication, IntegrationApplicationId, IntegrationApproval, IntegrationBatchId,
    IntegrationPreview, SessionId,
};
use rusqlite::{OptionalExtension as _, params};

use crate::{Database, StateError, StateResult};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrationBatchStatus {
    Preview,
    Blocked,
    Approved,
    Applying,
    Applied,
    NeedsAttention,
    Superseded,
}

impl IntegrationBatchStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Preview => "preview",
            Self::Blocked => "blocked",
            Self::Approved => "approved",
            Self::Applying => "applying",
            Self::Applied => "applied",
            Self::NeedsAttention => "needs_attention",
            Self::Superseded => "superseded",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredIntegrationBatch {
    pub preview: IntegrationPreview,
    pub status: IntegrationBatchStatus,
    pub approval: Option<IntegrationApproval>,
    pub application: Option<IntegrationApplication>,
}

impl Database {
    pub fn record_integration_preview(
        &self,
        preview: &IntegrationPreview,
    ) -> StateResult<StoredIntegrationBatch> {
        if !preview.verify_integrity() {
            return Err(StateError::InvalidRecord(
                "integration preview integrity check failed".to_owned(),
            ));
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let current_revision: Option<String> = transaction
            .query_row(
                "SELECT revision_id FROM session_graph_heads WHERE session_id = ?1",
                [preview.session_id.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        if current_revision.as_deref() != Some(&preview.graph_revision_id.to_string()) {
            return Err(StateError::InvalidRecord(
                "integration preview graph is not the current session graph".to_owned(),
            ));
        }
        transaction.execute(
            "UPDATE integration_batches SET status = 'superseded', completed_at = ?1
             WHERE session_id = ?2 AND status IN ('preview', 'blocked')",
            params![
                preview.created_at.to_rfc3339(),
                preview.session_id.to_string()
            ],
        )?;
        let ordinal: i64 = transaction.query_row(
            "SELECT coalesce(max(ordinal), 0) + 1 FROM integration_batches WHERE session_id = ?1",
            [preview.session_id.to_string()],
            |row| row.get(0),
        )?;
        let status = if preview.is_approvable() {
            IntegrationBatchStatus::Preview
        } else {
            IntegrationBatchStatus::Blocked
        };
        transaction.execute(
            "INSERT INTO integration_batches(batch_id, session_id, revision_id, ordinal,
                status, base_revision, preview_hash, preview_json, created_at, completed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, NULL)",
            params![
                preview.batch_id.to_string(),
                preview.session_id.to_string(),
                preview.graph_revision_id.to_string(),
                ordinal,
                status.as_str(),
                preview.base_revision,
                preview.preview_hash,
                serde_json::to_string(preview)?,
                preview.created_at.to_rfc3339(),
            ],
        )?;
        for (index, source) in preview.sources.iter().enumerate() {
            transaction.execute(
                "INSERT INTO integration_sources(batch_id, source_order, task_id,
                    checkpoint_id, verification_id, diff_sha256, source_json)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    preview.batch_id.to_string(),
                    i64::try_from(index + 1).unwrap_or(i64::MAX),
                    source.task_id.to_string(),
                    source.checkpoint_id.to_string(),
                    source.verification_id.to_string(),
                    source.diff_sha256,
                    serde_json::to_string(source)?,
                ],
            )?;
        }
        transaction.commit()?;
        Ok(StoredIntegrationBatch {
            preview: preview.clone(),
            status,
            approval: None,
            application: None,
        })
    }

    pub fn current_integration_batch(
        &self,
        session_id: SessionId,
    ) -> StateResult<Option<StoredIntegrationBatch>> {
        self.with_connection(|connection| {
            let row: Option<(String, String)> = connection
                .query_row(
                    "SELECT preview_json, status FROM integration_batches
                     WHERE session_id = ?1 ORDER BY ordinal DESC LIMIT 1",
                    [session_id.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            row.map(|(preview, status)| {
                let preview: IntegrationPreview = serde_json::from_str(&preview)?;
                load_batch(connection, preview, parse_status(&status)?)
            })
            .transpose()
        })
    }

    pub fn approve_integration(
        &self,
        approval: &IntegrationApproval,
    ) -> StateResult<StoredIntegrationBatch> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let (preview_json, status): (String, String) = transaction.query_row(
            "SELECT preview_json, status FROM integration_batches WHERE batch_id = ?1",
            [approval.batch_id.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        if parse_status(&status)? != IntegrationBatchStatus::Preview {
            return Err(StateError::InvalidRecord(
                "integration batch is not awaiting approval".to_owned(),
            ));
        }
        let preview: IntegrationPreview = serde_json::from_str(&preview_json)?;
        approval
            .validate_for(&preview)
            .map_err(|error| StateError::InvalidRecord(error.to_string()))?;
        transaction.execute(
            "INSERT INTO integration_approvals(batch_id, preview_hash, approved_by, approved_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                approval.batch_id.to_string(),
                approval.preview_hash,
                approval.approved_by.trim(),
                approval.approved_at.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "UPDATE integration_batches SET status = 'approved'
             WHERE batch_id = ?1 AND status = 'preview'",
            [approval.batch_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(StoredIntegrationBatch {
            preview,
            status: IntegrationBatchStatus::Approved,
            approval: Some(approval.clone()),
            application: None,
        })
    }

    pub fn start_integration_application(
        &self,
        batch_id: IntegrationBatchId,
        application_id: IntegrationApplicationId,
        worktree_path: &str,
        branch: &str,
        started_at: DateTime<Utc>,
    ) -> StateResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let preview_hash: String = transaction.query_row(
            "SELECT preview_hash FROM integration_batches WHERE batch_id = ?1 AND status = 'approved'",
            [batch_id.to_string()],
            |row| row.get(0),
        )?;
        transaction.execute(
            "INSERT INTO integration_applications(application_id, batch_id, preview_hash,
                state, worktree_path, branch_name, resulting_tree, detail_redacted,
                started_at, completed_at)
             VALUES (?1, ?2, ?3, 'applying', ?4, ?5, NULL, '', ?6, NULL)",
            params![
                application_id.to_string(),
                batch_id.to_string(),
                preview_hash,
                worktree_path,
                branch,
                started_at.to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "UPDATE integration_batches SET status = 'applying' WHERE batch_id = ?1",
            [batch_id.to_string()],
        )?;
        transaction.commit()?;
        Ok(())
    }

    pub fn finish_integration_application(
        &self,
        application: &IntegrationApplication,
    ) -> StateResult<()> {
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let state = if application.succeeded {
            "applied"
        } else {
            "failed"
        };
        let changed = transaction.execute(
            "UPDATE integration_applications SET state = ?1, resulting_tree = ?2,
                detail_redacted = ?3, completed_at = ?4
             WHERE application_id = ?5 AND batch_id = ?6 AND state = 'applying'
               AND preview_hash = ?7",
            params![
                state,
                application.resulting_tree,
                application.detail_redacted,
                application.completed_at.to_rfc3339(),
                application.application_id.to_string(),
                application.batch_id.to_string(),
                application.preview_hash,
            ],
        )?;
        if changed != 1 {
            return Err(StateError::OptimisticConflict {
                entity: format!("integration application {}", application.application_id),
            });
        }
        transaction.execute(
            "UPDATE integration_batches SET status = ?1, completed_at = ?2 WHERE batch_id = ?3",
            params![
                if application.succeeded {
                    "applied"
                } else {
                    "needs_attention"
                },
                application.completed_at.to_rfc3339(),
                application.batch_id.to_string(),
            ],
        )?;
        transaction.commit()?;
        Ok(())
    }
}

fn load_batch(
    connection: &rusqlite::Connection,
    preview: IntegrationPreview,
    status: IntegrationBatchStatus,
) -> StateResult<StoredIntegrationBatch> {
    if !preview.verify_integrity() {
        return Err(StateError::InvalidRecord(
            "stored integration preview integrity check failed".to_owned(),
        ));
    }
    let approval = connection
        .query_row(
            "SELECT preview_hash, approved_by, approved_at FROM integration_approvals
             WHERE batch_id = ?1",
            [preview.batch_id.to_string()],
            |row| {
                let approved_at: String = row.get(2)?;
                Ok(IntegrationApproval {
                    batch_id: preview.batch_id,
                    preview_hash: row.get(0)?,
                    approved_by: row.get(1)?,
                    approved_at: parse_time(&approved_at, 2)?,
                })
            },
        )
        .optional()?;
    let application = connection
        .query_row(
            "SELECT application_id, preview_hash, worktree_path, branch_name, resulting_tree,
                    state, detail_redacted, completed_at FROM integration_applications
             WHERE batch_id = ?1",
            [preview.batch_id.to_string()],
            |row| {
                let id: String = row.get(0)?;
                let state: String = row.get(5)?;
                let completed: Option<String> = row.get(7)?;
                Ok(IntegrationApplication {
                    application_id: IntegrationApplicationId::from_str(&id)
                        .map_err(|error| conversion(0, error))?,
                    batch_id: preview.batch_id,
                    preview_hash: row.get(1)?,
                    integration_worktree: row.get(2)?,
                    integration_branch: row.get(3)?,
                    resulting_tree: row.get(4)?,
                    succeeded: state == "applied",
                    detail_redacted: row.get(6)?,
                    completed_at: completed
                        .as_deref()
                        .map(|value| parse_time(value, 7))
                        .transpose()?
                        .unwrap_or(preview.created_at),
                })
            },
        )
        .optional()?;
    Ok(StoredIntegrationBatch {
        preview,
        status,
        approval,
        application,
    })
}

fn parse_status(value: &str) -> StateResult<IntegrationBatchStatus> {
    match value {
        "preview" => Ok(IntegrationBatchStatus::Preview),
        "blocked" => Ok(IntegrationBatchStatus::Blocked),
        "approved" => Ok(IntegrationBatchStatus::Approved),
        "applying" => Ok(IntegrationBatchStatus::Applying),
        "applied" => Ok(IntegrationBatchStatus::Applied),
        "needs_attention" => Ok(IntegrationBatchStatus::NeedsAttention),
        "superseded" => Ok(IntegrationBatchStatus::Superseded),
        _ => Err(StateError::InvalidRecord(format!(
            "unknown integration batch status `{value}`"
        ))),
    }
}

fn parse_time(value: &str, column: usize) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|value| value.with_timezone(&Utc))
        .map_err(|error| conversion(column, error))
}

fn conversion(
    column: usize,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(column, rusqlite::types::Type::Text, Box::new(error))
}
