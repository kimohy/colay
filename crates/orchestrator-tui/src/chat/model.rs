use std::collections::HashSet;

use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceCursor {
    pub message_ordinal: i64,
    pub event_sequence: i64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "target", content = "id", rename_all = "snake_case")]
pub enum ComposerTarget {
    #[default]
    Orchestrator,
    Task(String),
    AllRunning,
}

impl ComposerTarget {
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Orchestrator => "orchestrator",
            Self::Task(task_id) => task_id,
            Self::AllRunning => "all running",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DaemonConnectivity {
    Online,
    Stale,
    #[default]
    Offline,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub title: String,
    pub state: String,
    pub state_symbol: String,
    pub dependency_status: String,
    pub needs_attention: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub ordinal: i64,
    pub message_id: String,
    pub task_id: Option<String>,
    pub role: String,
    pub kind: String,
    pub state: String,
    pub content: String,
    pub created_at: String,
    pub folded: bool,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttentionSeverity {
    Info,
    #[default]
    Warning,
    Critical,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AttentionItem {
    pub key: String,
    pub task_id: Option<String>,
    pub severity: AttentionSeverity,
    pub label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskInspector {
    pub task_id: String,
    pub state: String,
    pub provider: String,
    pub profile: String,
    pub effort: String,
    pub progress: String,
    pub elapsed: String,
    pub dependencies: Vec<String>,
    pub worktree: String,
    pub changed_files: Vec<String>,
    pub tests: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceSnapshot {
    pub repository: String,
    pub session_id: String,
    pub session_title: String,
    pub session_state: String,
    pub daemon: DaemonConnectivity,
    pub running_count: usize,
    pub blocked_count: usize,
    pub tasks: Vec<TaskSummary>,
    pub messages: Vec<TimelineEntry>,
    pub has_older_messages: bool,
    pub attention: Vec<AttentionItem>,
    pub inspector: Option<TaskInspector>,
    pub cursor: WorkspaceCursor,
    pub read_only_reason: Option<String>,
}

impl WorkspaceSnapshot {
    /// Validates presentation identity and cross-row references.
    ///
    /// # Errors
    ///
    /// Returns [`WorkspaceModelError`] when required strings are blank, task IDs repeat,
    /// or inspector/message task references do not exist in the task list.
    pub fn validate(&self) -> Result<(), WorkspaceModelError> {
        require_non_blank(&self.repository, "repository")?;
        require_non_blank(&self.session_id, "session ID")?;
        require_non_blank(&self.session_title, "session title")?;
        require_non_blank(&self.session_state, "session state")?;

        let mut task_ids = HashSet::with_capacity(self.tasks.len());
        for task in &self.tasks {
            require_non_blank(&task.task_id, "task ID")?;
            require_non_blank(&task.title, "task title")?;
            require_non_blank(&task.state, "task state")?;
            if !task_ids.insert(task.task_id.as_str()) {
                return Err(WorkspaceModelError::DuplicateTask(task.task_id.clone()));
            }
        }
        if let Some(inspector) = &self.inspector
            && !task_ids.contains(inspector.task_id.as_str())
        {
            return Err(WorkspaceModelError::UnknownTaskReference(
                inspector.task_id.clone(),
            ));
        }
        for message in &self.messages {
            require_non_blank(&message.message_id, "message ID")?;
            if message.state != "streaming" {
                require_non_blank(&message.content, "message content")?;
            }
            if let Some(task_id) = &message.task_id
                && !task_ids.contains(task_id.as_str())
            {
                return Err(WorkspaceModelError::UnknownTaskReference(task_id.clone()));
            }
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "intent", rename_all = "snake_case")]
pub enum TaskControlIntent {
    Pause,
    Resume,
    Cancel,
    Handover { provider: String },
    Retry,
    Checkpoint,
    Provider,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WorkspaceAction {
    SubmitMessage {
        target: ComposerTarget,
        content: String,
    },
    RequestTaskControl {
        task_id: String,
        intent: TaskControlIntent,
    },
    OpenAdministration,
    Quit,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedbackLevel {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActionFeedback {
    pub level: FeedbackLevel,
    pub message: String,
}

impl ActionFeedback {
    #[must_use]
    pub fn info(message: impl Into<String>) -> Self {
        Self {
            level: FeedbackLevel::Info,
            message: message.into(),
        }
    }

    #[must_use]
    pub fn unavailable(feature: &str) -> Self {
        Self {
            level: FeedbackLevel::Warning,
            message: format!("{feature} becomes available in a later orchestration phase"),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum WorkspaceModelError {
    #[error("{0} must not be blank")]
    BlankField(&'static str),
    #[error("duplicate task ID `{0}`")]
    DuplicateTask(String),
    #[error("task reference `{0}` is absent from the workspace task list")]
    UnknownTaskReference(String),
}

fn require_non_blank(value: &str, field: &'static str) -> Result<(), WorkspaceModelError> {
    if value.trim().is_empty() {
        Err(WorkspaceModelError::BlankField(field))
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ComposerTarget, DaemonConnectivity, TaskInspector, TaskSummary, TimelineEntry,
        WorkspaceSnapshot,
    };

    fn task(task_id: &str) -> TaskSummary {
        TaskSummary {
            task_id: task_id.to_owned(),
            title: format!("Task {task_id}"),
            state: "running".to_owned(),
            state_symbol: "*".to_owned(),
            dependency_status: "ready".to_owned(),
            needs_attention: false,
        }
    }

    fn inspector(task_id: &str) -> TaskInspector {
        TaskInspector {
            task_id: task_id.to_owned(),
            state: "running".to_owned(),
            ..TaskInspector::default()
        }
    }

    fn sample_snapshot() -> WorkspaceSnapshot {
        WorkspaceSnapshot {
            repository: "colay".to_owned(),
            session_id: "session-01".to_owned(),
            session_title: "Auth refactor".to_owned(),
            session_state: "running".to_owned(),
            daemon: DaemonConnectivity::Online,
            tasks: vec![task("task-01")],
            messages: vec![TimelineEntry {
                ordinal: 1,
                message_id: "message-01".to_owned(),
                role: "user".to_owned(),
                kind: "user_message".to_owned(),
                state: "final".to_owned(),
                content: "Refactor auth".to_owned(),
                created_at: "2026-07-21T00:00:00Z".to_owned(),
                ..TimelineEntry::default()
            }],
            inspector: Some(inspector("task-01")),
            ..WorkspaceSnapshot::default()
        }
    }

    #[test]
    fn composer_target_round_trip_is_explicit() -> Result<(), serde_json::Error> {
        let target = ComposerTarget::Task("task-03".to_owned());
        let json = serde_json::to_string(&target)?;
        assert_eq!(serde_json::from_str::<ComposerTarget>(&json)?, target);
        assert_eq!(ComposerTarget::default(), ComposerTarget::Orchestrator);
        Ok(())
    }

    #[test]
    fn daemon_connectivity_uses_stable_textual_states() -> Result<(), serde_json::Error> {
        assert_eq!(
            serde_json::to_string(&DaemonConnectivity::Online)?,
            "\"online\""
        );
        assert_eq!(
            serde_json::from_str::<DaemonConnectivity>("\"stale\"")?,
            DaemonConnectivity::Stale
        );
        Ok(())
    }

    #[test]
    fn snapshot_rejects_duplicate_tasks_and_orphan_inspector() {
        let mut snapshot = sample_snapshot();
        assert_eq!(snapshot.validate(), Ok(()));

        snapshot.tasks.push(snapshot.tasks[0].clone());
        assert!(snapshot.validate().is_err());

        snapshot.tasks = vec![task("task-01")];
        snapshot.inspector = Some(inspector("task-02"));
        assert!(snapshot.validate().is_err());
    }

    #[test]
    fn snapshot_rejects_blank_identity_and_timeline_content() {
        let mut snapshot = sample_snapshot();
        snapshot.session_id.clear();
        assert!(snapshot.validate().is_err());

        snapshot = sample_snapshot();
        snapshot.messages[0].content.clear();
        assert!(snapshot.validate().is_err());
    }
}
