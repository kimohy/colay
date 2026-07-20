CREATE TABLE sessions (
    session_id TEXT PRIMARY KEY NOT NULL,
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
    archived_at TEXT
) STRICT;

CREATE TABLE conversation_messages (
    message_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
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
    UNIQUE(session_id, ordinal)
) STRICT;

CREATE TABLE client_commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN ('create_session', 'append_message', 'stop_daemon')),
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    idempotency_key TEXT NOT NULL UNIQUE CHECK (length(trim(idempotency_key)) > 0),
    state TEXT NOT NULL CHECK (state IN ('pending', 'claimed', 'completed', 'failed')),
    requested_by TEXT NOT NULL CHECK (length(trim(requested_by)) > 0),
    requested_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome TEXT
) STRICT;

CREATE TABLE daemon_instances (
    instance_id TEXT PRIMARY KEY NOT NULL,
    pid INTEGER NOT NULL CHECK (pid > 0),
    started_at TEXT NOT NULL,
    heartbeat_at TEXT NOT NULL,
    lease_expires_at TEXT NOT NULL,
    stop_requested_at TEXT,
    released_at TEXT
) STRICT;

CREATE UNIQUE INDEX one_unreleased_repository_daemon
    ON daemon_instances((1)) WHERE released_at IS NULL;
CREATE INDEX conversation_messages_session_ordinal
    ON conversation_messages(session_id, ordinal);
CREATE INDEX client_commands_pending
    ON client_commands(requested_at) WHERE state = 'pending';
CREATE INDEX daemon_instances_heartbeat
    ON daemon_instances(heartbeat_at DESC);

ALTER TABLE task_events
    ADD COLUMN session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT;
CREATE INDEX task_events_session_sequence ON task_events(session_id, sequence);

PRAGMA user_version = 4;
