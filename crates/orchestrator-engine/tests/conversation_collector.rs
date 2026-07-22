use orchestrator_domain::{
    ConversationAttemptId, ConversationOutcome, MessageId, ProviderId, SandboxMode, SchemaVersion,
    SessionId,
};
use orchestrator_engine::{
    CONVERSATION_MAX_OUTPUT_BYTES, ConversationExit, ConversationFailure, ConversationRequest,
    ConversationResponse, collect_conversation_response,
};
use serde_json::json;

fn request() -> ConversationRequest {
    ConversationRequest {
        attempt_id: ConversationAttemptId::new(),
        session_id: SessionId::new(),
        source_message_id: MessageId::new(),
        transcript_redacted: "user: why does colay need Git?".to_owned(),
        repository_summary_redacted: "repository availability is unknown".to_owned(),
        sandbox: SandboxMode::ReadOnly,
    }
}

fn response(request: &ConversationRequest, output_redacted: Vec<u8>) -> ConversationResponse {
    ConversationResponse {
        schema_version: SchemaVersion::v1(),
        attempt_id: request.attempt_id,
        session_id: request.session_id,
        source_message_id: request.source_message_id,
        provider: ProviderId::Codex,
        sandbox: SandboxMode::ReadOnly,
        exit: ConversationExit::Succeeded,
        output_redacted,
        evidence_redacted: "fake provider exited 0".to_owned(),
    }
}

#[test]
fn accepts_one_strict_provider_neutral_outcome() -> Result<(), Box<dyn std::error::Error>> {
    let request = request();
    let output = serde_json::to_vec(&json!({
        "outcome": "answer_complete",
        "response_redacted": "Git is required only when writable work is approved."
    }))?;
    let outcome = collect_conversation_response(&request, response(&request, output))?;
    assert!(matches!(
        outcome,
        ConversationOutcome::AnswerComplete { .. }
    ));
    Ok(())
}

#[test]
fn rejects_prose_multiple_values_and_oversized_output() {
    let request = request();
    for output in [b"answer".to_vec(), b"{} {}".to_vec()] {
        assert!(matches!(
            collect_conversation_response(&request, response(&request, output)),
            Err(ConversationFailure::MalformedOutput { .. })
        ));
    }
    assert!(matches!(
        collect_conversation_response(
            &request,
            response(&request, vec![b'x'; CONVERSATION_MAX_OUTPUT_BYTES + 1])
        ),
        Err(ConversationFailure::OutputTooLarge { .. })
    ));
}

#[test]
fn rejects_identity_mismatch_and_mutable_sandbox() {
    let request = request();
    let output = br#"{"outcome":"answer_complete","response_redacted":"answer"}"#.to_vec();
    let mut wrong = response(&request, output.clone());
    wrong.source_message_id = MessageId::new();
    assert!(matches!(
        collect_conversation_response(&request, wrong),
        Err(ConversationFailure::IdentityMismatch { .. })
    ));
    let mut writable = response(&request, output);
    writable.sandbox = SandboxMode::WorkspaceWrite;
    assert_eq!(
        collect_conversation_response(&request, writable),
        Err(ConversationFailure::NotReadOnly)
    );
}

#[test]
fn rejects_incomplete_task_candidate() -> Result<(), Box<dyn std::error::Error>> {
    let request = request();
    let output = serde_json::to_vec(&json!({
        "outcome": "worktree_task_candidate",
        "response_redacted": "ready",
        "requirements": {
            "objective": "fix it",
            "in_scope": ["requested fix"],
            "out_of_scope": [],
            "constraints": [],
            "acceptance_criteria": ["passes"],
            "verification_plan": [{"executable": "cargo", "args": ["test"]}],
            "risks": [],
            "open_questions": ["which crate?"]
        }
    }))?;
    assert!(matches!(
        collect_conversation_response(&request, response(&request, output)),
        Err(ConversationFailure::Validation { .. })
    ));
    Ok(())
}

#[test]
fn lifecycle_failure_is_preserved_without_parsing() {
    let request = request();
    let mut failed = response(&request, Vec::new());
    failed.exit = ConversationExit::TimedOut;
    assert!(matches!(
        collect_conversation_response(&request, failed),
        Err(ConversationFailure::Lifecycle { .. })
    ));
}
