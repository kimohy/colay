ALTER TABLE client_commands RENAME TO client_commands_v9;

CREATE TABLE client_commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN (
        'create_session', 'append_message', 'request_conversation_turn', 'stop_daemon',
        'request_plan', 'approve_graph', 'revise_graph', 'cancel_plan',
        'request_integration', 'approve_integration', 'create_resolution_task'
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

INSERT INTO client_commands SELECT * FROM client_commands_v9;
DROP TABLE client_commands_v9;
CREATE INDEX client_commands_pending ON client_commands(requested_at) WHERE state = 'pending';

CREATE TABLE conversation_attempts (
    attempt_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    source_message_id TEXT NOT NULL REFERENCES conversation_messages(message_id) ON DELETE RESTRICT,
    provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude', 'agy')),
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled')),
    outcome_json TEXT CHECK (outcome_json IS NULL OR json_valid(outcome_json)),
    error_redacted TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    CHECK (
        (status = 'running' AND outcome_json IS NULL AND error_redacted IS NULL AND completed_at IS NULL)
        OR (status = 'succeeded' AND outcome_json IS NOT NULL AND error_redacted IS NULL AND completed_at IS NOT NULL)
        OR (status IN ('failed', 'cancelled') AND outcome_json IS NULL AND error_redacted IS NOT NULL AND completed_at IS NOT NULL)
    )
) STRICT;

CREATE TABLE requirement_revisions (
    requirement_revision_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    source_message_id TEXT NOT NULL REFERENCES conversation_messages(message_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    schema_version TEXT NOT NULL,
    snapshot_hash TEXT NOT NULL CHECK (length(snapshot_hash) = 64),
    snapshot_json TEXT NOT NULL CHECK (json_valid(snapshot_json)),
    complete INTEGER NOT NULL CHECK (complete IN (0, 1)),
    created_at TEXT NOT NULL,
    UNIQUE(session_id, ordinal)
) STRICT;

CREATE TABLE session_requirement_heads (
    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    requirement_revision_id TEXT NOT NULL UNIQUE REFERENCES requirement_revisions(requirement_revision_id) ON DELETE RESTRICT,
    updated_at TEXT NOT NULL
) STRICT;

ALTER TABLE graph_revisions ADD COLUMN requirement_revision_id TEXT
    REFERENCES requirement_revisions(requirement_revision_id) ON DELETE RESTRICT;
ALTER TABLE graph_revisions ADD COLUMN validation_hash TEXT
    CHECK (validation_hash IS NULL OR length(validation_hash) = 64);
ALTER TABLE graph_revisions ADD COLUMN base_commit TEXT
    CHECK (base_commit IS NULL OR length(base_commit) BETWEEN 40 AND 64);

CREATE INDEX conversation_attempts_session_time
    ON conversation_attempts(session_id, started_at DESC);
CREATE INDEX requirement_revisions_session_ordinal
    ON requirement_revisions(session_id, ordinal DESC);

CREATE TRIGGER conversation_attempts_immutable_identity
BEFORE UPDATE OF attempt_id, session_id, source_message_id, provider_id, started_at
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

CREATE TRIGGER graph_revision_authority_immutable
BEFORE UPDATE OF requirement_revision_id, validation_hash, base_commit ON graph_revisions
WHEN OLD.status <> 'planning'
BEGIN SELECT RAISE(ABORT, 'graph validation authority is immutable'); END;

PRAGMA user_version = 10;
