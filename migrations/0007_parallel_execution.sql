CREATE TABLE task_schedule_claims (
    schedule_claim_id TEXT PRIMARY KEY NOT NULL,
    daemon_instance_id TEXT NOT NULL REFERENCES daemon_instances(instance_id) ON DELETE RESTRICT,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    revision_id TEXT NOT NULL REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude')),
    acquired_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT,
    release_reason TEXT,
    CHECK(expires_at >= acquired_at),
    CHECK((released_at IS NULL AND release_reason IS NULL) OR
          (released_at IS NOT NULL AND length(trim(release_reason)) > 0))
) STRICT;

CREATE UNIQUE INDEX task_schedule_claims_one_active_task
    ON task_schedule_claims(task_id) WHERE released_at IS NULL;
CREATE INDEX task_schedule_claims_active_provider
    ON task_schedule_claims(provider_id, expires_at) WHERE released_at IS NULL;
CREATE INDEX task_schedule_claims_active_daemon
    ON task_schedule_claims(daemon_instance_id, expires_at) WHERE released_at IS NULL;

CREATE TABLE resource_claims (
    resource_claim_id TEXT PRIMARY KEY NOT NULL,
    schedule_claim_id TEXT NOT NULL REFERENCES task_schedule_claims(schedule_claim_id) ON DELETE RESTRICT,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    revision_id TEXT NOT NULL REFERENCES graph_revisions(revision_id) ON DELETE RESTRICT,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    path TEXT,
    repository_wide INTEGER NOT NULL CHECK (repository_wide IN (0, 1)),
    acquired_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT,
    release_reason TEXT,
    CHECK((repository_wide = 1 AND path IS NULL) OR
          (repository_wide = 0 AND path IS NOT NULL AND length(trim(path)) > 0)),
    CHECK(expires_at >= acquired_at),
    CHECK((released_at IS NULL AND release_reason IS NULL) OR
          (released_at IS NOT NULL AND length(trim(release_reason)) > 0))
) STRICT;

CREATE INDEX resource_claims_active
    ON resource_claims(expires_at) WHERE released_at IS NULL;
CREATE INDEX resource_claims_task
    ON resource_claims(task_id, acquired_at DESC);

CREATE TABLE task_instructions (
    instruction_id TEXT PRIMARY KEY NOT NULL,
    session_id TEXT NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    message_id TEXT NOT NULL UNIQUE REFERENCES conversation_messages(message_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    state TEXT NOT NULL CHECK (state IN (
        'queued', 'applying', 'applied', 'rejected', 'interrupted'
    )),
    content_redacted TEXT NOT NULL CHECK (length(trim(content_redacted)) > 0),
    queued_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome_redacted TEXT,
    UNIQUE(task_id, ordinal),
    CHECK(
        (state = 'queued' AND claimed_at IS NULL AND completed_at IS NULL AND outcome_redacted IS NULL) OR
        (state = 'applying' AND claimed_at IS NOT NULL AND completed_at IS NULL AND outcome_redacted IS NULL) OR
        (state IN ('applied', 'rejected', 'interrupted') AND claimed_at IS NOT NULL AND completed_at IS NOT NULL)
    )
) STRICT;

CREATE INDEX task_instructions_pending
    ON task_instructions(task_id, ordinal) WHERE state IN ('queued', 'interrupted');

CREATE TRIGGER task_instructions_identity_immutable
BEFORE UPDATE OF instruction_id, session_id, task_id, message_id, ordinal,
                 content_redacted, queued_at ON task_instructions
BEGIN
    SELECT RAISE(ABORT, 'task instruction identity is immutable');
END;

CREATE TRIGGER task_instructions_transition_guard
BEFORE UPDATE OF state ON task_instructions
WHEN NOT (
    (OLD.state IN ('queued', 'interrupted') AND NEW.state IN ('applying', 'rejected')) OR
    (OLD.state = 'applying' AND NEW.state IN ('applied', 'rejected', 'interrupted'))
)
BEGIN
    SELECT RAISE(ABORT, 'invalid task instruction transition');
END;

PRAGMA user_version = 7;
