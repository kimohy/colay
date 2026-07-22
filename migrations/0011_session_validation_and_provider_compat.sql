ALTER TABLE sessions ADD COLUMN state_v2 TEXT CHECK (state_v2 IS NULL OR state_v2 IN (
    'drafting', 'planning', 'validating', 'awaiting_approval', 'running',
    'needs_attention', 'integrating', 'verifying', 'completed',
    'stopping', 'cancelled'
));
UPDATE sessions SET state_v2 = state;

CREATE TRIGGER sessions_legacy_state_sync
AFTER UPDATE OF state ON sessions
WHEN NEW.state_v2 IS OLD.state_v2
BEGIN
    UPDATE sessions SET state_v2 = NEW.state WHERE session_id = NEW.session_id;
END;

ALTER TABLE graph_revisions ADD COLUMN planner_provider_v2 TEXT
    CHECK (planner_provider_v2 IS NULL OR planner_provider_v2 IN ('gemini', 'codex', 'claude', 'agy'));
UPDATE graph_revisions SET planner_provider_v2 = planner_provider;
CREATE TRIGGER graph_revision_provider_v2_immutable
BEFORE UPDATE OF planner_provider_v2 ON graph_revisions
WHEN OLD.status <> 'planning'
BEGIN SELECT RAISE(ABORT, 'graph planner provider is immutable'); END;

ALTER TABLE planning_attempts ADD COLUMN planner_provider_v2 TEXT
    CHECK (planner_provider_v2 IS NULL OR planner_provider_v2 IN ('gemini', 'codex', 'claude', 'agy'));
UPDATE planning_attempts SET planner_provider_v2 = planner_provider;
CREATE TRIGGER planning_attempt_provider_v2_immutable
BEFORE UPDATE OF planner_provider_v2 ON planning_attempts
BEGIN SELECT RAISE(ABORT, 'planning attempt provider is immutable'); END;

ALTER TABLE session_tasks ADD COLUMN provider_id_v2 TEXT
    CHECK (provider_id_v2 IS NULL OR provider_id_v2 IN ('gemini', 'codex', 'claude', 'agy'));
UPDATE session_tasks SET provider_id_v2 = provider_id;

ALTER TABLE graph_approvals ADD COLUMN session_id TEXT
    REFERENCES sessions(session_id) ON DELETE RESTRICT;
ALTER TABLE graph_approvals ADD COLUMN requirement_revision_id TEXT
    REFERENCES requirement_revisions(requirement_revision_id) ON DELETE RESTRICT;
ALTER TABLE graph_approvals ADD COLUMN validation_hash TEXT
    CHECK (validation_hash IS NULL OR length(validation_hash) = 64);
ALTER TABLE graph_approvals ADD COLUMN base_commit TEXT
    CHECK (base_commit IS NULL OR length(base_commit) BETWEEN 40 AND 64);
UPDATE graph_approvals
SET session_id = (SELECT session_id FROM graph_revisions
                  WHERE graph_revisions.revision_id = graph_approvals.revision_id),
    requirement_revision_id = (SELECT requirement_revision_id FROM graph_revisions
                               WHERE graph_revisions.revision_id = graph_approvals.revision_id),
    validation_hash = (SELECT validation_hash FROM graph_revisions
                       WHERE graph_revisions.revision_id = graph_approvals.revision_id),
    base_commit = (SELECT base_commit FROM graph_revisions
                   WHERE graph_revisions.revision_id = graph_approvals.revision_id);

ALTER TABLE daemon_instances ADD COLUMN executable_path TEXT;
ALTER TABLE daemon_instances ADD COLUMN build_version TEXT;
ALTER TABLE daemon_instances ADD COLUMN build_target TEXT;

PRAGMA user_version = 11;
