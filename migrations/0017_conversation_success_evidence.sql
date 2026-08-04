PRAGMA defer_foreign_keys = ON;

DROP TRIGGER conversation_attempts_immutable_identity;
DROP TRIGGER conversation_attempts_single_completion;
DROP TRIGGER conversation_attempts_no_delete;
DROP INDEX conversation_attempts_session_time;

ALTER TABLE conversation_attempts RENAME TO conversation_attempts_v16;

CREATE TABLE conversation_attempts (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace()),
    attempt_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    source_message_id TEXT NOT NULL,
    provider_id TEXT NOT NULL CHECK (provider_id IN ('gemini', 'codex', 'claude', 'agy')),
    status TEXT NOT NULL CHECK (status IN ('running', 'succeeded', 'failed', 'cancelled')),
    outcome_json TEXT CHECK (outcome_json IS NULL OR json_valid(outcome_json)),
    error_redacted TEXT,
    evidence_redacted TEXT,
    started_at TEXT NOT NULL,
    completed_at TEXT,
    PRIMARY KEY(workspace_id, attempt_id),
    FOREIGN KEY(workspace_id, session_id)
        REFERENCES sessions(workspace_id, session_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, source_message_id)
        REFERENCES conversation_messages(workspace_id, message_id) ON DELETE RESTRICT,
    CHECK (
        (status = 'running'
            AND outcome_json IS NULL
            AND error_redacted IS NULL
            AND evidence_redacted IS NULL
            AND completed_at IS NULL)
        OR (status = 'succeeded'
            AND outcome_json IS NOT NULL
            AND error_redacted IS NULL
            AND (evidence_redacted IS NULL OR (
                length(trim(
                    evidence_redacted,
                    char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                         8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202,
                         8232, 8233, 8239, 8287, 12288)
                )) > 0
                AND length(CAST(evidence_redacted AS BLOB)) <= 16384
            ))
            AND completed_at IS NOT NULL)
        OR (status IN ('failed', 'cancelled')
            AND outcome_json IS NOT NULL
            AND coalesce(
                json_type(outcome_json, '$.outcome') = 'text'
                AND json_extract(outcome_json, '$.outcome') = 'needs_attention'
                AND json_type(outcome_json, '$.response_redacted') = 'text'
                AND length(trim(
                    json_extract(outcome_json, '$.response_redacted'),
                    char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                         8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202,
                         8232, 8233, 8239, 8287, 12288)
                )) > 0
                AND json_type(outcome_json, '$.evidence_redacted') = 'text'
                AND length(trim(
                    json_extract(outcome_json, '$.evidence_redacted'),
                    char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                         8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202,
                         8232, 8233, 8239, 8287, 12288)
                )) > 0
                AND length(CAST(
                    json_extract(outcome_json, '$.evidence_redacted') AS BLOB
                )) <= 16384,
                0
            )
            AND error_redacted IS NOT NULL
            AND length(trim(
                error_redacted,
                char(9, 10, 11, 12, 13, 32, 133, 160, 5760,
                     8192, 8193, 8194, 8195, 8196, 8197, 8198, 8199, 8200, 8201, 8202,
                     8232, 8233, 8239, 8287, 12288)
            )) > 0
            AND length(CAST(error_redacted AS BLOB)) <= 16384
            AND evidence_redacted IS NULL
            AND completed_at IS NOT NULL)
    )
) STRICT;

INSERT INTO conversation_attempts(
    workspace_id, attempt_id, session_id, source_message_id, provider_id, status,
    outcome_json, error_redacted, evidence_redacted, started_at, completed_at
)
SELECT workspace_id, attempt_id, session_id, source_message_id, provider_id, status,
       outcome_json, error_redacted, NULL, started_at, completed_at
FROM conversation_attempts_v16;

DROP TABLE conversation_attempts_v16;

CREATE INDEX conversation_attempts_session_time
    ON conversation_attempts(workspace_id, session_id, started_at DESC);

CREATE TRIGGER conversation_attempts_immutable_identity
BEFORE UPDATE OF workspace_id, attempt_id, session_id, source_message_id, provider_id, started_at
ON conversation_attempts
BEGIN SELECT RAISE(ABORT, 'conversation attempt identity is immutable'); END;

CREATE TRIGGER conversation_attempts_single_completion
BEFORE UPDATE OF status, outcome_json, error_redacted, evidence_redacted, completed_at
ON conversation_attempts
WHEN OLD.status <> 'running' OR NEW.status = 'running' OR NEW.completed_at IS NULL
BEGIN SELECT RAISE(ABORT, 'conversation attempt completion is append-once'); END;

CREATE TRIGGER conversation_attempts_no_delete BEFORE DELETE ON conversation_attempts
BEGIN SELECT RAISE(ABORT, 'conversation attempts are append-only'); END;

PRAGMA user_version = 17;
