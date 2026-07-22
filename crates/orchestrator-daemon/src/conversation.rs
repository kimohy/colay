use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState, ConversationAttemptId,
    ConversationMessage, ConversationOutcome, CorrelationId, EventActor, EventId, EventType,
    MessageId, MessageKind, MessageRole, MessageState, RequestConversationTurnCommandPayload,
    RequestPlanCommandPayload, RequirementRevision, RequirementRevisionId, SandboxMode,
    SchemaVersion, SessionId, TaskEvent,
};
use orchestrator_engine::{
    ConversationOrchestrator, ConversationRequest, collect_conversation_response,
};
use orchestrator_state::{Database, NewConversationAttempt, StateError};

use crate::MessageRedactor;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConversationCommandError {
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    State(#[from] StateError),
}

pub(crate) async fn request_conversation_turn(
    database: &Database,
    orchestrator: &dyn ConversationOrchestrator,
    provider: orchestrator_domain::ProviderId,
    redactor: &dyn MessageRedactor,
    command: &ClientCommand,
    now: DateTime<Utc>,
) -> Result<String, ConversationCommandError> {
    let session_id = command.session_id.ok_or_else(|| {
        ConversationCommandError::Rejected(
            "conversation command requires a session target".to_owned(),
        )
    })?;
    if command.task_id.is_some() {
        return Err(ConversationCommandError::Rejected(
            "conversation command cannot target a task".to_owned(),
        ));
    }
    let payload: RequestConversationTurnCommandPayload =
        serde_json::from_value(command.payload.clone()).map_err(|_| {
            ConversationCommandError::Rejected("conversation payload is invalid".to_owned())
        })?;
    let source = database
        .load_message(payload.source_message_id)?
        .ok_or_else(|| {
            ConversationCommandError::Rejected(
                "conversation source message does not exist".to_owned(),
            )
        })?;
    if source.session_id != session_id
        || source.task_id.is_some()
        || source.role != MessageRole::User
        || source.state != MessageState::Final
    {
        return Err(ConversationCommandError::Rejected(
            "conversation source must be a final session-level user message".to_owned(),
        ));
    }

    let attempt_id = ConversationAttemptId::from_uuid(command.command_id.into_uuid());
    let stored = database.load_conversation_attempt(attempt_id)?;
    let outcome = if let Some(existing) = stored.as_ref().and_then(|value| value.outcome.clone()) {
        existing
    } else {
        database.begin_conversation_attempt(&NewConversationAttempt {
            attempt_id,
            session_id,
            source_message_id: payload.source_message_id,
            provider,
            started_at: command.requested_at,
        })?;
        let transcript = database
            .messages_after(session_id, 0, 200)?
            .into_iter()
            .filter(|(_, message)| message.task_id.is_none())
            .map(|(_, message)| {
                format!("{}: {}", role_name(message.role), message.content_redacted)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let request = ConversationRequest {
            attempt_id,
            session_id,
            source_message_id: payload.source_message_id,
            transcript_redacted: transcript,
            repository_summary_redacted:
                "Repository metadata is optional for conversation and required before approval"
                    .to_owned(),
            sandbox: SandboxMode::ReadOnly,
        };
        let collected = match orchestrator.converse(request.clone()).await {
            Ok(response) => collect_conversation_response(&request, response),
            Err(error) => Err(error),
        };
        let outcome = collected.unwrap_or_else(|error| ConversationOutcome::NeedsAttention {
            response_redacted: "The read-only conversation provider needs attention; your message and session were preserved.".to_owned(),
            evidence_redacted: redactor.redact(&error.to_string()),
        });
        database.finish_conversation_attempt(attempt_id, &outcome, now)?;
        outcome
    };
    reconcile_outcome(
        database,
        command,
        session_id,
        payload.source_message_id,
        &outcome,
    )?;
    Ok(format!("conversation:{attempt_id}"))
}

fn reconcile_outcome(
    database: &Database,
    command: &ClientCommand,
    session_id: SessionId,
    source_message_id: MessageId,
    outcome: &ConversationOutcome,
) -> Result<(), ConversationCommandError> {
    let response = match outcome {
        ConversationOutcome::AnswerComplete { response_redacted }
        | ConversationOutcome::MoreInformationNeeded {
            response_redacted, ..
        }
        | ConversationOutcome::WorktreeTaskCandidate {
            response_redacted, ..
        }
        | ConversationOutcome::NeedsAttention {
            response_redacted, ..
        } => response_redacted,
    };
    append_response(database, command, session_id, response)?;

    let requirements = match outcome {
        ConversationOutcome::MoreInformationNeeded { requirements, .. }
        | ConversationOutcome::WorktreeTaskCandidate { requirements, .. } => Some(requirements),
        ConversationOutcome::AnswerComplete { .. } | ConversationOutcome::NeedsAttention { .. } => {
            None
        }
    };
    if let Some(snapshot) = requirements {
        let requirement_revision_id =
            RequirementRevisionId::from_uuid(command.command_id.into_uuid());
        let current = database.current_requirement_revision(session_id)?;
        let ordinal = current.as_ref().map_or(1, |revision| {
            if revision.requirement_revision_id == requirement_revision_id {
                revision.ordinal
            } else {
                revision.ordinal.saturating_add(1)
            }
        });
        let revision = RequirementRevision::seal(
            requirement_revision_id,
            session_id,
            source_message_id,
            ordinal,
            snapshot.clone(),
            command.requested_at,
        )
        .map_err(|error| ConversationCommandError::Rejected(error.to_string()))?;
        database.record_requirement_revision(&revision)?;
        if matches!(outcome, ConversationOutcome::WorktreeTaskCandidate { .. }) {
            database.submit_client_command(&plan_command(command, source_message_id)?)?;
        }
    }
    Ok(())
}

fn append_response(
    database: &Database,
    command: &ClientCommand,
    session_id: SessionId,
    content: &str,
) -> Result<(), ConversationCommandError> {
    let timestamp = command.requested_at;
    let message_id = derived_message_id(command.command_id, 0x40);
    let expected = ConversationMessage {
        message_id,
        session_id,
        task_id: None,
        role: MessageRole::Orchestrator,
        kind: MessageKind::OrchestratorMessage,
        state: MessageState::Final,
        content_redacted: content.to_owned(),
        created_at: timestamp,
        finalized_at: Some(timestamp),
    };
    if let Some(existing) = database.load_message(message_id)? {
        if existing == expected {
            return Ok(());
        }
        return Err(ConversationCommandError::Rejected(
            "conversation response replay conflicts with stored message".to_owned(),
        ));
    }
    database.append_message_with_event(
        &expected,
        TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            session_id: Some(session_id),
            task_id: None,
            occurred_at: timestamp,
            event_type: EventType::MessageAppended,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::Orchestrator,
            correlation_id: CorrelationId::from_uuid(command.command_id.into_uuid()),
            causation_id: Some(EventId::from_uuid(command.command_id.into_uuid())),
            payload: serde_json::json!({
                "command_id": command.command_id,
                "message_id": message_id,
                "conversation_outcome": true,
            }),
            previous_hash: None,
            event_hash: String::new(),
        },
    )?;
    Ok(())
}

fn plan_command(
    source: &ClientCommand,
    goal_message_id: MessageId,
) -> Result<ClientCommand, ConversationCommandError> {
    let command_id = derived_command_id(source.command_id, 0x80);
    Ok(ClientCommand {
        command_id,
        session_id: source.session_id,
        task_id: None,
        action: ClientCommandAction::RequestPlan,
        payload: serde_json::to_value(RequestPlanCommandPayload { goal_message_id })
            .map_err(StateError::from)?,
        idempotency_key: format!("conversation-plan-{}", source.command_id),
        state: ClientCommandState::Pending,
        requested_by: "conversation-orchestrator".to_owned(),
        requested_at: source.requested_at,
        claimed_at: None,
        completed_at: None,
        outcome: None,
    })
}

fn derived_command_id(source: ClientCommandId, mask: u8) -> ClientCommandId {
    let mut bytes = *source.as_uuid().as_bytes();
    bytes[0] ^= mask;
    ClientCommandId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

fn derived_message_id(source: ClientCommandId, mask: u8) -> MessageId {
    let mut bytes = *source.as_uuid().as_bytes();
    bytes[0] ^= mask;
    MessageId::from_uuid(uuid::Uuid::from_bytes(bytes))
}

const fn role_name(role: MessageRole) -> &'static str {
    match role {
        MessageRole::User => "user",
        MessageRole::Orchestrator => "orchestrator",
        MessageRole::Agent => "agent",
        MessageRole::System => "system",
    }
}
