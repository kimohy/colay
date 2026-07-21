ALTER TABLE client_commands RENAME TO client_commands_v5;

CREATE TABLE client_commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN (
        'create_session', 'append_message', 'stop_daemon',
        'request_plan', 'approve_graph', 'revise_graph', 'cancel_plan'
    )),
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    idempotency_key TEXT NOT NULL UNIQUE CHECK (length(trim(idempotency_key)) > 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'completed', 'failed')),
    requested_by TEXT NOT NULL CHECK (length(trim(requested_by)) > 0),
    requested_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome TEXT
) STRICT;

INSERT INTO client_commands(
    command_id, session_id, task_id, action, payload_json, idempotency_key, state,
    requested_by, requested_at, claimed_at, completed_at, outcome
)
SELECT command_id, session_id, task_id, action, payload_json, idempotency_key, state,
       requested_by, requested_at, claimed_at, completed_at, outcome
FROM client_commands_v5;

DROP TABLE client_commands_v5;
CREATE INDEX client_commands_pending
    ON client_commands(requested_at) WHERE state = 'pending';

CREATE TABLE graph_revisions (
    revision_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    goal_message_id TEXT NOT NULL REFERENCES conversation_messages(message_id) ON DELETE RESTRICT,
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
    UNIQUE(session_id, ordinal)
) STRICT;

CREATE TABLE planning_attempts (
    attempt_id TEXT PRIMARY KEY NOT NULL,
    revision_id TEXT NOT NULL REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    goal_message_id TEXT NOT NULL REFERENCES conversation_messages(message_id) ON DELETE RESTRICT,
    planner_provider TEXT NOT NULL CHECK (planner_provider IN ('gemini', 'codex', 'claude')),
    outcome TEXT NOT NULL CHECK (outcome IN (
        'planning', 'invalid', 'awaiting_approval', 'failed', 'cancelled'
    )),
    error_redacted TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT
) STRICT;

CREATE TABLE session_tasks (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    revision_id TEXT NOT NULL REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
    task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id) ON DELETE RESTRICT,
    node_key TEXT NOT NULL CHECK (length(trim(node_key)) > 0),
    display_order INTEGER NOT NULL CHECK (display_order > 0),
    provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude')),
    model_profile TEXT NOT NULL CHECK (model_profile IN ('economy', 'standard', 'premium')),
    PRIMARY KEY(session_id, revision_id, node_key),
    UNIQUE(session_id, revision_id, display_order)
) STRICT;

CREATE TABLE task_dependencies (
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    revision_id TEXT NOT NULL REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    depends_on_task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    PRIMARY KEY(task_id, depends_on_task_id),
    CHECK(task_id <> depends_on_task_id)
) STRICT;

CREATE TABLE session_graph_heads (
    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    revision_id TEXT NOT NULL UNIQUE REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
    updated_at TEXT NOT NULL
) STRICT;

CREATE TABLE graph_approvals (
    revision_id TEXT PRIMARY KEY NOT NULL REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
    proposal_hash TEXT NOT NULL CHECK (length(proposal_hash) = 64),
    approved_by TEXT NOT NULL CHECK (length(trim(approved_by)) > 0),
    approved_at TEXT NOT NULL
) STRICT;

CREATE INDEX graph_revisions_session_ordinal
    ON graph_revisions(session_id, ordinal DESC);
CREATE INDEX planning_attempts_session_time
    ON planning_attempts(session_id, completed_at DESC);
CREATE INDEX session_tasks_session_order
    ON session_tasks(session_id, display_order);
CREATE INDEX task_dependencies_revision
    ON task_dependencies(revision_id, task_id);

CREATE TRIGGER graph_revisions_immutable_payload
BEFORE UPDATE OF session_id, goal_message_id, ordinal, proposal_hash, proposal_json,
                 validation_json, planner_provider, created_at ON graph_revisions
WHEN OLD.status <> 'planning'
BEGIN
    SELECT RAISE(ABORT, 'graph revision payload is immutable');
END;

CREATE TRIGGER graph_revisions_no_delete
BEFORE DELETE ON graph_revisions
BEGIN
    SELECT RAISE(ABORT, 'graph revisions are append-only');
END;

CREATE TRIGGER planning_attempts_immutable_identity
BEFORE UPDATE OF attempt_id, revision_id, session_id, goal_message_id,
                 planner_provider, started_at ON planning_attempts
BEGIN
    SELECT RAISE(ABORT, 'planning attempt identity is immutable');
END;

CREATE TRIGGER planning_attempts_single_completion
BEFORE UPDATE OF outcome, error_redacted, completed_at ON planning_attempts
WHEN OLD.outcome <> 'planning' OR NEW.outcome = 'planning' OR NEW.completed_at IS NULL
BEGIN
    SELECT RAISE(ABORT, 'planning attempt completion is append-once');
END;

CREATE TRIGGER planning_attempts_no_delete
BEFORE DELETE ON planning_attempts
BEGIN
    SELECT RAISE(ABORT, 'planning attempts are append-only');
END;

CREATE TRIGGER graph_approvals_no_update
BEFORE UPDATE ON graph_approvals
BEGIN
    SELECT RAISE(ABORT, 'graph approvals are immutable');
END;

CREATE TRIGGER graph_approvals_no_delete
BEFORE DELETE ON graph_approvals
BEGIN
    SELECT RAISE(ABORT, 'graph approvals are append-only');
END;

PRAGMA user_version = 6;
