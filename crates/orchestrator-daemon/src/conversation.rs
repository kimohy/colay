use chrono::{DateTime, Utc};
use orchestrator_domain::{
    ClientCommand, ClientCommandAction, ClientCommandId, ClientCommandState, ConversationAttemptId,
    ConversationMessage, ConversationOutcome, CorrelationId, EventActor, EventId, EventType,
    MessageId, MessageKind, MessageRole, MessageState, RequestConversationTurnCommandPayload,
    RequestPlanCommandPayload, RequirementRevision, RequirementRevisionId, RequirementSnapshot,
    SandboxMode, SchemaVersion, SessionId, TaskEvent, VerificationCommand,
};
use orchestrator_engine::{
    CONVERSATION_MAX_EVIDENCE_BYTES, CollectedConversationResponse, ConversationFailure,
    ConversationFailureKind, ConversationOrchestrator, ConversationRequest,
    collect_conversation_response_with_evidence, diagnose_conversation_failure,
};
use orchestrator_state::{
    ConversationAttemptStatus, NewConversationAttempt, StateError, WorkspaceDatabase,
};

use crate::MessageRedactor;

#[derive(Debug, thiserror::Error)]
pub(crate) enum ConversationCommandError {
    #[error("{0}")]
    Rejected(String),
    #[error(transparent)]
    State(#[from] StateError),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ConversationProviderSelection {
    pub requested: Option<orchestrator_domain::ProviderId>,
    pub selected: orchestrator_domain::ProviderId,
    pub used_fallback: bool,
}

fn select_conversation_provider(
    requested: Option<orchestrator_domain::ProviderId>,
    candidates: &[orchestrator_domain::ProviderId],
) -> Result<ConversationProviderSelection, ConversationCommandError> {
    let selected = requested
        .filter(|provider| candidates.contains(provider))
        .or_else(|| candidates.first().copied())
        .ok_or_else(|| {
            ConversationCommandError::Rejected(
                "no evidenced provider is eligible for this conversation".to_owned(),
            )
        })?;
    Ok(ConversationProviderSelection {
        requested,
        selected,
        used_fallback: requested.is_some_and(|provider| provider != selected),
    })
}

pub(crate) async fn request_conversation_turn(
    database: &WorkspaceDatabase<'_>,
    orchestrator: &dyn ConversationOrchestrator,
    conversation_providers: &[orchestrator_domain::ProviderId],
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
    if let Some(existing) = stored.as_ref()
        && let Some(outcome) = existing.outcome.as_ref()
    {
        reconcile_outcome(
            database,
            command,
            session_id,
            payload.source_message_id,
            outcome,
        )?;
        return match existing.status {
            ConversationAttemptStatus::Succeeded => Ok(format!("conversation:{attempt_id}")),
            ConversationAttemptStatus::Failed | ConversationAttemptStatus::Cancelled => Err(
                ConversationCommandError::Rejected(failure_error_from_outcome(redactor, outcome)),
            ),
            ConversationAttemptStatus::Running => Err(ConversationCommandError::Rejected(
                "running conversation attempt unexpectedly has an outcome".to_owned(),
            )),
        };
    }

    let provider_selection =
        select_conversation_provider(payload.requested_provider, conversation_providers)?;

    database.begin_conversation_attempt(&NewConversationAttempt {
        attempt_id,
        session_id,
        source_message_id: payload.source_message_id,
        provider: provider_selection.selected,
        started_at: command.requested_at,
    })?;
    let transcript = database
        .latest_messages(session_id, 200)?
        .into_iter()
        .filter(|(_, message)| message.task_id.is_none())
        .map(|(_, message)| format!("{}: {}", role_name(message.role), message.content_redacted))
        .collect::<Vec<_>>()
        .join("\n");
    let request = ConversationRequest {
        attempt_id,
        session_id,
        source_message_id: payload.source_message_id,
        provider: provider_selection.selected,
        transcript_redacted: transcript,
        repository_summary_redacted:
            "Repository metadata is optional for conversation and required before approval"
                .to_owned(),
        sandbox: SandboxMode::ReadOnly,
    };
    let collected = match orchestrator.converse(request.clone()).await {
        Ok(response) => collect_conversation_response_with_evidence(&request, response)
            .and_then(|collected| redact_collected_outcome(redactor, collected)),
        Err(error) => Err(error),
    };
    finalize_conversation_turn(
        database,
        redactor,
        command,
        &request,
        provider_selection,
        collected,
        now,
    )
}

fn redact_collected_outcome(
    redactor: &dyn MessageRedactor,
    collected: CollectedConversationResponse,
) -> Result<CollectedConversationResponse, ConversationFailure> {
    let outcome = redact_conversation_outcome(redactor, collected.outcome);
    outcome
        .validate()
        .map_err(|source| ConversationFailure::Validation {
            source,
            evidence_redacted: bounded_redacted(redactor, &collected.evidence_redacted),
        })?;
    Ok(CollectedConversationResponse {
        outcome,
        evidence_redacted: collected.evidence_redacted,
    })
}

fn redact_conversation_outcome(
    redactor: &dyn MessageRedactor,
    outcome: ConversationOutcome,
) -> ConversationOutcome {
    match outcome {
        ConversationOutcome::AnswerComplete { response_redacted } => {
            ConversationOutcome::AnswerComplete {
                response_redacted: redactor.redact(&response_redacted),
            }
        }
        ConversationOutcome::MoreInformationNeeded {
            response_redacted,
            requirements,
        } => ConversationOutcome::MoreInformationNeeded {
            response_redacted: redactor.redact(&response_redacted),
            requirements: redact_requirement_snapshot(redactor, requirements),
        },
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted,
            requirements,
        } => ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: redactor.redact(&response_redacted),
            requirements: redact_requirement_snapshot(redactor, requirements),
        },
        ConversationOutcome::NeedsAttention {
            response_redacted,
            evidence_redacted,
        } => ConversationOutcome::NeedsAttention {
            response_redacted: redactor.redact(&response_redacted),
            evidence_redacted: redactor.redact(&evidence_redacted),
        },
    }
}

fn redact_requirement_snapshot(
    redactor: &dyn MessageRedactor,
    requirements: RequirementSnapshot,
) -> RequirementSnapshot {
    RequirementSnapshot {
        objective: redactor.redact(&requirements.objective),
        in_scope: redact_strings(redactor, requirements.in_scope),
        out_of_scope: redact_strings(redactor, requirements.out_of_scope),
        constraints: redact_strings(redactor, requirements.constraints),
        acceptance_criteria: redact_strings(redactor, requirements.acceptance_criteria),
        verification_plan: requirements
            .verification_plan
            .into_iter()
            .map(|command| redact_verification_command(redactor, command))
            .collect(),
        risks: redact_strings(redactor, requirements.risks),
        open_questions: redact_strings(redactor, requirements.open_questions),
    }
}

fn redact_verification_command(
    redactor: &dyn MessageRedactor,
    command: VerificationCommand,
) -> VerificationCommand {
    VerificationCommand {
        executable: redactor.redact(&command.executable),
        args: redact_strings(redactor, command.args),
    }
}

fn redact_strings(redactor: &dyn MessageRedactor, values: Vec<String>) -> Vec<String> {
    values
        .into_iter()
        .map(|value| redactor.redact(&value))
        .collect()
}

fn finalize_conversation_turn(
    database: &WorkspaceDatabase<'_>,
    redactor: &dyn MessageRedactor,
    command: &ClientCommand,
    request: &ConversationRequest,
    provider_selection: ConversationProviderSelection,
    collected: Result<CollectedConversationResponse, ConversationFailure>,
    now: DateTime<Utc>,
) -> Result<String, ConversationCommandError> {
    match collected {
        Ok(collected) => {
            let outcome = apply_provider_fallback_notice(collected.outcome, provider_selection);
            let evidence_redacted = bounded_redacted(redactor, &collected.evidence_redacted);
            let evidence_redacted =
                (!evidence_redacted.trim().is_empty()).then_some(evidence_redacted.as_str());
            database.finish_conversation_attempt_with_evidence(
                request.attempt_id,
                &outcome,
                evidence_redacted,
                now,
            )?;
            reconcile_outcome(
                database,
                command,
                request.session_id,
                request.source_message_id,
                &outcome,
            )?;
            Ok(format!("conversation:{}", request.attempt_id))
        }
        Err(failure) => {
            let diagnostic = diagnose_conversation_failure(provider_selection.selected, &failure);
            let status = if diagnostic.kind == ConversationFailureKind::Cancelled {
                ConversationAttemptStatus::Cancelled
            } else {
                ConversationAttemptStatus::Failed
            };
            let response_redacted = bounded_redacted(redactor, &diagnostic.response_redacted);
            let evidence_redacted = nonblank_failure_evidence(bounded_redacted(
                redactor,
                &diagnostic.evidence_redacted,
            ));
            let outcome = apply_provider_fallback_notice(
                ConversationOutcome::NeedsAttention {
                    response_redacted,
                    evidence_redacted: evidence_redacted.clone(),
                },
                provider_selection,
            );
            let error_redacted = failure_error_from_outcome(redactor, &outcome);
            database.finalize_conversation_failure(
                request.attempt_id,
                status,
                &outcome,
                &error_redacted,
                now,
            )?;
            reconcile_outcome(
                database,
                command,
                request.session_id,
                request.source_message_id,
                &outcome,
            )?;
            Err(ConversationCommandError::Rejected(error_redacted))
        }
    }
}

fn outcome_response(outcome: &ConversationOutcome) -> &str {
    match outcome {
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
    }
}

fn nonblank_failure_evidence(evidence_redacted: String) -> String {
    if evidence_redacted.trim().is_empty() {
        "provider failure evidence was fully redacted".to_owned()
    } else {
        evidence_redacted
    }
}

fn failure_error_from_outcome(
    redactor: &dyn MessageRedactor,
    outcome: &ConversationOutcome,
) -> String {
    bounded_redacted(redactor, outcome_response(outcome))
}

fn bounded_redacted(redactor: &dyn MessageRedactor, value: &str) -> String {
    const TRUNCATED: &str = "[truncated]";
    let redacted = redactor.redact(value);
    if redacted.len() <= CONVERSATION_MAX_EVIDENCE_BYTES {
        return redacted;
    }
    let mut end = CONVERSATION_MAX_EVIDENCE_BYTES.saturating_sub(TRUNCATED.len());
    while !redacted.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{TRUNCATED}", &redacted[..end])
}

fn apply_provider_fallback_notice(
    mut outcome: ConversationOutcome,
    selection: ConversationProviderSelection,
) -> ConversationOutcome {
    if let Some(requested) = selection.requested.filter(|_| selection.used_fallback) {
        let notice = format!(
            "Requested provider {requested} is unavailable; using {} for this read-only turn.\n",
            selection.selected
        );
        match &mut outcome {
            ConversationOutcome::AnswerComplete { response_redacted }
            | ConversationOutcome::MoreInformationNeeded {
                response_redacted, ..
            }
            | ConversationOutcome::WorktreeTaskCandidate {
                response_redacted, ..
            }
            | ConversationOutcome::NeedsAttention {
                response_redacted, ..
            } => response_redacted.insert_str(0, &notice),
        }
    }
    outcome
}

fn reconcile_outcome(
    database: &WorkspaceDatabase<'_>,
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
            database.submit_derived_client_command(
                command.command_id,
                &plan_command(command, source_message_id)?,
            )?;
        }
    }
    Ok(())
}

fn append_response(
    database: &WorkspaceDatabase<'_>,
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
        requested_by: source.requested_by.clone(),
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

#[cfg(test)]
mod tests {
    use orchestrator_domain::ProviderId;

    use super::select_conversation_provider;

    #[test]
    fn requested_provider_is_selected_when_it_is_eligible() -> Result<(), Box<dyn std::error::Error>>
    {
        let candidates = [ProviderId::Codex, ProviderId::Claude, ProviderId::Gemini];
        assert_eq!(
            select_conversation_provider(Some(ProviderId::Claude), &candidates)?.selected,
            ProviderId::Claude
        );
        Ok(())
    }

    #[test]
    fn unavailable_requested_provider_uses_the_first_eligible_candidate()
    -> Result<(), Box<dyn std::error::Error>> {
        let candidates = [ProviderId::Codex, ProviderId::Claude, ProviderId::Gemini];
        let selection = select_conversation_provider(Some(ProviderId::Agy), &candidates)?;
        assert_eq!(selection.selected, ProviderId::Codex);
        assert!(selection.used_fallback);
        Ok(())
    }
}
