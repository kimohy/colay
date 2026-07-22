use std::collections::BTreeMap;

use chrono::{TimeZone as _, Utc};
use orchestrator_domain::{
    ClientCommandAction, ConversationOutcome, GraphRevisionId, MessageId, ModelProfile, ProviderId,
    RepoValidationEvidence, RequestConversationTurnCommandPayload, RequirementRevision,
    RequirementRevisionId, RequirementSnapshot, SchemaVersion, SessionId, SessionState,
    VerificationCommand,
};

fn snapshot(open_questions: Vec<String>) -> RequirementSnapshot {
    RequirementSnapshot {
        objective: "make ordinary chat conversation-first".to_owned(),
        in_scope: vec!["session-level conversation".to_owned()],
        out_of_scope: vec!["automatic merge or push".to_owned()],
        constraints: vec!["do not create tasks before approval".to_owned()],
        acceptance_criteria: vec!["answer-only chat creates zero tasks".to_owned()],
        verification_plan: vec![VerificationCommand {
            executable: "cargo".to_owned(),
            args: vec![
                "test".to_owned(),
                "--workspace".to_owned(),
                "--all-features".to_owned(),
            ],
        }],
        risks: vec!["stale approval".to_owned()],
        open_questions,
    }
}

#[test]
fn candidate_verification_rejects_shell_interpolation() {
    let mut candidate = snapshot(Vec::new());
    candidate.verification_plan[0].executable = "cargo && git".to_owned();
    assert!(
        ConversationOutcome::WorktreeTaskCandidate {
            response_redacted: "ready".to_owned(),
            requirements: candidate,
        }
        .validate()
        .is_err()
    );
}

#[test]
fn conversation_outcomes_are_provider_neutral_and_strict() -> Result<(), Box<dyn std::error::Error>>
{
    let answer = ConversationOutcome::AnswerComplete {
        response_redacted: "Git is only needed for writable execution.".to_owned(),
    };
    let encoded = serde_json::to_value(&answer)?;
    assert_eq!(encoded["outcome"], "answer_complete");
    assert_eq!(
        serde_json::from_value::<ConversationOutcome>(encoded)?,
        answer
    );

    let candidate = ConversationOutcome::WorktreeTaskCandidate {
        response_redacted: "The requirement is ready for validation.".to_owned(),
        requirements: snapshot(Vec::new()),
    };
    assert_eq!(candidate.validate(), Ok(()));

    let incomplete = ConversationOutcome::WorktreeTaskCandidate {
        response_redacted: "ready".to_owned(),
        requirements: snapshot(vec!["Which package?".to_owned()]),
    };
    assert!(incomplete.validate().is_err());
    Ok(())
}

#[test]
fn requirement_revision_hash_is_deterministic_and_material_changes_reseal()
-> Result<(), Box<dyn std::error::Error>> {
    let session_id = SessionId::new();
    let source_message_id = MessageId::new();
    let created_at = Utc
        .with_ymd_and_hms(2026, 7, 22, 9, 0, 0)
        .single()
        .unwrap_or_default();
    let first = RequirementRevision::seal(
        RequirementRevisionId::new(),
        session_id,
        source_message_id,
        1,
        snapshot(Vec::new()),
        created_at,
    )?;
    let replay = RequirementRevision::seal(
        first.requirement_revision_id,
        session_id,
        source_message_id,
        1,
        first.snapshot.clone(),
        created_at,
    )?;
    assert_eq!(first.snapshot_hash, replay.snapshot_hash);

    let mut changed = first.snapshot.clone();
    changed.constraints.push("Windows and WSL".to_owned());
    let changed = RequirementRevision::seal(
        RequirementRevisionId::new(),
        session_id,
        source_message_id,
        2,
        changed,
        created_at,
    )?;
    assert_ne!(first.snapshot_hash, changed.snapshot_hash);
    Ok(())
}

#[test]
fn conversation_command_and_validation_state_are_explicit() -> Result<(), Box<dyn std::error::Error>>
{
    let payload = RequestConversationTurnCommandPayload {
        source_message_id: MessageId::new(),
    };
    assert!(serde_json::to_value(payload).is_ok());
    assert_eq!(
        SessionState::Planning.validate_transition(SessionState::Validating),
        Ok(())
    );
    assert_eq!(
        SessionState::Validating.validate_transition(SessionState::AwaitingApproval),
        Ok(())
    );
    assert_eq!(
        serde_json::to_value(ClientCommandAction::RequestConversationTurn)?,
        "request_conversation_turn"
    );
    Ok(())
}

#[test]
fn repository_validation_evidence_is_sealable_approval_authority()
-> Result<(), Box<dyn std::error::Error>> {
    let evidence = RepoValidationEvidence {
        schema_version: SchemaVersion::v1(),
        requirement_revision_id: RequirementRevisionId::new(),
        requirement_snapshot_hash: "d".repeat(64),
        graph_revision_id: GraphRevisionId::new(),
        git_root_redacted: "C:/repo".to_owned(),
        base_commit: "a".repeat(40),
        eligible_providers: vec![ProviderId::Codex],
        eligible_profiles: vec![ModelProfile::Standard],
        max_parallel_workers: 2,
        per_provider_limits: BTreeMap::from([(ProviderId::Codex, 1)]),
        normalized_write_scopes: vec!["crates/orchestrator-domain".to_owned()],
        verification_plan: snapshot(Vec::new()).verification_plan,
        required_approvals: vec!["exact validated graph approval".to_owned()],
        checks: vec!["git_ready".to_owned(), "graph_valid".to_owned()],
        validated_at: Utc
            .with_ymd_and_hms(2026, 7, 22, 9, 1, 0)
            .single()
            .unwrap_or_default(),
    };
    let hash = evidence.seal()?;
    assert_eq!(hash.len(), 64);
    assert!(evidence.validate_for(ProviderId::Codex).is_ok());
    Ok(())
}
