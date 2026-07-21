use chrono::{DateTime, Utc};
use orchestrator_domain::{
    AppendMessageCommandPayload, ClientCommand, ClientCommandAction, ClientCommandId,
    ConversationMessage, CorrelationId, CreateSessionCommandPayload, EventActor, EventId,
    EventType, MessageKind, MessageRole, MessageState, SchemaVersion, SessionState, TaskEvent,
};
use orchestrator_state::{Database, NewSessionRecord, StateError};
use sha2::{Digest as _, Sha256};

use crate::DaemonError;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandProcessingResult {
    Completed(ClientCommandId),
    Failed(ClientCommandId),
}

pub trait MessageRedactor: Send + Sync {
    fn redact(&self, value: &str) -> String;
}

/// Claims and processes one pending session command.
///
/// # Errors
///
/// Returns a daemon error when durable state cannot be read or mutated. Invalid
/// client payloads are recorded as failed commands and returned as a successful
/// failed processing outcome.
pub fn process_next_client_command(
    database: &Database,
    redactor: &dyn MessageRedactor,
    now: DateTime<Utc>,
) -> Result<Option<CommandProcessingResult>, DaemonError> {
    let Some(command) = database.claim_next_session_client_command(now)? else {
        return Ok(None);
    };
    match execute_command(database, redactor, &command) {
        Ok(outcome) => {
            database.complete_client_command(command.command_id, &outcome, now)?;
            Ok(Some(CommandProcessingResult::Completed(command.command_id)))
        }
        Err(CommandExecutionError::Rejected(reason)) => {
            database.fail_client_command(command.command_id, reason, now)?;
            Ok(Some(CommandProcessingResult::Failed(command.command_id)))
        }
        Err(CommandExecutionError::State(error)) => Err(error.into()),
    }
}

enum CommandExecutionError {
    Rejected(&'static str),
    State(StateError),
}

impl From<StateError> for CommandExecutionError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

fn execute_command(
    database: &Database,
    redactor: &dyn MessageRedactor,
    command: &ClientCommand,
) -> Result<String, CommandExecutionError> {
    match command.action {
        ClientCommandAction::CreateSession => create_session(database, redactor, command),
        ClientCommandAction::AppendMessage => append_message(database, redactor, command),
        ClientCommandAction::StopDaemon => Err(CommandExecutionError::Rejected(
            "stop command requires daemon lease reconciliation",
        )),
        ClientCommandAction::RequestPlan
        | ClientCommandAction::ApproveGraph
        | ClientCommandAction::ReviseGraph
        | ClientCommandAction::CancelPlan
        | ClientCommandAction::RequestIntegration
        | ClientCommandAction::ApproveIntegration
        | ClientCommandAction::CreateResolutionTask => Err(CommandExecutionError::Rejected(
            "orchestration command requires planning services",
        )),
    }
}

fn create_session(
    database: &Database,
    redactor: &dyn MessageRedactor,
    command: &ClientCommand,
) -> Result<String, CommandExecutionError> {
    if command.session_id.is_some() || command.task_id.is_some() {
        return Err(CommandExecutionError::Rejected(
            "create-session command has an invalid target",
        ));
    }
    let mut payload: CreateSessionCommandPayload = serde_json::from_value(command.payload.clone())
        .map_err(|_| CommandExecutionError::Rejected("create-session payload is invalid"))?;
    payload
        .validate()
        .map_err(|_| CommandExecutionError::Rejected("create-session payload is invalid"))?;
    payload.title = redactor.redact(payload.title.trim());
    if payload.title.trim().is_empty() {
        return Err(CommandExecutionError::Rejected(
            "create-session title is empty after redaction",
        ));
    }
    if let Some(existing) = database.load_session(payload.session_id)? {
        if existing.title == payload.title && existing.created_at == command.requested_at {
            return Ok(format!("session:{}", payload.session_id));
        }
        return Err(CommandExecutionError::Rejected(
            "create-session projection conflicts with the command",
        ));
    }
    let session = NewSessionRecord {
        session_id: payload.session_id,
        schema_version: SchemaVersion::V1.to_owned(),
        title: payload.title.clone(),
        state: SessionState::Drafting,
        created_at: command.requested_at,
    };
    database.create_session_with_event(
        &session,
        command_event(
            command,
            Some(payload.session_id),
            None,
            EventType::SessionCreated,
            serde_json::json!({
                "command_id": command.command_id,
                "title": payload.title,
            }),
        ),
    )?;
    Ok(format!("session:{}", payload.session_id))
}

fn append_message(
    database: &Database,
    redactor: &dyn MessageRedactor,
    command: &ClientCommand,
) -> Result<String, CommandExecutionError> {
    let session_id = command.session_id.ok_or(CommandExecutionError::Rejected(
        "append-message command requires a session target",
    ))?;
    if database.load_session(session_id)?.is_none() {
        return Err(CommandExecutionError::Rejected(
            "append-message session target does not exist",
        ));
    }
    if let Some(task_id) = command.task_id
        && database.load_task(task_id)?.is_none()
    {
        return Err(CommandExecutionError::Rejected(
            "append-message task target does not exist",
        ));
    }
    let payload: AppendMessageCommandPayload = serde_json::from_value(command.payload.clone())
        .map_err(|_| CommandExecutionError::Rejected("append-message payload is invalid"))?;
    payload
        .validate()
        .map_err(|_| CommandExecutionError::Rejected("append-message payload is invalid"))?;
    let content_redacted = redactor.redact(payload.content.trim());
    if content_redacted.trim().is_empty() {
        return Err(CommandExecutionError::Rejected(
            "append-message content is empty after redaction",
        ));
    }
    let expected = ConversationMessage {
        message_id: payload.message_id,
        session_id,
        task_id: command.task_id,
        role: MessageRole::User,
        kind: MessageKind::UserMessage,
        state: MessageState::Final,
        content_redacted,
        created_at: command.requested_at,
        finalized_at: Some(command.requested_at),
    };
    if let Some(existing) = database.load_message(payload.message_id)? {
        if existing == expected {
            return Ok(format!("message:{}", payload.message_id));
        }
        return Err(CommandExecutionError::Rejected(
            "append-message projection conflicts with the command",
        ));
    }
    let content_hash = hex::encode(Sha256::digest(expected.content_redacted.as_bytes()));
    database
        .append_message_with_event_and_instruction(
            &expected,
            command_event(
                command,
                Some(session_id),
                command.task_id,
                EventType::MessageAppended,
                serde_json::json!({
                    "command_id": command.command_id,
                    "message_id": payload.message_id,
                    "content_sha256": content_hash,
                }),
            ),
        )
        .map_err(|error| match error {
            StateError::InvalidRecord(_) if command.task_id.is_some() => {
                CommandExecutionError::Rejected(
                    "append-message task target cannot accept instructions",
                )
            }
            error => CommandExecutionError::State(error),
        })?;
    Ok(format!("message:{}", payload.message_id))
}

fn command_event(
    command: &ClientCommand,
    session_id: Option<orchestrator_domain::SessionId>,
    task_id: Option<orchestrator_domain::TaskId>,
    event_type: EventType,
    payload: serde_json::Value,
) -> TaskEvent {
    TaskEvent {
        schema_version: SchemaVersion::state_current(),
        sequence: 0,
        event_id: EventId::new(),
        session_id,
        task_id,
        occurred_at: command.requested_at,
        event_type,
        from_state: None,
        to_state: None,
        reason: None,
        actor: EventActor::User,
        correlation_id: CorrelationId::new(),
        causation_id: None,
        payload,
        previous_hash: None,
        event_hash: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone as _, Utc};
    use orchestrator_domain::{
        AppendMessageCommandPayload, ClientCommand, ClientCommandAction, ClientCommandId,
        ClientCommandState, ConversationMessage, CorrelationId, CreateSessionCommandPayload,
        EventActor, EventId, EventType, MessageKind, MessageRole, MessageState, SchemaVersion,
        SessionId, TaskEvent, TaskId,
    };
    use orchestrator_state::{Database, SessionListFilter, StateResult};

    use super::{CommandProcessingResult, MessageRedactor, process_next_client_command};

    struct SecretRedactor;

    impl MessageRedactor for SecretRedactor {
        fn redact(&self, value: &str) -> String {
            value.replace("secret", "[REDACTED]")
        }
    }

    fn timestamp() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 13, 0, 0)
            .single()
            .unwrap_or_default()
    }

    fn database() -> StateResult<Database> {
        let database = Database::open_in_memory()?;
        database.migrate_with_backup(std::path::Path::new("unused"))?;
        Ok(database)
    }

    fn command(
        action: ClientCommandAction,
        session_id: Option<SessionId>,
        task_id: Option<TaskId>,
        payload: serde_json::Value,
        key: &str,
    ) -> ClientCommand {
        ClientCommand {
            command_id: ClientCommandId::new(),
            session_id,
            task_id,
            action,
            payload,
            idempotency_key: key.to_owned(),
            state: ClientCommandState::Pending,
            requested_by: "tui".to_owned(),
            requested_at: timestamp(),
            claimed_at: None,
            completed_at: None,
            outcome: None,
        }
    }

    fn create_command(session_id: SessionId) -> ClientCommand {
        command(
            ClientCommandAction::CreateSession,
            None,
            None,
            serde_json::to_value(CreateSessionCommandPayload {
                session_id,
                title: "auth refactor".to_owned(),
            })
            .unwrap_or_default(),
            "create-session",
        )
    }

    fn append_command(
        session_id: SessionId,
        message_id: orchestrator_domain::MessageId,
        content: &str,
        key: &str,
    ) -> ClientCommand {
        command(
            ClientCommandAction::AppendMessage,
            Some(session_id),
            None,
            serde_json::to_value(AppendMessageCommandPayload {
                message_id,
                content: content.to_owned(),
            })
            .unwrap_or_default(),
            key,
        )
    }

    fn message_event(
        session_id: SessionId,
        message_id: orchestrator_domain::MessageId,
    ) -> TaskEvent {
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: Some(session_id),
            task_id: None,
            occurred_at: timestamp(),
            event_type: EventType::MessageAppended,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::User,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: serde_json::json!({"message_id": message_id}),
            previous_hash: None,
            event_hash: String::new(),
        }
    }

    #[test]
    fn create_session_and_append_message_commands_complete_with_redaction()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let session_id = SessionId::new();
        let create = create_command(session_id);
        database.submit_client_command(&create)?;
        assert_eq!(
            process_next_client_command(&database, &SecretRedactor, timestamp())?,
            Some(CommandProcessingResult::Completed(create.command_id))
        );
        assert_eq!(
            database
                .load_session(session_id)?
                .map(|session| session.title),
            Some("auth refactor".to_owned())
        );

        let message_id = orchestrator_domain::MessageId::new();
        let append = append_command(session_id, message_id, "token=secret", "append-message");
        database.submit_client_command(&append)?;
        assert_eq!(
            process_next_client_command(
                &database,
                &SecretRedactor,
                timestamp() + TimeDelta::seconds(1),
            )?,
            Some(CommandProcessingResult::Completed(append.command_id))
        );
        let message = database
            .load_message(message_id)?
            .ok_or("processed message is missing")?;
        assert_eq!(message.content_redacted, "token=[REDACTED]");
        assert_eq!(message.session_id, session_id);
        assert_eq!(
            database
                .load_client_command(append.command_id)?
                .map(|value| value.state),
            Some(ClientCommandState::Completed)
        );
        Ok(())
    }

    #[test]
    fn malformed_payload_fails_command_without_persisting_projection()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let malformed = command(
            ClientCommandAction::CreateSession,
            None,
            None,
            serde_json::json!({"title": 17}),
            "malformed",
        );
        database.submit_client_command(&malformed)?;
        assert_eq!(
            process_next_client_command(&database, &SecretRedactor, timestamp())?,
            Some(CommandProcessingResult::Failed(malformed.command_id))
        );
        assert_eq!(
            database
                .load_client_command(malformed.command_id)?
                .map(|value| value.state),
            Some(ClientCommandState::Failed)
        );
        assert!(
            database
                .list_sessions(&SessionListFilter::default())?
                .is_empty()
        );
        Ok(())
    }

    #[test]
    fn replay_after_projection_crash_completes_without_duplicate_message()
    -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let session_id = SessionId::new();
        database.submit_client_command(&create_command(session_id))?;
        process_next_client_command(&database, &SecretRedactor, timestamp())?;

        let message_id = orchestrator_domain::MessageId::new();
        let append = append_command(session_id, message_id, "secret", "crash-replay");
        database.submit_client_command(&append)?;
        database.claim_next_client_command(timestamp())?;
        let projected = ConversationMessage {
            message_id,
            session_id,
            task_id: None,
            role: MessageRole::User,
            kind: MessageKind::UserMessage,
            state: MessageState::Final,
            content_redacted: "[REDACTED]".to_owned(),
            created_at: append.requested_at,
            finalized_at: Some(append.requested_at),
        };
        database.append_message_with_event(&projected, message_event(session_id, message_id))?;
        database.recover_stale_client_commands(timestamp() + TimeDelta::seconds(1))?;

        assert_eq!(
            process_next_client_command(
                &database,
                &SecretRedactor,
                timestamp() + TimeDelta::seconds(2),
            )?,
            Some(CommandProcessingResult::Completed(append.command_id))
        );
        assert_eq!(database.messages_after(session_id, 0, 10)?.len(), 1);
        Ok(())
    }

    #[test]
    fn replay_with_mismatched_projection_fails_closed() -> Result<(), Box<dyn std::error::Error>> {
        let database = database()?;
        let session_id = SessionId::new();
        database.submit_client_command(&create_command(session_id))?;
        process_next_client_command(&database, &SecretRedactor, timestamp())?;

        let message_id = orchestrator_domain::MessageId::new();
        let append = append_command(session_id, message_id, "expected", "mismatch");
        database.submit_client_command(&append)?;
        database.claim_next_client_command(timestamp())?;
        let mismatched = ConversationMessage {
            message_id,
            session_id,
            task_id: None,
            role: MessageRole::User,
            kind: MessageKind::UserMessage,
            state: MessageState::Final,
            content_redacted: "other".to_owned(),
            created_at: append.requested_at,
            finalized_at: Some(append.requested_at),
        };
        database.append_message_with_event(&mismatched, message_event(session_id, message_id))?;
        database.recover_stale_client_commands(timestamp() + TimeDelta::seconds(1))?;

        assert_eq!(
            process_next_client_command(
                &database,
                &SecretRedactor,
                timestamp() + TimeDelta::seconds(2),
            )?,
            Some(CommandProcessingResult::Failed(append.command_id))
        );
        assert_eq!(database.messages_after(session_id, 0, 10)?.len(), 1);
        Ok(())
    }
}
