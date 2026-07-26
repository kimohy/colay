PRAGMA defer_foreign_keys = ON;

-- A pre-v13 database represented one repository. Preserve those rows in a single
-- reserved partition, but do not create that partition for a fresh global image.
INSERT INTO workspaces(workspace_id, kind, status, created_at, last_seen_at)
SELECT '00000000-0000-0000-0000-000000000001', 'directory', 'detached',
       strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), strftime('%Y-%m-%dT%H:%M:%fZ', 'now')
WHERE EXISTS(SELECT 1 FROM tasks)
   OR EXISTS(SELECT 1 FROM sessions)
   OR EXISTS(SELECT 1 FROM task_events)
   OR EXISTS(SELECT 1 FROM client_commands)
   OR EXISTS(SELECT 1 FROM approval_records)
   OR EXISTS(SELECT 1 FROM artifacts WHERE task_id IS NOT NULL)
   OR EXISTS(SELECT 1 FROM provider_usage_snapshots WHERE task_id IS NOT NULL);

DROP TRIGGER conversation_attempts_immutable_identity;
DROP TRIGGER conversation_attempts_no_delete;
DROP TRIGGER conversation_attempts_single_completion;
DROP TRIGGER graph_approvals_no_delete;
DROP TRIGGER graph_approvals_no_update;
DROP TRIGGER graph_revision_authority_immutable;
DROP TRIGGER graph_revision_provider_v2_immutable;
DROP TRIGGER graph_revisions_immutable_payload;
DROP TRIGGER graph_revisions_no_delete;
DROP TRIGGER integration_approvals_no_delete;
DROP TRIGGER integration_approvals_no_update;
DROP TRIGGER integration_batches_no_delete;
DROP TRIGGER integration_batches_payload_immutable;
DROP TRIGGER integration_sources_no_delete;
DROP TRIGGER integration_sources_no_update;
DROP TRIGGER planning_attempt_provider_v2_immutable;
DROP TRIGGER planning_attempts_immutable_identity;
DROP TRIGGER planning_attempts_no_delete;
DROP TRIGGER planning_attempts_single_completion;
DROP TRIGGER requirement_revisions_no_delete;
DROP TRIGGER requirement_revisions_no_update;
DROP TRIGGER sessions_legacy_state_sync;
DROP TRIGGER task_instructions_identity_immutable;
DROP TRIGGER task_instructions_transition_guard;

ALTER TABLE tasks RENAME TO tasks_v12;
ALTER TABLE task_attempts RENAME TO task_attempts_v12;
ALTER TABLE provider_usage_snapshots RENAME TO provider_usage_snapshots_v12;
ALTER TABLE routing_decisions RENAME TO routing_decisions_v12;
ALTER TABLE routing_decision_usage RENAME TO routing_decision_usage_v12;
ALTER TABLE artifacts RENAME TO artifacts_v12;
ALTER TABLE command_evidence RENAME TO command_evidence_v12;
ALTER TABLE checkpoints RENAME TO checkpoints_v12;
ALTER TABLE handovers RENAME TO handovers_v12;
ALTER TABLE verification_results RENAME TO verification_results_v12;
ALTER TABLE task_events RENAME TO task_events_v12;
ALTER TABLE event_log_state RENAME TO event_log_state_v12;
ALTER TABLE task_controls RENAME TO task_controls_v12;
ALTER TABLE worktrees RENAME TO worktrees_v12;
ALTER TABLE coordinator_leases RENAME TO coordinator_leases_v12;
ALTER TABLE worker_leases RENAME TO worker_leases_v12;
ALTER TABLE changed_files RENAME TO changed_files_v12;
ALTER TABLE approval_records RENAME TO approval_records_v12;
ALTER TABLE compatibility_runs RENAME TO compatibility_runs_v12;
ALTER TABLE sessions RENAME TO sessions_v12;
ALTER TABLE conversation_messages RENAME TO conversation_messages_v12;
ALTER TABLE client_commands RENAME TO client_commands_v12;
ALTER TABLE session_workspace_state RENAME TO session_workspace_state_v12;
ALTER TABLE graph_revisions RENAME TO graph_revisions_v12;
ALTER TABLE planning_attempts RENAME TO planning_attempts_v12;
ALTER TABLE session_tasks RENAME TO session_tasks_v12;
ALTER TABLE task_dependencies RENAME TO task_dependencies_v12;
ALTER TABLE session_graph_heads RENAME TO session_graph_heads_v12;
ALTER TABLE graph_approvals RENAME TO graph_approvals_v12;
ALTER TABLE task_schedule_claims RENAME TO task_schedule_claims_v12;
ALTER TABLE resource_claims RENAME TO resource_claims_v12;
ALTER TABLE task_instructions RENAME TO task_instructions_v12;
ALTER TABLE integration_batches RENAME TO integration_batches_v12;
ALTER TABLE integration_sources RENAME TO integration_sources_v12;
ALTER TABLE integration_approvals RENAME TO integration_approvals_v12;
ALTER TABLE integration_applications RENAME TO integration_applications_v12;
ALTER TABLE integration_resolution_tasks RENAME TO integration_resolution_tasks_v12;
ALTER TABLE conversation_attempts RENAME TO conversation_attempts_v12;
ALTER TABLE requirement_revisions RENAME TO requirement_revisions_v12;
ALTER TABLE session_requirement_heads RENAME TO session_requirement_heads_v12;

CREATE TABLE tasks (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    task_id TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    state TEXT NOT NULL CHECK (state IN (
        'queued', 'analyzing', 'planned', 'running', 'checkpoint_requested',
        'checkpointing', 'checkpointed', 'handover_requested', 'handing_over',
        'resuming', 'verifying', 'completed', 'blocked', 'failed', 'cancelled'
    )),
    resume_state TEXT CHECK (resume_state IS NULL OR resume_state IN (
        'queued', 'analyzing', 'planned', 'running', 'checkpoint_requested',
        'checkpointing', 'checkpointed', 'handover_requested', 'handing_over',
        'resuming', 'verifying', 'completed', 'blocked', 'failed', 'cancelled'
    )),
    paused INTEGER NOT NULL DEFAULT 0 CHECK (paused IN (0, 1)),
    objective TEXT NOT NULL,
    original_request_redacted TEXT NOT NULL,
    task_envelope_json TEXT NOT NULL CHECK (json_valid(task_envelope_json)),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    archived_at TEXT,
    PRIMARY KEY(workspace_id, task_id)
) STRICT;

CREATE TABLE task_attempts (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    attempt_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    provider_id TEXT,
    worker_mode TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    outcome TEXT,
    worker_result_json TEXT CHECK (worker_result_json IS NULL OR json_valid(worker_result_json)),
    PRIMARY KEY(workspace_id, attempt_id),
    UNIQUE(workspace_id, task_id, ordinal),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE provider_usage_snapshots (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    snapshot_id TEXT NOT NULL,
    task_id TEXT,
    provider_id TEXT NOT NULL,
    quota_scope TEXT NOT NULL,
    quota_period TEXT NOT NULL,
    usage_unit TEXT NOT NULL,
    used REAL CHECK (used IS NULL OR used >= 0),
    quota_limit REAL CHECK (quota_limit IS NULL OR quota_limit > 0),
    remaining REAL CHECK (remaining IS NULL OR remaining >= 0),
    used_percent REAL CHECK (used_percent IS NULL OR used_percent BETWEEN 0 AND 100),
    remaining_percent REAL CHECK (remaining_percent IS NULL OR remaining_percent BETWEEN 0 AND 100),
    period_started_at TEXT,
    resets_at TEXT,
    source TEXT NOT NULL,
    confidence TEXT NOT NULL,
    snapshot_json TEXT NOT NULL CHECK (json_valid(snapshot_json)),
    collected_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, snapshot_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE global_provider_usage_snapshots (
    snapshot_id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL,
    quota_scope TEXT NOT NULL,
    quota_period TEXT NOT NULL,
    usage_unit TEXT NOT NULL,
    used REAL CHECK (used IS NULL OR used >= 0),
    quota_limit REAL CHECK (quota_limit IS NULL OR quota_limit > 0),
    remaining REAL CHECK (remaining IS NULL OR remaining >= 0),
    used_percent REAL CHECK (used_percent IS NULL OR used_percent BETWEEN 0 AND 100),
    remaining_percent REAL CHECK (remaining_percent IS NULL OR remaining_percent BETWEEN 0 AND 100),
    period_started_at TEXT,
    resets_at TEXT,
    source TEXT NOT NULL,
    confidence TEXT NOT NULL,
    snapshot_json TEXT NOT NULL CHECK (json_valid(snapshot_json)),
    collected_at TEXT NOT NULL
) STRICT;

CREATE TABLE routing_decisions (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    decision_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    selected_provider TEXT,
    model_profile TEXT,
    effort TEXT,
    difficulty TEXT NOT NULL,
    risk_json TEXT NOT NULL CHECK (json_valid(risk_json)),
    candidates_json TEXT NOT NULL CHECK (json_valid(candidates_json)),
    policy_json TEXT NOT NULL CHECK (json_valid(policy_json)),
    downgraded INTEGER NOT NULL DEFAULT 0 CHECK (downgraded IN (0, 1)),
    rationale_json TEXT NOT NULL CHECK (json_valid(rationale_json)),
    schema_version TEXT NOT NULL,
    decided_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, decision_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE routing_decision_usage (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    decision_id TEXT NOT NULL,
    snapshot_id TEXT NOT NULL,
    PRIMARY KEY(workspace_id, decision_id, snapshot_id),
    FOREIGN KEY(workspace_id, decision_id)
        REFERENCES routing_decisions(workspace_id, decision_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, snapshot_id)
        REFERENCES provider_usage_snapshots(workspace_id, snapshot_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE artifacts (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    artifact_id TEXT NOT NULL,
    task_id TEXT,
    kind TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    media_type TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, artifact_id),
    UNIQUE(workspace_id, relative_path),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE global_artifacts (
    artifact_id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL,
    relative_path TEXT NOT NULL UNIQUE,
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    media_type TEXT,
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE command_evidence (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    command_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    attempt_id TEXT,
    executable TEXT NOT NULL,
    args_json TEXT NOT NULL CHECK (json_valid(args_json)),
    working_directory TEXT,
    exit_code INTEGER,
    termination TEXT NOT NULL,
    stdout_artifact_id TEXT,
    stderr_artifact_id TEXT,
    stdout_truncated INTEGER NOT NULL CHECK (stdout_truncated IN (0, 1)),
    stderr_truncated INTEGER NOT NULL CHECK (stderr_truncated IN (0, 1)),
    invalid_utf8 INTEGER NOT NULL CHECK (invalid_utf8 IN (0, 1)),
    started_at TEXT NOT NULL,
    ended_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, command_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, attempt_id)
        REFERENCES task_attempts(workspace_id, attempt_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, stdout_artifact_id)
        REFERENCES artifacts(workspace_id, artifact_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, stderr_artifact_id)
        REFERENCES artifacts(workspace_id, artifact_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE checkpoints (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    checkpoint_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    attempt_id TEXT,
    schema_version TEXT NOT NULL,
    checkpoint_json TEXT NOT NULL CHECK (json_valid(checkpoint_json)),
    integrity_hash TEXT NOT NULL CHECK (length(integrity_hash) = 64),
    diff_artifact_id TEXT,
    git_head TEXT,
    created_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, checkpoint_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, attempt_id)
        REFERENCES task_attempts(workspace_id, attempt_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, diff_artifact_id)
        REFERENCES artifacts(workspace_id, artifact_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE handovers (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    handover_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    checkpoint_id TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    from_provider TEXT NOT NULL,
    to_provider TEXT NOT NULL,
    reason TEXT NOT NULL,
    bundle_json TEXT NOT NULL CHECK (json_valid(bundle_json)),
    integrity_hash TEXT NOT NULL CHECK (length(integrity_hash) = 64),
    acknowledgement_json TEXT CHECK (acknowledgement_json IS NULL OR json_valid(acknowledgement_json)),
    started_at TEXT NOT NULL,
    completed_at TEXT,
    PRIMARY KEY(workspace_id, handover_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, checkpoint_id)
        REFERENCES checkpoints(workspace_id, checkpoint_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE verification_results (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    verification_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    attempt_id TEXT,
    reviewer_provider TEXT,
    outcome TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    result_json TEXT NOT NULL CHECK (json_valid(result_json)),
    started_at TEXT NOT NULL,
    completed_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, verification_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, attempt_id)
        REFERENCES task_attempts(workspace_id, attempt_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE task_events (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    sequence INTEGER NOT NULL CHECK(sequence > 0),
    event_id TEXT NOT NULL,
    task_id TEXT,
    event_type TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    event_json TEXT NOT NULL CHECK (json_valid(event_json)),
    previous_hash TEXT,
    event_hash TEXT NOT NULL CHECK (length(event_hash) = 64),
    exported_at TEXT,
    session_id TEXT,
    PRIMARY KEY(workspace_id, sequence),
    UNIQUE(workspace_id, event_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE event_log_state (
    workspace_id TEXT PRIMARY KEY NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    last_exported_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_exported_sequence >= 0),
    last_exported_hash TEXT,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE task_controls (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    control_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    action TEXT NOT NULL,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    requested_by TEXT NOT NULL,
    requested_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome TEXT,
    PRIMARY KEY(workspace_id, control_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE worktrees (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    worktree_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    repo_root TEXT NOT NULL,
    worktree_path TEXT NOT NULL,
    branch_name TEXT NOT NULL,
    base_revision TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    cleanup_approved_at TEXT,
    archived_at TEXT,
    PRIMARY KEY(workspace_id, worktree_id),
    UNIQUE(workspace_id, worktree_path),
    UNIQUE(workspace_id, branch_name),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE coordinator_leases (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    lease_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    worktree_id TEXT,
    owner_id TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    renewed_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT,
    PRIMARY KEY(workspace_id, lease_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, worktree_id)
        REFERENCES worktrees(workspace_id, worktree_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE worker_leases (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    lease_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    worktree_id TEXT,
    coordinator_lease_id TEXT,
    provider_id TEXT NOT NULL,
    mode TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT,
    PRIMARY KEY(workspace_id, lease_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, worktree_id)
        REFERENCES worktrees(workspace_id, worktree_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, coordinator_lease_id)
        REFERENCES coordinator_leases(workspace_id, lease_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE changed_files (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    task_id TEXT NOT NULL,
    worktree_id TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    owner_lease_id TEXT,
    sha256 TEXT,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, task_id, relative_path),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, worktree_id)
        REFERENCES worktrees(workspace_id, worktree_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, owner_lease_id)
        REFERENCES worker_leases(workspace_id, lease_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE approval_records (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    approval_id TEXT NOT NULL,
    task_id TEXT,
    action TEXT NOT NULL,
    scope_json TEXT NOT NULL CHECK (json_valid(scope_json)),
    approved_by TEXT NOT NULL,
    approved_at TEXT NOT NULL,
    expires_at TEXT,
    revoked_at TEXT,
    PRIMARY KEY(workspace_id, approval_id),
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE sessions (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    session_id TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    revision INTEGER NOT NULL DEFAULT 0 CHECK (revision >= 0),
    title TEXT NOT NULL CHECK (length(trim(title)) > 0),
    state TEXT NOT NULL CHECK (state IN (
        'drafting', 'planning', 'awaiting_approval', 'running',
        'needs_attention', 'integrating', 'verifying', 'completed',
        'stopping', 'cancelled'
    )),
    created_at TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    archived_at TEXT,
    state_v2 TEXT CHECK (state_v2 IS NULL OR state_v2 IN (
        'drafting', 'planning', 'validating', 'awaiting_approval', 'running',
        'needs_attention', 'integrating', 'verifying', 'completed',
        'stopping', 'cancelled'
    )),
    PRIMARY KEY(workspace_id, session_id)
) STRICT;

CREATE TABLE conversation_messages (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    message_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    role TEXT NOT NULL CHECK (role IN ('user', 'orchestrator', 'agent', 'system')),
    kind TEXT NOT NULL CHECK (kind IN (
        'user_message', 'orchestrator_message', 'agent_message', 'plan',
        'tool_summary', 'state_change', 'approval_request', 'warning', 'error'
    )),
    state TEXT NOT NULL CHECK (state IN ('streaming', 'final', 'interrupted', 'rejected')),
    content_redacted TEXT NOT NULL,
    created_at TEXT NOT NULL,
    finalized_at TEXT,
    PRIMARY KEY(workspace_id, message_id),
    UNIQUE(workspace_id, session_id, ordinal),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE client_commands (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()) REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    command_id TEXT NOT NULL,
    session_id TEXT,
    task_id TEXT,
    action TEXT NOT NULL CHECK (action IN (
        'create_session', 'append_message', 'request_conversation_turn', 'stop_daemon',
        'request_plan', 'approve_graph', 'revise_graph', 'cancel_plan',
        'request_integration', 'approve_integration', 'create_resolution_task'
    )),
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    idempotency_key TEXT NOT NULL CHECK (length(trim(idempotency_key)) > 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'completed', 'failed')),
    requested_by TEXT NOT NULL CHECK (length(trim(requested_by)) > 0),
    requested_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome TEXT,
    PRIMARY KEY(workspace_id, command_id),
    UNIQUE(workspace_id, idempotency_key),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE session_workspace_state (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    session_id TEXT NOT NULL,
    selected_task_id TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, session_id),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, selected_task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE conversation_attempts (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    attempt_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    source_message_id TEXT NOT NULL,
    provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude', 'agy')),
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled')),
    outcome_json TEXT CHECK (outcome_json IS NULL OR json_valid(outcome_json)),
    error_redacted TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    PRIMARY KEY(workspace_id, attempt_id),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, source_message_id)
        REFERENCES conversation_messages(workspace_id, message_id) ON DELETE RESTRICT,
    CHECK (
        (status = 'running' AND outcome_json IS NULL AND error_redacted IS NULL AND completed_at IS NULL)
        OR (status = 'succeeded' AND outcome_json IS NOT NULL AND error_redacted IS NULL AND completed_at IS NOT NULL)
        OR (status IN ('failed', 'cancelled') AND outcome_json IS NULL AND error_redacted IS NOT NULL AND completed_at IS NOT NULL)
    )
) STRICT;

CREATE TABLE requirement_revisions (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    requirement_revision_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    source_message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    schema_version TEXT NOT NULL,
    snapshot_hash TEXT NOT NULL CHECK (length(snapshot_hash) = 64),
    snapshot_json TEXT NOT NULL CHECK (json_valid(snapshot_json)),
    complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
    created_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, requirement_revision_id),
    UNIQUE(workspace_id, session_id, ordinal),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, source_message_id)
        REFERENCES conversation_messages(workspace_id, message_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE session_requirement_heads (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    session_id TEXT NOT NULL,
    requirement_revision_id TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, session_id),
    UNIQUE(workspace_id, requirement_revision_id),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, requirement_revision_id)
        REFERENCES requirement_revisions(workspace_id, requirement_revision_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE graph_revisions (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    revision_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    goal_message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    status TEXT NOT NULL CHECK (status IN (
        'planning', 'invalid', 'awaiting_approval', 'approved', 'superseded', 'cancelled'
    )),
    proposal_hash TEXT CHECK (proposal_hash IS NULL OR length(proposal_hash) = 64),
    proposal_json TEXT CHECK (proposal_json IS NULL OR json_valid(proposal_json)),
    validation_json TEXT NOT NULL CHECK (json_valid(validation_json)),
    planner_provider TEXT CHECK (planner_provider IS NULL OR planner_provider IN ('gemini', 'codex', 'claude')),
    created_at TEXT NOT NULL,
    completed_at TEXT,
    requirement_revision_id TEXT,
    validation_hash TEXT CHECK (validation_hash IS NULL OR length(validation_hash) = 64),
    base_commit TEXT CHECK (base_commit IS NULL OR length(base_commit) BETWEEN 40 AND 64),
    planner_provider_v2 TEXT CHECK (
        planner_provider_v2 IS NULL OR planner_provider_v2 IN ('gemini', 'codex', 'claude', 'agy')
    ),
    PRIMARY KEY(workspace_id, revision_id),
    UNIQUE(workspace_id, session_id, ordinal),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, goal_message_id)
        REFERENCES conversation_messages(workspace_id, message_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, requirement_revision_id)
        REFERENCES requirement_revisions(workspace_id, requirement_revision_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE planning_attempts (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    attempt_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    goal_message_id TEXT NOT NULL,
    planner_provider TEXT NOT NULL CHECK (planner_provider IN ('gemini', 'codex', 'claude')),
    outcome TEXT NOT NULL CHECK (outcome IN (
        'planning', 'invalid', 'awaiting_approval', 'failed', 'cancelled'
    )),
    error_redacted TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    planner_provider_v2 TEXT CHECK (
        planner_provider_v2 IS NULL OR planner_provider_v2 IN ('gemini', 'codex', 'claude', 'agy')
    ),
    PRIMARY KEY(workspace_id, attempt_id),
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, goal_message_id)
        REFERENCES conversation_messages(workspace_id, message_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE session_tasks (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    session_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    node_key TEXT NOT NULL CHECK (length(trim(node_key)) > 0),
    display_order INTEGER NOT NULL CHECK (display_order > 0),
    provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude')),
    model_profile TEXT NOT NULL CHECK (model_profile IN ('economy', 'standard', 'premium')),
    provider_id_v2 TEXT CHECK (
        provider_id_v2 IS NULL OR provider_id_v2 IN ('gemini', 'codex', 'claude', 'agy')
    ),
    PRIMARY KEY(workspace_id, session_id, revision_id, node_key),
    UNIQUE(workspace_id, task_id),
    UNIQUE(workspace_id, session_id, revision_id, display_order),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE task_dependencies (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    session_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    depends_on_task_id TEXT NOT NULL,
    PRIMARY KEY(workspace_id, task_id, depends_on_task_id),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, depends_on_task_id)
        REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    CHECK(task_id <> depends_on_task_id)
) STRICT;

CREATE TABLE session_graph_heads (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    session_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    updated_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, session_id),
    UNIQUE(workspace_id, revision_id),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE graph_approvals (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    revision_id TEXT NOT NULL,
    proposal_hash TEXT NOT NULL CHECK (length(proposal_hash) = 64),
    approved_by TEXT NOT NULL CHECK (length(trim(approved_by)) > 0),
    approved_at TEXT NOT NULL,
    session_id TEXT,
    requirement_revision_id TEXT,
    validation_hash TEXT CHECK (validation_hash IS NULL OR length(validation_hash) = 64),
    base_commit TEXT CHECK (base_commit IS NULL OR length(base_commit) BETWEEN 40 AND 64),
    PRIMARY KEY(workspace_id, revision_id),
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, requirement_revision_id)
        REFERENCES requirement_revisions(workspace_id, requirement_revision_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE task_schedule_claims (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    schedule_claim_id TEXT NOT NULL,
    daemon_instance_id TEXT NOT NULL REFERENCES daemon_instances(instance_id) ON DELETE RESTRICT,
    session_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude')),
    acquired_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT,
    release_reason TEXT,
    PRIMARY KEY(workspace_id, schedule_claim_id),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    CHECK(expires_at >= acquired_at),
    CHECK((released_at IS NULL AND release_reason IS NULL) OR
          (released_at IS NOT NULL AND length(trim(release_reason)) > 0))
) STRICT;

CREATE TABLE resource_claims (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    resource_claim_id TEXT NOT NULL,
    schedule_claim_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    path TEXT,
    repository_wide INTEGER NOT NULL CHECK (repository_wide IN (0, 1)),
    acquired_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT,
    release_reason TEXT,
    PRIMARY KEY(workspace_id, resource_claim_id),
    FOREIGN KEY(workspace_id, schedule_claim_id)
        REFERENCES task_schedule_claims(workspace_id, schedule_claim_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    CHECK((repository_wide = 1 AND path IS NULL) OR
          (repository_wide = 0 AND path IS NOT NULL AND length(trim(path)) > 0)),
    CHECK(expires_at >= acquired_at),
    CHECK((released_at IS NULL AND release_reason IS NULL) OR
          (released_at IS NOT NULL AND length(trim(release_reason)) > 0))
) STRICT;

CREATE TABLE task_instructions (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    instruction_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    message_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    state TEXT NOT NULL CHECK (state IN (
        'queued', 'applying', 'applied', 'rejected', 'interrupted'
    )),
    content_redacted TEXT NOT NULL CHECK (length(trim(content_redacted)) > 0),
    queued_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome_redacted TEXT,
    PRIMARY KEY(workspace_id, instruction_id),
    UNIQUE(workspace_id, message_id),
    UNIQUE(workspace_id, task_id, ordinal),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, message_id)
        REFERENCES conversation_messages(workspace_id, message_id) ON DELETE RESTRICT,
    CHECK(
        (state = 'queued' AND claimed_at IS NULL AND completed_at IS NULL AND outcome_redacted IS NULL) OR
        (state = 'applying' AND claimed_at IS NOT NULL AND completed_at IS NULL AND outcome_redacted IS NULL) OR
        (state IN ('applied', 'rejected', 'interrupted') AND claimed_at IS NOT NULL AND completed_at IS NOT NULL)
    )
) STRICT;

CREATE TABLE integration_batches (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    batch_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    revision_id TEXT NOT NULL,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    status TEXT NOT NULL CHECK (status IN (
        'preview', 'blocked', 'approved', 'applying', 'applied',
        'needs_attention', 'superseded'
    )),
    base_revision TEXT NOT NULL CHECK (length(base_revision) BETWEEN 40 AND 64),
    preview_hash TEXT NOT NULL CHECK (length(preview_hash) = 64),
    preview_json TEXT NOT NULL CHECK (json_valid(preview_json)),
    created_at TEXT NOT NULL,
    completed_at TEXT,
    PRIMARY KEY(workspace_id, batch_id),
    UNIQUE(workspace_id, session_id, ordinal),
    FOREIGN KEY(workspace_id, session_id) REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, revision_id)
        REFERENCES graph_revisions(workspace_id, revision_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE integration_sources (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    batch_id TEXT NOT NULL,
    source_order INTEGER NOT NULL CHECK (source_order > 0),
    task_id TEXT NOT NULL,
    checkpoint_id TEXT NOT NULL,
    verification_id TEXT NOT NULL,
    diff_sha256 TEXT NOT NULL CHECK (length(diff_sha256) = 64),
    source_json TEXT NOT NULL CHECK (json_valid(source_json)),
    PRIMARY KEY(workspace_id, batch_id, source_order),
    UNIQUE(workspace_id, batch_id, task_id),
    FOREIGN KEY(workspace_id, batch_id)
        REFERENCES integration_batches(workspace_id, batch_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, checkpoint_id)
        REFERENCES checkpoints(workspace_id, checkpoint_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, verification_id)
        REFERENCES verification_results(workspace_id, verification_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE integration_approvals (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    batch_id TEXT NOT NULL,
    preview_hash TEXT NOT NULL CHECK (length(preview_hash) = 64),
    approved_by TEXT NOT NULL CHECK (length(trim(approved_by)) > 0),
    approved_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, batch_id),
    FOREIGN KEY(workspace_id, batch_id)
        REFERENCES integration_batches(workspace_id, batch_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE integration_applications (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    application_id TEXT NOT NULL,
    batch_id TEXT NOT NULL,
    preview_hash TEXT NOT NULL CHECK (length(preview_hash) = 64),
    state TEXT NOT NULL CHECK (state IN ('applying', 'applied', 'failed', 'interrupted')),
    worktree_path TEXT NOT NULL CHECK (length(trim(worktree_path)) > 0),
    branch_name TEXT NOT NULL CHECK (length(trim(branch_name)) > 0),
    resulting_tree TEXT,
    detail_redacted TEXT NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    PRIMARY KEY(workspace_id, application_id),
    UNIQUE(workspace_id, batch_id),
    FOREIGN KEY(workspace_id, batch_id)
        REFERENCES integration_batches(workspace_id, batch_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE integration_resolution_tasks (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    batch_id TEXT NOT NULL,
    task_id TEXT NOT NULL,
    created_by TEXT NOT NULL CHECK (length(trim(created_by)) > 0),
    created_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, batch_id),
    UNIQUE(workspace_id, task_id),
    FOREIGN KEY(workspace_id, batch_id)
        REFERENCES integration_batches(workspace_id, batch_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, task_id) REFERENCES tasks(workspace_id, task_id) ON DELETE RESTRICT
) STRICT;

CREATE TABLE compatibility_runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL,
    detected_version TEXT,
    classification TEXT NOT NULL,
    capabilities_json TEXT NOT NULL CHECK (json_valid(capabilities_json)),
    fixture_fingerprint TEXT,
    report_artifact_id TEXT REFERENCES global_artifacts(artifact_id) ON DELETE RESTRICT,
    checked_at TEXT NOT NULL
) STRICT;

INSERT INTO tasks
SELECT '00000000-0000-0000-0000-000000000001', task_id, schema_version, revision,
       state, resume_state, paused, objective, original_request_redacted,
       task_envelope_json, created_at, updated_at, archived_at
FROM tasks_v12;

INSERT INTO task_attempts
SELECT '00000000-0000-0000-0000-000000000001', attempt_id, task_id, ordinal,
       provider_id, worker_mode, started_at, ended_at, outcome, worker_result_json
FROM task_attempts_v12;

INSERT INTO global_provider_usage_snapshots
SELECT snapshot_id, provider_id, quota_scope, quota_period, usage_unit, used, quota_limit,
       remaining, used_percent, remaining_percent, period_started_at, resets_at, source,
       confidence, snapshot_json, collected_at
FROM provider_usage_snapshots_v12
WHERE task_id IS NULL;

INSERT INTO provider_usage_snapshots
SELECT '00000000-0000-0000-0000-000000000001', snapshots.snapshot_id,
       snapshots.task_id, snapshots.provider_id,
       snapshots.quota_scope, snapshots.quota_period, snapshots.usage_unit, snapshots.used,
       snapshots.quota_limit, snapshots.remaining, snapshots.used_percent,
       snapshots.remaining_percent, snapshots.period_started_at, snapshots.resets_at,
       snapshots.source, snapshots.confidence, snapshots.snapshot_json, snapshots.collected_at
FROM provider_usage_snapshots_v12 snapshots
WHERE snapshots.task_id IS NOT NULL
   OR EXISTS(
       SELECT 1 FROM routing_decision_usage_v12 links
       WHERE links.snapshot_id = snapshots.snapshot_id
   );

INSERT INTO routing_decisions
SELECT '00000000-0000-0000-0000-000000000001', decision_id, task_id,
       selected_provider, model_profile, effort, difficulty, risk_json, candidates_json,
       policy_json, downgraded, rationale_json, schema_version, decided_at
FROM routing_decisions_v12;

INSERT INTO routing_decision_usage
SELECT '00000000-0000-0000-0000-000000000001', decision_id, snapshot_id
FROM routing_decision_usage_v12;

INSERT OR IGNORE INTO global_artifacts
SELECT artifact_id, kind, relative_path, sha256, byte_length, media_type, created_at
FROM artifacts_v12 artifacts
WHERE EXISTS(SELECT 1 FROM compatibility_runs_v12 runs
             WHERE runs.report_artifact_id = artifacts.artifact_id)
   OR (task_id IS NULL
       AND NOT EXISTS(SELECT 1 FROM command_evidence_v12 evidence
                      WHERE evidence.stdout_artifact_id = artifacts.artifact_id
                         OR evidence.stderr_artifact_id = artifacts.artifact_id)
       AND NOT EXISTS(SELECT 1 FROM checkpoints_v12 checkpoints
                      WHERE checkpoints.diff_artifact_id = artifacts.artifact_id));

INSERT OR IGNORE INTO artifacts
SELECT '00000000-0000-0000-0000-000000000001', artifacts.artifact_id,
       coalesce(artifacts.task_id, evidence.task_id, checkpoints.task_id),
       artifacts.kind, artifacts.relative_path, artifacts.sha256, artifacts.byte_length,
       artifacts.media_type, artifacts.created_at
FROM artifacts_v12 artifacts
LEFT JOIN command_evidence_v12 evidence
       ON evidence.stdout_artifact_id = artifacts.artifact_id
       OR evidence.stderr_artifact_id = artifacts.artifact_id
LEFT JOIN checkpoints_v12 checkpoints ON checkpoints.diff_artifact_id = artifacts.artifact_id
WHERE artifacts.task_id IS NOT NULL
   OR evidence.command_id IS NOT NULL
   OR checkpoints.checkpoint_id IS NOT NULL;

INSERT INTO command_evidence
SELECT '00000000-0000-0000-0000-000000000001', command_id, task_id, attempt_id,
       executable, args_json, working_directory, exit_code, termination,
       stdout_artifact_id, stderr_artifact_id, stdout_truncated, stderr_truncated,
       invalid_utf8, started_at, ended_at
FROM command_evidence_v12;

INSERT INTO checkpoints
SELECT '00000000-0000-0000-0000-000000000001', checkpoint_id, task_id, attempt_id,
       schema_version, checkpoint_json, integrity_hash, diff_artifact_id, git_head, created_at
FROM checkpoints_v12;

INSERT INTO handovers
SELECT '00000000-0000-0000-0000-000000000001', handover_id, task_id, checkpoint_id,
       schema_version, from_provider, to_provider, reason, bundle_json, integrity_hash,
       acknowledgement_json, started_at, completed_at
FROM handovers_v12;

INSERT INTO verification_results
SELECT '00000000-0000-0000-0000-000000000001', verification_id, task_id, attempt_id,
       reviewer_provider, outcome, schema_version, result_json, started_at, completed_at
FROM verification_results_v12;

INSERT INTO task_controls
SELECT '00000000-0000-0000-0000-000000000001', control_id, task_id, action,
       payload_json, requested_by, requested_at, claimed_at, completed_at, outcome
FROM task_controls_v12;

INSERT INTO worktrees
SELECT '00000000-0000-0000-0000-000000000001', worktree_id, task_id, repo_root,
       worktree_path, branch_name, base_revision, state, created_at, cleanup_approved_at,
       archived_at
FROM worktrees_v12;

INSERT INTO coordinator_leases
SELECT '00000000-0000-0000-0000-000000000001', lease_id, task_id, worktree_id,
       owner_id, acquired_at, renewed_at, expires_at, released_at
FROM coordinator_leases_v12;

INSERT INTO worker_leases
SELECT '00000000-0000-0000-0000-000000000001', lease_id, task_id, worktree_id,
       coordinator_lease_id, provider_id, mode, acquired_at, expires_at, released_at
FROM worker_leases_v12;

INSERT INTO changed_files
SELECT '00000000-0000-0000-0000-000000000001', task_id, worktree_id, relative_path,
       owner_lease_id, sha256, first_seen_at, last_seen_at
FROM changed_files_v12;

INSERT INTO approval_records
SELECT '00000000-0000-0000-0000-000000000001', approval_id, task_id, action,
       scope_json, approved_by, approved_at, expires_at, revoked_at
FROM approval_records_v12;

INSERT INTO sessions
SELECT '00000000-0000-0000-0000-000000000001', session_id, schema_version, revision,
       title, state, created_at, updated_at, archived_at, state_v2
FROM sessions_v12;

INSERT INTO conversation_messages
SELECT '00000000-0000-0000-0000-000000000001', message_id, session_id, task_id,
       ordinal, role, kind, state, content_redacted, created_at, finalized_at
FROM conversation_messages_v12;

INSERT INTO client_commands
SELECT '00000000-0000-0000-0000-000000000001', command_id, session_id, task_id,
       action, payload_json, idempotency_key, state, requested_by, requested_at,
       claimed_at, completed_at, outcome
FROM client_commands_v12;

INSERT INTO session_workspace_state
SELECT '00000000-0000-0000-0000-000000000001', session_id, selected_task_id, updated_at
FROM session_workspace_state_v12;

INSERT INTO conversation_attempts
SELECT '00000000-0000-0000-0000-000000000001', attempt_id, session_id,
       source_message_id, provider_id, status, outcome_json, error_redacted, started_at,
       completed_at
FROM conversation_attempts_v12;

INSERT INTO requirement_revisions
SELECT '00000000-0000-0000-0000-000000000001', requirement_revision_id, session_id,
       source_message_id, ordinal, schema_version, snapshot_hash, snapshot_json, complete,
       created_at
FROM requirement_revisions_v12;

INSERT INTO session_requirement_heads
SELECT '00000000-0000-0000-0000-000000000001', session_id,
       requirement_revision_id, updated_at
FROM session_requirement_heads_v12;

INSERT INTO graph_revisions
SELECT '00000000-0000-0000-0000-000000000001', revision_id, session_id,
       goal_message_id, ordinal, status, proposal_hash, proposal_json, validation_json,
       planner_provider, created_at, completed_at, requirement_revision_id,
       validation_hash, base_commit, planner_provider_v2
FROM graph_revisions_v12;

INSERT INTO planning_attempts
SELECT '00000000-0000-0000-0000-000000000001', attempt_id, revision_id, session_id,
       goal_message_id, planner_provider, outcome, error_redacted, started_at,
       completed_at, planner_provider_v2
FROM planning_attempts_v12;

INSERT INTO session_tasks
SELECT '00000000-0000-0000-0000-000000000001', session_id, revision_id, task_id,
       node_key, display_order, provider_id, model_profile, provider_id_v2
FROM session_tasks_v12;

INSERT INTO task_dependencies
SELECT '00000000-0000-0000-0000-000000000001', session_id, revision_id, task_id,
       depends_on_task_id
FROM task_dependencies_v12;

INSERT INTO session_graph_heads
SELECT '00000000-0000-0000-0000-000000000001', session_id, revision_id, updated_at
FROM session_graph_heads_v12;

INSERT INTO graph_approvals
SELECT '00000000-0000-0000-0000-000000000001', revision_id, proposal_hash,
       approved_by, approved_at, session_id, requirement_revision_id, validation_hash,
       base_commit
FROM graph_approvals_v12;

INSERT INTO task_schedule_claims
SELECT '00000000-0000-0000-0000-000000000001', schedule_claim_id,
       daemon_instance_id, session_id, revision_id, task_id, provider_id, acquired_at,
       expires_at, released_at, release_reason
FROM task_schedule_claims_v12;

INSERT INTO resource_claims
SELECT '00000000-0000-0000-0000-000000000001', resource_claim_id,
       schedule_claim_id, session_id, revision_id, task_id, path, repository_wide,
       acquired_at, expires_at, released_at, release_reason
FROM resource_claims_v12;

INSERT INTO task_instructions
SELECT '00000000-0000-0000-0000-000000000001', instruction_id, session_id, task_id,
       message_id, ordinal, state, content_redacted, queued_at, claimed_at, completed_at,
       outcome_redacted
FROM task_instructions_v12;

INSERT INTO integration_batches
SELECT '00000000-0000-0000-0000-000000000001', batch_id, session_id, revision_id,
       ordinal, status, base_revision, preview_hash, preview_json, created_at, completed_at
FROM integration_batches_v12;

INSERT INTO integration_sources
SELECT '00000000-0000-0000-0000-000000000001', batch_id, source_order, task_id,
       checkpoint_id, verification_id, diff_sha256, source_json
FROM integration_sources_v12;

INSERT INTO integration_approvals
SELECT '00000000-0000-0000-0000-000000000001', batch_id, preview_hash,
       approved_by, approved_at
FROM integration_approvals_v12;

INSERT INTO integration_applications
SELECT '00000000-0000-0000-0000-000000000001', application_id, batch_id,
       preview_hash, state, worktree_path, branch_name, resulting_tree, detail_redacted,
       started_at, completed_at
FROM integration_applications_v12;

INSERT INTO integration_resolution_tasks
SELECT '00000000-0000-0000-0000-000000000001', batch_id, task_id, created_by, created_at
FROM integration_resolution_tasks_v12;

INSERT INTO task_events
SELECT '00000000-0000-0000-0000-000000000001', sequence, event_id, task_id,
       event_type, schema_version, occurred_at, event_json, previous_hash, event_hash,
       exported_at, session_id
FROM task_events_v12;

INSERT INTO event_log_state
SELECT '00000000-0000-0000-0000-000000000001', last_exported_sequence,
       last_exported_hash, updated_at
FROM event_log_state_v12
WHERE EXISTS(
    SELECT 1 FROM workspaces
    WHERE workspace_id = '00000000-0000-0000-0000-000000000001'
);

INSERT INTO compatibility_runs
SELECT run_id, provider_id, detected_version, classification, capabilities_json,
       fixture_fingerprint, report_artifact_id, checked_at
FROM compatibility_runs_v12;

DROP TABLE integration_resolution_tasks_v12;
DROP TABLE integration_applications_v12;
DROP TABLE integration_approvals_v12;
DROP TABLE integration_sources_v12;
DROP TABLE integration_batches_v12;
DROP TABLE task_instructions_v12;
DROP TABLE resource_claims_v12;
DROP TABLE task_schedule_claims_v12;
DROP TABLE graph_approvals_v12;
DROP TABLE session_graph_heads_v12;
DROP TABLE task_dependencies_v12;
DROP TABLE session_tasks_v12;
DROP TABLE planning_attempts_v12;
DROP TABLE graph_revisions_v12;
DROP TABLE session_requirement_heads_v12;
DROP TABLE requirement_revisions_v12;
DROP TABLE conversation_attempts_v12;
DROP TABLE session_workspace_state_v12;
DROP TABLE client_commands_v12;
DROP TABLE conversation_messages_v12;
DROP TABLE sessions_v12;
DROP TABLE compatibility_runs_v12;
DROP TABLE approval_records_v12;
DROP TABLE changed_files_v12;
DROP TABLE worker_leases_v12;
DROP TABLE coordinator_leases_v12;
DROP TABLE worktrees_v12;
DROP TABLE task_controls_v12;
DROP TABLE event_log_state_v12;
DROP TABLE task_events_v12;
DROP TABLE verification_results_v12;
DROP TABLE handovers_v12;
DROP TABLE checkpoints_v12;
DROP TABLE command_evidence_v12;
DROP TABLE artifacts_v12;
DROP TABLE routing_decision_usage_v12;
DROP TABLE routing_decisions_v12;
DROP TABLE provider_usage_snapshots_v12;
DROP TABLE task_attempts_v12;
DROP TABLE tasks_v12;

CREATE INDEX provider_usage_provider_time
    ON provider_usage_snapshots(workspace_id, provider_id, collected_at DESC);
CREATE INDEX global_provider_usage_provider_time
    ON global_provider_usage_snapshots(provider_id, collected_at DESC);
CREATE INDEX routing_decisions_task_time
    ON routing_decisions(workspace_id, task_id, decided_at DESC);
CREATE INDEX checkpoints_task_time
    ON checkpoints(workspace_id, task_id, created_at DESC);
CREATE INDEX handovers_task_time
    ON handovers(workspace_id, task_id, started_at DESC);
CREATE INDEX verification_task_time
    ON verification_results(workspace_id, task_id, completed_at DESC);
CREATE INDEX task_events_task_sequence
    ON task_events(workspace_id, task_id, sequence);
CREATE INDEX task_events_session_sequence
    ON task_events(workspace_id, session_id, sequence);
CREATE INDEX task_controls_pending
    ON task_controls(workspace_id, task_id, requested_at) WHERE claimed_at IS NULL;
CREATE UNIQUE INDEX one_active_coordinator_lease_per_task
    ON coordinator_leases(workspace_id, task_id) WHERE released_at IS NULL;
CREATE UNIQUE INDEX one_active_writable_lease_per_task
    ON worker_leases(workspace_id, task_id)
    WHERE mode = 'writable' AND released_at IS NULL;
CREATE INDEX worker_leases_expiry
    ON worker_leases(workspace_id, expires_at) WHERE released_at IS NULL;
CREATE INDEX coordinator_leases_expiry
    ON coordinator_leases(workspace_id, expires_at) WHERE released_at IS NULL;
CREATE INDEX worker_leases_coordinator
    ON worker_leases(workspace_id, coordinator_lease_id) WHERE released_at IS NULL;
CREATE INDEX conversation_messages_session_ordinal
    ON conversation_messages(workspace_id, session_id, ordinal);
CREATE INDEX client_commands_pending
    ON client_commands(workspace_id, requested_at) WHERE state = 'pending';
CREATE INDEX graph_revisions_session_ordinal
    ON graph_revisions(workspace_id, session_id, ordinal DESC);
CREATE INDEX planning_attempts_session_time
    ON planning_attempts(workspace_id, session_id, started_at DESC);
CREATE INDEX session_tasks_session_order
    ON session_tasks(workspace_id, session_id, revision_id, display_order);
CREATE INDEX task_dependencies_revision
    ON task_dependencies(workspace_id, session_id, revision_id);
CREATE UNIQUE INDEX task_schedule_claims_one_active_task
    ON task_schedule_claims(workspace_id, task_id) WHERE released_at IS NULL;
CREATE INDEX task_schedule_claims_active_provider
    ON task_schedule_claims(workspace_id, provider_id, expires_at) WHERE released_at IS NULL;
CREATE INDEX task_schedule_claims_active_daemon
    ON task_schedule_claims(daemon_instance_id, workspace_id, expires_at) WHERE released_at IS NULL;
CREATE INDEX resource_claims_active
    ON resource_claims(workspace_id, expires_at) WHERE released_at IS NULL;
CREATE INDEX resource_claims_task
    ON resource_claims(workspace_id, task_id, acquired_at DESC);
CREATE INDEX task_instructions_pending
    ON task_instructions(workspace_id, task_id, ordinal) WHERE state IN ('queued', 'interrupted');
CREATE INDEX integration_batches_session_ordinal
    ON integration_batches(workspace_id, session_id, ordinal DESC);
CREATE INDEX integration_sources_task
    ON integration_sources(workspace_id, task_id);
CREATE INDEX conversation_attempts_session_time
    ON conversation_attempts(workspace_id, session_id, started_at DESC);
CREATE INDEX requirement_revisions_session_ordinal
    ON requirement_revisions(workspace_id, session_id, ordinal DESC);

CREATE TRIGGER sessions_legacy_state_sync
AFTER UPDATE OF state ON sessions
WHEN NEW.state_v2 IS OLD.state_v2
BEGIN
    UPDATE sessions SET state_v2 = NEW.state
    WHERE workspace_id = NEW.workspace_id AND session_id = NEW.session_id;
END;

CREATE TRIGGER graph_revisions_immutable_payload
BEFORE UPDATE OF workspace_id, session_id, goal_message_id, ordinal, proposal_hash,
                 proposal_json, validation_json, planner_provider, created_at ON graph_revisions
WHEN OLD.status <> 'planning'
BEGIN SELECT RAISE(ABORT, 'graph revision payload is immutable'); END;
CREATE TRIGGER graph_revisions_no_delete BEFORE DELETE ON graph_revisions
BEGIN SELECT RAISE(ABORT, 'graph revisions are append-only'); END;
CREATE TRIGGER graph_revision_authority_immutable
BEFORE UPDATE OF requirement_revision_id, validation_hash, base_commit ON graph_revisions
WHEN OLD.status <> 'planning'
BEGIN SELECT RAISE(ABORT, 'graph validation authority is immutable'); END;
CREATE TRIGGER graph_revision_provider_v2_immutable
BEFORE UPDATE OF planner_provider_v2 ON graph_revisions
WHEN OLD.status <> 'planning'
BEGIN SELECT RAISE(ABORT, 'graph planner provider is immutable'); END;

CREATE TRIGGER planning_attempts_immutable_identity
BEFORE UPDATE OF workspace_id, attempt_id, revision_id, session_id, goal_message_id,
                 planner_provider, started_at ON planning_attempts
BEGIN SELECT RAISE(ABORT, 'planning attempt identity is immutable'); END;
CREATE TRIGGER planning_attempts_single_completion
BEFORE UPDATE OF outcome, error_redacted, completed_at ON planning_attempts
WHEN OLD.outcome <> 'planning' OR NEW.outcome = 'planning' OR NEW.completed_at IS NULL
BEGIN SELECT RAISE(ABORT, 'planning attempt completion is append-once'); END;
CREATE TRIGGER planning_attempts_no_delete BEFORE DELETE ON planning_attempts
BEGIN SELECT RAISE(ABORT, 'planning attempts are append-only'); END;
CREATE TRIGGER planning_attempt_provider_v2_immutable
BEFORE UPDATE OF planner_provider_v2 ON planning_attempts
BEGIN SELECT RAISE(ABORT, 'planning attempt provider is immutable'); END;

CREATE TRIGGER graph_approvals_no_update BEFORE UPDATE ON graph_approvals
BEGIN SELECT RAISE(ABORT, 'graph approvals are immutable'); END;
CREATE TRIGGER graph_approvals_no_delete BEFORE DELETE ON graph_approvals
BEGIN SELECT RAISE(ABORT, 'graph approvals are append-only'); END;

CREATE TRIGGER task_instructions_identity_immutable
BEFORE UPDATE OF workspace_id, instruction_id, session_id, task_id, message_id, ordinal,
                 content_redacted, queued_at ON task_instructions
BEGIN SELECT RAISE(ABORT, 'task instruction identity is immutable'); END;
CREATE TRIGGER task_instructions_transition_guard
BEFORE UPDATE OF state ON task_instructions
WHEN NOT (
    (OLD.state IN ('queued', 'interrupted') AND NEW.state IN ('applying', 'rejected')) OR
    (OLD.state = 'applying' AND NEW.state IN ('applied', 'rejected', 'interrupted'))
)
BEGIN SELECT RAISE(ABORT, 'invalid task instruction transition'); END;

CREATE TRIGGER integration_batches_payload_immutable
BEFORE UPDATE OF workspace_id, batch_id, session_id, revision_id, ordinal, base_revision,
                 preview_hash, preview_json, created_at ON integration_batches
BEGIN SELECT RAISE(ABORT, 'integration preview payload is immutable'); END;
CREATE TRIGGER integration_batches_no_delete BEFORE DELETE ON integration_batches
BEGIN SELECT RAISE(ABORT, 'integration batches are append-only'); END;
CREATE TRIGGER integration_sources_no_update BEFORE UPDATE ON integration_sources
BEGIN SELECT RAISE(ABORT, 'integration sources are immutable'); END;
CREATE TRIGGER integration_sources_no_delete BEFORE DELETE ON integration_sources
BEGIN SELECT RAISE(ABORT, 'integration sources are append-only'); END;
CREATE TRIGGER integration_approvals_no_update BEFORE UPDATE ON integration_approvals
BEGIN SELECT RAISE(ABORT, 'integration approvals are immutable'); END;
CREATE TRIGGER integration_approvals_no_delete BEFORE DELETE ON integration_approvals
BEGIN SELECT RAISE(ABORT, 'integration approvals are append-only'); END;

CREATE TRIGGER conversation_attempts_immutable_identity
BEFORE UPDATE OF workspace_id, attempt_id, session_id, source_message_id, provider_id, started_at
ON conversation_attempts
BEGIN SELECT RAISE(ABORT, 'conversation attempt identity is immutable'); END;
CREATE TRIGGER conversation_attempts_single_completion
BEFORE UPDATE OF status, outcome_json, error_redacted, completed_at ON conversation_attempts
WHEN OLD.status <> 'running' OR NEW.status = 'running' OR NEW.completed_at IS NULL
BEGIN SELECT RAISE(ABORT, 'conversation attempt completion is append-once'); END;
CREATE TRIGGER conversation_attempts_no_delete BEFORE DELETE ON conversation_attempts
BEGIN SELECT RAISE(ABORT, 'conversation attempts are append-only'); END;
CREATE TRIGGER requirement_revisions_no_update BEFORE UPDATE ON requirement_revisions
BEGIN SELECT RAISE(ABORT, 'requirement revisions are immutable'); END;
CREATE TRIGGER requirement_revisions_no_delete BEFORE DELETE ON requirement_revisions
BEGIN SELECT RAISE(ABORT, 'requirement revisions are append-only'); END;

PRAGMA user_version = 13;
