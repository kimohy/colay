use orchestrator_domain::{
    ConversationAttemptId, ConversationOutcome, MessageId, ProviderId, SandboxMode, SchemaVersion,
    SessionId,
};
use orchestrator_engine::{
    CONVERSATION_MAX_EVIDENCE_BYTES, CONVERSATION_MAX_OUTPUT_BYTES, ConversationExit,
    ConversationFailure, ConversationFailureKind, ConversationRequest, ConversationResponse,
    collect_conversation_response, collect_conversation_response_with_evidence,
    diagnose_conversation_failure,
};
use serde_json::json;

fn request() -> ConversationRequest {
    ConversationRequest {
        attempt_id: ConversationAttemptId::new(),
        session_id: SessionId::new(),
        source_message_id: MessageId::new(),
        provider: ProviderId::Codex,
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
fn successful_collection_returns_bounded_evidence_separate_from_canonical_outcome()
-> Result<(), Box<dyn std::error::Error>> {
    let request = request();
    let output = serde_json::to_vec(&json!({
        "outcome": "answer_complete",
        "response_redacted": "Git is required only when writable work is approved."
    }))?;
    let mut response = response(&request, output);
    response.evidence_redacted = format!(
        "read-only provider command started\n{}",
        "x".repeat(CONVERSATION_MAX_EVIDENCE_BYTES * 2)
    );

    let collected = collect_conversation_response_with_evidence(&request, response)?;

    assert!(matches!(
        collected.outcome,
        ConversationOutcome::AnswerComplete { .. }
    ));
    assert!(
        collected
            .evidence_redacted
            .starts_with("read-only provider command started")
    );
    assert!(collected.evidence_redacted.ends_with("[truncated]"));
    assert!(collected.evidence_redacted.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
    assert_eq!(
        serde_json::to_value(&collected.outcome)?,
        json!({
            "outcome": "answer_complete",
            "response_redacted": "Git is required only when writable work is approved."
        })
    );
    Ok(())
}

#[test]
fn multibyte_success_evidence_is_valid_utf8_and_strictly_byte_bounded()
-> Result<(), Box<dyn std::error::Error>> {
    let request = request();
    let output = serde_json::to_vec(&json!({
        "outcome": "answer_complete",
        "response_redacted": "safe answer"
    }))?;
    let mut small = response(&request, output.clone());
    small.evidence_redacted = "read-only command: \u{d55c}\u{ae00}".to_owned();
    let small = collect_conversation_response_with_evidence(&request, small)?;
    assert!(small.evidence_redacted.contains("\u{d55c}\u{ae00}"));
    assert!(!small.evidence_redacted.ends_with("[truncated]"));

    let mut oversized = response(&request, output);
    oversized.evidence_redacted = "\u{d55c}".repeat(CONVERSATION_MAX_EVIDENCE_BYTES);
    let oversized = collect_conversation_response_with_evidence(&request, oversized)?;

    assert!(oversized.evidence_redacted.ends_with("[truncated]"));
    assert!(oversized.evidence_redacted.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
    assert!(std::str::from_utf8(oversized.evidence_redacted.as_bytes()).is_ok());
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
    let mut wrong_provider = response(&request, output.clone());
    wrong_provider.provider = ProviderId::Claude;
    assert!(matches!(
        collect_conversation_response(&request, wrong_provider),
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

#[test]
fn classifies_provider_failures_into_actionable_vendor_neutral_diagnostics() {
    let cases = [
        (
            ConversationFailure::Invocation {
                reason: "token_expired".to_owned(),
                evidence_redacted: "credential was [redacted]".to_owned(),
            },
            ConversationFailureKind::Authentication,
            "codex could not authenticate. Reauthenticate the selected provider, then retry this conversation.",
        ),
        (
            ConversationFailure::Invocation {
                reason: "Credit balance is too low".to_owned(),
                evidence_redacted: "billing request was rejected".to_owned(),
            },
            ConversationFailureKind::QuotaOrBilling,
            "codex cannot continue because quota or billing is unavailable. Check the selected provider's quota or billing, then retry this conversation.",
        ),
        (
            ConversationFailure::Invocation {
                reason: "UNSUPPORTED_CLIENT".to_owned(),
                evidence_redacted: "account rejected this client".to_owned(),
            },
            ConversationFailureKind::UnsupportedClientOrAccount,
            "codex is not supported by the installed client or current account. Update the selected provider client or use a supported account, then retry this conversation.",
        ),
        (
            ConversationFailure::Invocation {
                reason: "No supported transport is available".to_owned(),
                evidence_redacted: "client capability probe failed".to_owned(),
            },
            ConversationFailureKind::Compatibility,
            "codex is incompatible with the required read-only conversation protocol. Update or reconfigure the selected provider client, then retry this conversation.",
        ),
        (
            ConversationFailure::Lifecycle {
                exit: ConversationExit::TimedOut,
                evidence_redacted: "provider exceeded its deadline".to_owned(),
            },
            ConversationFailureKind::Timeout,
            "codex timed out. Check the selected provider's availability, then retry this conversation.",
        ),
        (
            ConversationFailure::Lifecycle {
                exit: ConversationExit::Cancelled,
                evidence_redacted: "request was cancelled".to_owned(),
            },
            ConversationFailureKind::Cancelled,
            "codex was cancelled. Retry this conversation when you are ready.",
        ),
        (
            ConversationFailure::Lifecycle {
                exit: ConversationExit::Crashed {
                    exit_code: Some(17),
                },
                evidence_redacted: "provider exited 17".to_owned(),
            },
            ConversationFailureKind::ProcessFailure,
            "codex process failed. Review the redacted evidence, then retry this conversation.",
        ),
    ];

    for (failure, expected_kind, expected_response) in cases {
        let diagnostic = diagnose_conversation_failure(ProviderId::Codex, &failure);
        assert_eq!(diagnostic.kind, expected_kind, "failure: {failure}");
        assert_eq!(
            diagnostic.response_redacted, expected_response,
            "failure: {failure}"
        );
    }
}

#[test]
fn failure_diagnostic_evidence_is_bounded() {
    let diagnostic = diagnose_conversation_failure(
        ProviderId::Codex,
        &ConversationFailure::Invocation {
            reason: "unknown provider failure".to_owned(),
            evidence_redacted: "x".repeat(CONVERSATION_MAX_EVIDENCE_BYTES * 2),
        },
    );

    assert_eq!(diagnostic.kind, ConversationFailureKind::ProcessFailure);
    assert!(diagnostic.evidence_redacted.ends_with("[truncated]"));
    assert!(diagnostic.evidence_redacted.len() <= CONVERSATION_MAX_EVIDENCE_BYTES);
}
