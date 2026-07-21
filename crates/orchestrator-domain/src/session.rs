use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{ClientCommandId, GraphRevisionId, MessageId, SchemaVersion, SessionId, TaskId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Drafting,
    Planning,
    AwaitingApproval,
    Running,
    NeedsAttention,
    Integrating,
    Verifying,
    Completed,
    Stopping,
    Cancelled,
}

impl SessionState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Cancelled)
    }

    /// Validates an explicit session lifecycle edge.
    ///
    /// # Errors
    ///
    /// Returns [`SessionTransitionError`] for no-op or unlisted transitions.
    pub fn validate_transition(self, next: Self) -> Result<(), SessionTransitionError> {
        if self == next {
            return Err(SessionTransitionError::NoOp(self));
        }
        let allowed = match self {
            Self::Drafting => matches!(next, Self::Planning | Self::Stopping),
            Self::Planning => matches!(
                next,
                Self::AwaitingApproval | Self::NeedsAttention | Self::Stopping
            ),
            Self::AwaitingApproval => {
                matches!(next, Self::Planning | Self::Running | Self::Stopping)
            }
            Self::Running => matches!(
                next,
                Self::NeedsAttention | Self::Integrating | Self::Stopping
            ),
            Self::NeedsAttention => matches!(
                next,
                Self::Planning | Self::Running | Self::Integrating | Self::Stopping
            ),
            Self::Integrating => {
                matches!(
                    next,
                    Self::NeedsAttention | Self::Verifying | Self::Stopping
                )
            }
            Self::Verifying => matches!(
                next,
                Self::Completed | Self::NeedsAttention | Self::Stopping
            ),
            Self::Stopping => next == Self::Cancelled,
            Self::Completed | Self::Cancelled => false,
        };
        if allowed {
            Ok(())
        } else {
            Err(SessionTransitionError::NotAllowed {
                from: self,
                to: next,
            })
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SessionTransitionError {
    #[error("session is already in state {0:?}")]
    NoOp(SessionState),
    #[error("session transition from {from:?} to {to:?} is not allowed")]
    NotAllowed {
        from: SessionState,
        to: SessionState,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Session {
    pub schema_version: SchemaVersion,
    pub session_id: SessionId,
    pub revision: u64,
    pub title: String,
    pub state: SessionState,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub archived_at: Option<DateTime<Utc>>,
}

impl Session {
    /// Creates a new draft session with a normalized title.
    ///
    /// # Errors
    ///
    /// Returns [`SessionValidationError::BlankTitle`] for a blank title.
    pub fn new(
        title: impl Into<String>,
        created_at: DateTime<Utc>,
    ) -> Result<Self, SessionValidationError> {
        let title = title.into().trim().to_owned();
        if title.is_empty() {
            return Err(SessionValidationError::BlankTitle);
        }
        Ok(Self {
            schema_version: SchemaVersion::v1(),
            session_id: SessionId::new(),
            revision: 0,
            title,
            state: SessionState::Drafting,
            created_at,
            updated_at: created_at,
            archived_at: None,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Orchestrator,
    Agent,
    System,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageKind {
    UserMessage,
    OrchestratorMessage,
    AgentMessage,
    Plan,
    ToolSummary,
    StateChange,
    ApprovalRequest,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageState {
    Streaming,
    Final,
    Interrupted,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConversationMessage {
    pub message_id: MessageId,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub role: MessageRole,
    pub kind: MessageKind,
    pub state: MessageState,
    pub content_redacted: String,
    pub created_at: DateTime<Utc>,
    pub finalized_at: Option<DateTime<Utc>>,
}

impl ConversationMessage {
    /// Validates message lifecycle fields before persistence.
    ///
    /// # Errors
    ///
    /// Returns [`SessionValidationError`] when state, content, and timestamps disagree.
    pub fn validate(&self) -> Result<(), SessionValidationError> {
        match self.state {
            MessageState::Streaming if self.finalized_at.is_some() => {
                Err(SessionValidationError::StreamingMessageFinalized)
            }
            MessageState::Final if self.content_redacted.trim().is_empty() => {
                Err(SessionValidationError::BlankFinalMessage)
            }
            MessageState::Final | MessageState::Interrupted | MessageState::Rejected
                if self.finalized_at.is_none() =>
            {
                Err(SessionValidationError::MissingMessageFinalizedAt)
            }
            MessageState::Streaming
            | MessageState::Final
            | MessageState::Interrupted
            | MessageState::Rejected => Ok(()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSessionCommandPayload {
    pub session_id: SessionId,
    pub title: String,
}

impl CreateSessionCommandPayload {
    /// Validates a durable create-session command payload.
    ///
    /// # Errors
    ///
    /// Returns [`SessionValidationError::BlankTitle`] for a blank session title.
    pub fn validate(&self) -> Result<(), SessionValidationError> {
        if self.title.trim().is_empty() {
            Err(SessionValidationError::BlankTitle)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppendMessageCommandPayload {
    pub message_id: MessageId,
    pub content: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RequestPlanCommandPayload {
    pub goal_message_id: MessageId,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveGraphCommandPayload {
    pub revision_id: GraphRevisionId,
    pub proposal_hash: String,
    pub approved_by: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApproveIntegrationCommandPayload {
    pub batch_id: crate::IntegrationBatchId,
    pub preview_hash: String,
    pub approved_by: String,
}

impl ApproveIntegrationCommandPayload {
    /// Validates exact integration approval authority fields.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a malformed preview hash or blank approver.
    pub fn validate(&self) -> Result<(), SessionValidationError> {
        if self.preview_hash.len() != 64
            || !self
                .preview_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SessionValidationError::InvalidIntegrationPreviewHash);
        }
        if self.approved_by.trim().is_empty() {
            return Err(SessionValidationError::BlankApprover);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateResolutionTaskCommandPayload {
    pub batch_id: crate::IntegrationBatchId,
}

impl ApproveGraphCommandPayload {
    /// Validates the typed authority fields for exact graph approval.
    ///
    /// # Errors
    ///
    /// Returns a validation error for a non-hex SHA-256 hash or blank approver identity.
    pub fn validate(&self) -> Result<(), SessionValidationError> {
        if self.proposal_hash.len() != 64
            || !self
                .proposal_hash
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(SessionValidationError::InvalidProposalHash);
        }
        if self.approved_by.trim().is_empty() {
            return Err(SessionValidationError::BlankApprover);
        }
        Ok(())
    }
}

impl AppendMessageCommandPayload {
    /// Validates a durable user-message command payload.
    ///
    /// # Errors
    ///
    /// Returns [`SessionValidationError::BlankFinalMessage`] for blank content.
    pub fn validate(&self) -> Result<(), SessionValidationError> {
        if self.content.trim().is_empty() {
            Err(SessionValidationError::BlankFinalMessage)
        } else {
            Ok(())
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientCommandAction {
    CreateSession,
    AppendMessage,
    StopDaemon,
    RequestPlan,
    ApproveGraph,
    ReviseGraph,
    CancelPlan,
    RequestIntegration,
    ApproveIntegration,
    CreateResolutionTask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClientCommandState {
    Pending,
    Claimed,
    Completed,
    Failed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClientCommand {
    pub command_id: ClientCommandId,
    pub session_id: Option<SessionId>,
    pub task_id: Option<TaskId>,
    pub action: ClientCommandAction,
    pub payload: serde_json::Value,
    pub idempotency_key: String,
    pub state: ClientCommandState,
    pub requested_by: String,
    pub requested_at: DateTime<Utc>,
    pub claimed_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub outcome: Option<String>,
}

impl ClientCommand {
    /// Validates durable command identity and lifecycle timestamps.
    ///
    /// # Errors
    ///
    /// Returns [`SessionValidationError`] when required identity or lifecycle data is absent.
    pub fn validate(&self) -> Result<(), SessionValidationError> {
        if self.idempotency_key.trim().is_empty() {
            return Err(SessionValidationError::BlankIdempotencyKey);
        }
        if self.requested_by.trim().is_empty() {
            return Err(SessionValidationError::BlankRequester);
        }
        match self.state {
            ClientCommandState::Pending
                if self.claimed_at.is_none()
                    && self.completed_at.is_none()
                    && self.outcome.is_none() =>
            {
                Ok(())
            }
            ClientCommandState::Claimed
                if self.claimed_at.is_some()
                    && self.completed_at.is_none()
                    && self.outcome.is_none() =>
            {
                Ok(())
            }
            ClientCommandState::Completed | ClientCommandState::Failed
                if self.claimed_at.is_some()
                    && self.completed_at.is_some()
                    && self
                        .outcome
                        .as_deref()
                        .is_some_and(|outcome| !outcome.trim().is_empty()) =>
            {
                Ok(())
            }
            _ => Err(SessionValidationError::InvalidCommandLifecycle),
        }
    }
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SessionValidationError {
    #[error("session title must not be blank")]
    BlankTitle,
    #[error("final message content must not be blank")]
    BlankFinalMessage,
    #[error("streaming message must not have a finalization timestamp")]
    StreamingMessageFinalized,
    #[error("non-streaming message requires a finalization timestamp")]
    MissingMessageFinalizedAt,
    #[error("client command idempotency key must not be blank")]
    BlankIdempotencyKey,
    #[error("client command requester must not be blank")]
    BlankRequester,
    #[error("graph approval proposal hash must be a hexadecimal SHA-256 value")]
    InvalidProposalHash,
    #[error("graph approver identity must not be blank")]
    BlankApprover,
    #[error("integration preview hash must be a hexadecimal SHA-256 value")]
    InvalidIntegrationPreviewHash,
    #[error("client command state does not match its lifecycle timestamps")]
    InvalidCommandLifecycle,
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone as _, Utc};
    use serde_json::json;

    use super::{
        AppendMessageCommandPayload, ApproveGraphCommandPayload, ClientCommand,
        ClientCommandAction, ClientCommandState, ConversationMessage, CreateSessionCommandPayload,
        MessageKind, MessageRole, MessageState, RequestPlanCommandPayload, Session, SessionState,
    };
    use crate::{ClientCommandId, GraphRevisionId, MessageId, SessionId};

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 9, 0, 0)
            .single()
            .unwrap_or_default()
    }

    #[test]
    fn session_title_must_not_be_blank() {
        assert!(Session::new("   ", timestamp()).is_err());
        let result = Session::new(" auth refactor ", timestamp());
        assert!(result.is_ok());
        let Ok(session) = result else {
            return;
        };
        assert_eq!(session.title, "auth refactor");
        assert_eq!(session.state, SessionState::Drafting);
    }

    #[test]
    fn session_transition_matrix_allows_attention_recovery_and_safe_stop() {
        assert_eq!(
            SessionState::Running.validate_transition(SessionState::NeedsAttention),
            Ok(())
        );
        assert_eq!(
            SessionState::NeedsAttention.validate_transition(SessionState::Running),
            Ok(())
        );
        assert_eq!(
            SessionState::Running.validate_transition(SessionState::Stopping),
            Ok(())
        );
        assert_eq!(
            SessionState::Stopping.validate_transition(SessionState::Cancelled),
            Ok(())
        );
    }

    #[test]
    fn typed_graph_commands_round_trip_and_validate_exact_authority() {
        let request = RequestPlanCommandPayload {
            goal_message_id: MessageId::new(),
        };
        let parsed = serde_json::from_value::<RequestPlanCommandPayload>(
            serde_json::to_value(&request).unwrap_or_default(),
        );
        assert!(parsed.is_ok());
        assert_eq!(parsed.ok(), Some(request));
        let approval = ApproveGraphCommandPayload {
            revision_id: GraphRevisionId::new(),
            proposal_hash: "a".repeat(64),
            approved_by: "operator".to_owned(),
        };
        assert!(approval.validate().is_ok());
        let mut invalid = approval;
        invalid.proposal_hash = "not-a-hash".to_owned();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn terminal_sessions_and_unlisted_edges_do_not_transition() {
        assert!(
            SessionState::Completed
                .validate_transition(SessionState::Running)
                .is_err()
        );
        assert!(
            SessionState::Cancelled
                .validate_transition(SessionState::Drafting)
                .is_err()
        );
        assert!(
            SessionState::Drafting
                .validate_transition(SessionState::Completed)
                .is_err()
        );
        assert!(
            SessionState::Running
                .validate_transition(SessionState::Running)
                .is_err()
        );
    }

    #[test]
    fn final_message_requires_content_but_streaming_message_may_start_empty() {
        let mut message = ConversationMessage {
            message_id: MessageId::new(),
            session_id: SessionId::new(),
            task_id: None,
            role: MessageRole::Agent,
            kind: MessageKind::AgentMessage,
            state: MessageState::Streaming,
            content_redacted: String::new(),
            created_at: timestamp(),
            finalized_at: None,
        };
        assert_eq!(message.validate(), Ok(()));
        message.state = MessageState::Final;
        message.finalized_at = Some(timestamp());
        assert!(message.validate().is_err());
        message.content_redacted = "done".to_owned();
        assert_eq!(message.validate(), Ok(()));
    }

    #[test]
    fn client_command_requires_idempotency_key_and_requester() {
        let mut command = ClientCommand {
            command_id: ClientCommandId::new(),
            session_id: None,
            task_id: None,
            action: ClientCommandAction::CreateSession,
            payload: json!({"title": "auth refactor"}),
            idempotency_key: " ".to_owned(),
            state: ClientCommandState::Pending,
            requested_by: "terminal-user".to_owned(),
            requested_at: timestamp(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        };
        assert!(command.validate().is_err());
        command.idempotency_key = "session-create-1".to_owned();
        assert_eq!(command.validate(), Ok(()));
        command.requested_by.clear();
        assert!(command.validate().is_err());
    }

    #[test]
    fn typed_session_command_payloads_validate_and_round_trip() -> Result<(), serde_json::Error> {
        let create = CreateSessionCommandPayload {
            session_id: SessionId::new(),
            title: "auth refactor".to_owned(),
        };
        assert_eq!(create.validate(), Ok(()));
        assert_eq!(
            serde_json::from_value::<CreateSessionCommandPayload>(serde_json::to_value(&create)?)?,
            create
        );

        let append = AppendMessageCommandPayload {
            message_id: MessageId::new(),
            content: "continue task-03".to_owned(),
        };
        assert_eq!(append.validate(), Ok(()));
        assert_eq!(
            serde_json::from_value::<AppendMessageCommandPayload>(serde_json::to_value(&append)?)?,
            append
        );
        Ok(())
    }

    #[test]
    fn typed_session_command_payloads_reject_blank_user_text() {
        assert!(
            CreateSessionCommandPayload {
                session_id: SessionId::new(),
                title: " ".to_owned(),
            }
            .validate()
            .is_err()
        );
        assert!(
            AppendMessageCommandPayload {
                message_id: MessageId::new(),
                content: String::new(),
            }
            .validate()
            .is_err()
        );
    }
}
