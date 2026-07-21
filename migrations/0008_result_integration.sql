ALTER TABLE client_commands RENAME TO client_commands_v7;

CREATE TABLE client_commands (
    command_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    action TEXT NOT NULL CHECK (action IN (
        'create_session', 'append_message', 'stop_daemon',
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

INSERT INTO client_commands SELECT * FROM client_commands_v7;
DROP TABLE client_commands_v7;
CREATE INDEX client_commands_pending ON client_commands(requested_at) WHERE state = 'pending';

CREATE TABLE integration_batches (
    batch_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    revision_id TEXT NOT NULL REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
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
    UNIQUE(session_id, ordinal)
) STRICT;

CREATE TABLE integration_sources (
    batch_id TEXT NOT NULL REFERENCES integration_batches(batch_id) ON DELETE RESTRICT,
    source_order INTEGER NOT NULL CHECK (source_order > 0),
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    checkpoint_id TEXT NOT NULL REFERENCES checkpoints(checkpoint_id) ON DELETE RESTRICT,
    verification_id TEXT NOT NULL REFERENCES verification_results(verification_id) ON DELETE RESTRICT,
    diff_sha256 TEXT NOT NULL CHECK (length(diff_sha256) = 64),
    source_json TEXT NOT NULL CHECK (json_valid(source_json)),
    PRIMARY KEY(batch_id, source_order),
    UNIQUE(batch_id, task_id)
) STRICT;

CREATE TABLE integration_approvals (
    batch_id TEXT PRIMARY KEY NOT NULL REFERENCES integration_batches(batch_id) ON DELETE RESTRICT,
    preview_hash TEXT NOT NULL CHECK (length(preview_hash) = 64),
    approved_by TEXT NOT NULL CHECK (length(trim(approved_by)) > 0),
    approved_at TEXT NOT NULL
) STRICT;

CREATE TABLE integration_applications (
    application_id TEXT PRIMARY KEY NOT NULL,
    batch_id TEXT NOT NULL UNIQUE REFERENCES integration_batches(batch_id) ON DELETE RESTRICT,
    preview_hash TEXT NOT NULL CHECK (length(preview_hash) = 64),
    state TEXT NOT NULL CHECK (state IN ('applying', 'applied', 'failed', 'interrupted')),
    worktree_path TEXT NOT NULL CHECK (length(trim(worktree_path)) > 0),
    branch_name TEXT NOT NULL CHECK (length(trim(branch_name)) > 0),
    resulting_tree TEXT,
    detail_redacted TEXT NOT NULL,
    started_at TEXT NOT NULL,
    completed_at TEXT
) STRICT;

CREATE TABLE integration_resolution_tasks (
    batch_id TEXT PRIMARY KEY NOT NULL REFERENCES integration_batches(batch_id) ON DELETE RESTRICT,
    task_id TEXT NOT NULL UNIQUE REFERENCES tasks(task_id) ON DELETE RESTRICT,
    created_by TEXT NOT NULL CHECK (length(trim(created_by)) > 0),
    created_at TEXT NOT NULL
) STRICT;

CREATE INDEX integration_batches_session_ordinal ON integration_batches(session_id, ordinal DESC);
CREATE INDEX integration_sources_task ON integration_sources(task_id);

CREATE TRIGGER integration_batches_payload_immutable
BEFORE UPDATE OF batch_id, session_id, revision_id, ordinal, base_revision,
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

PRAGMA user_version = 8;
