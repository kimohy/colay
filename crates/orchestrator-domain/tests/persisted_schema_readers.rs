use chrono::Utc;
use orchestrator_domain::{
    AttemptId, Checkpoint, CheckpointId, HandoverBundle, HandoverId, ModelProfile, ProviderId,
    QuotaPeriod, QuotaScope, RoutingDecision, RoutingDecisionId, SchemaVersion, TaskEnvelope,
    UsageSnapshot, UsageUnit, WorkerOutcome, WorkerResult,
};
use serde::{Serialize, de::DeserializeOwned};

fn assert_v1_reader_fails_closed<T>(value: &T) -> Result<(), Box<dyn std::error::Error>>
where
    T: Serialize + DeserializeOwned,
{
    let encoded = serde_json::to_value(value)?;
    let _: T = serde_json::from_value(encoded.clone())?;

    let mut future = encoded;
    future["schema_version"] = serde_json::json!("999");
    assert!(serde_json::from_value::<T>(future).is_err());
    Ok(())
}

#[test]
#[allow(clippy::too_many_lines)]
fn persisted_v1_contract_readers_reject_future_versions() -> Result<(), Box<dyn std::error::Error>>
{
    let now = Utc::now();
    let task = TaskEnvelope::new("objective", "redacted request", now);
    assert_v1_reader_fails_closed(&task)?;

    let usage = UsageSnapshot::unknown(
        ProviderId::Codex,
        QuotaScope::new("monthly", QuotaPeriod::CalendarMonth, UsageUnit::Credits),
        now,
    );
    assert_v1_reader_fails_closed(&usage)?;

    let routing = RoutingDecision {
        schema_version: SchemaVersion::v1(),
        decision_id: RoutingDecisionId::new(),
        task_id: task.task_id,
        selected_provider: Some(ProviderId::Codex),
        selected_profile: Some(ModelProfile::Standard),
        reasoning_effort: None,
        parallel_workers: 1,
        candidate_scores: Vec::new(),
        rationale: vec!["fixture".to_owned()],
        downgrade: false,
        applied_policy: "default".to_owned(),
        blocked_options: Vec::new(),
        created_at: now,
    };
    assert_v1_reader_fails_closed(&routing)?;

    let worker_result = WorkerResult {
        schema_version: SchemaVersion::v1(),
        task_id: task.task_id,
        attempt_id: AttemptId::new(),
        provider: ProviderId::Codex,
        outcome: WorkerOutcome::Succeeded,
        exit_code: Some(0),
        session_id: None,
        summary: Some("done".to_owned()),
        commands: Vec::new(),
        tests: Vec::new(),
        started_at: now,
        finished_at: now,
        output_truncated: false,
    };
    assert_v1_reader_fails_closed(&worker_result)?;

    let checkpoint = Checkpoint {
        schema_version: SchemaVersion::v1(),
        checkpoint_id: CheckpointId::new(),
        task_id: task.task_id,
        attempt_id: worker_result.attempt_id,
        objective: task.objective.clone(),
        current_plan: Vec::new(),
        completed_steps: Vec::new(),
        pending_steps: Vec::new(),
        files_read: Vec::new(),
        files_changed: Vec::new(),
        git_base: None,
        diff_path: None,
        commands_run: Vec::new(),
        tests: Vec::new(),
        decisions: Vec::new(),
        unresolved_questions: Vec::new(),
        known_failures: Vec::new(),
        worker_claim: None,
        current_worker: ProviderId::Codex,
        concise_context_summary: "checkpoint".to_owned(),
        created_at: now,
        integrity_hash: String::new(),
    };
    assert_v1_reader_fails_closed(&checkpoint)?;

    let handover = HandoverBundle {
        schema_version: SchemaVersion::v1(),
        handover_id: HandoverId::new(),
        task_id: task.task_id,
        objective: task.objective,
        original_request: task.original_request_redacted,
        constraints: Vec::new(),
        acceptance_criteria: Vec::new(),
        current_plan: Vec::new(),
        completed_steps: Vec::new(),
        pending_steps: Vec::new(),
        files_read: Vec::new(),
        files_changed: Vec::new(),
        git_base: None,
        diff_path: None,
        commands_run: Vec::new(),
        tests: Vec::new(),
        decisions: Vec::new(),
        unresolved_questions: Vec::new(),
        known_failures: Vec::new(),
        current_worker: ProviderId::Codex,
        recommended_next_worker: ProviderId::Claude,
        usage_snapshots: vec![usage],
        concise_context_summary: "handover".to_owned(),
        created_at: now,
        integrity_hash: String::new(),
    };
    assert_v1_reader_fails_closed(&handover)?;
    Ok(())
}
