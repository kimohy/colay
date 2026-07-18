CREATE TABLE routing_decisions (
    decision_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
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
    decided_at TEXT NOT NULL
) STRICT;

CREATE TABLE routing_decision_usage (
    decision_id TEXT NOT NULL REFERENCES routing_decisions(decision_id) ON DELETE RESTRICT,
    snapshot_id TEXT NOT NULL REFERENCES provider_usage_snapshots(snapshot_id) ON DELETE RESTRICT,
    PRIMARY KEY (decision_id, snapshot_id)
) STRICT;

CREATE TABLE artifacts (
    artifact_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    kind TEXT NOT NULL,
    relative_path TEXT NOT NULL,
    sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
    byte_length INTEGER NOT NULL CHECK (byte_length >= 0),
    media_type TEXT,
    created_at TEXT NOT NULL,
    UNIQUE (relative_path)
) STRICT;

CREATE TABLE command_evidence (
    command_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    attempt_id TEXT REFERENCES task_attempts(attempt_id) ON DELETE RESTRICT,
    executable TEXT NOT NULL,
    args_json TEXT NOT NULL CHECK (json_valid(args_json)),
    working_directory TEXT,
    exit_code INTEGER,
    termination TEXT NOT NULL,
    stdout_artifact_id TEXT REFERENCES artifacts(artifact_id) ON DELETE RESTRICT,
    stderr_artifact_id TEXT REFERENCES artifacts(artifact_id) ON DELETE RESTRICT,
    stdout_truncated INTEGER NOT NULL CHECK (stdout_truncated IN (0, 1)),
    stderr_truncated INTEGER NOT NULL CHECK (stderr_truncated IN (0, 1)),
    invalid_utf8 INTEGER NOT NULL CHECK (invalid_utf8 IN (0, 1)),
    started_at TEXT NOT NULL,
    ended_at TEXT NOT NULL
) STRICT;

CREATE TABLE checkpoints (
    checkpoint_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    attempt_id TEXT REFERENCES task_attempts(attempt_id) ON DELETE RESTRICT,
    schema_version TEXT NOT NULL,
    checkpoint_json TEXT NOT NULL CHECK (json_valid(checkpoint_json)),
    integrity_hash TEXT NOT NULL CHECK (length(integrity_hash) = 64),
    diff_artifact_id TEXT REFERENCES artifacts(artifact_id) ON DELETE RESTRICT,
    git_head TEXT,
    created_at TEXT NOT NULL
) STRICT;

CREATE TABLE handovers (
    handover_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    checkpoint_id TEXT NOT NULL REFERENCES checkpoints(checkpoint_id) ON DELETE RESTRICT,
    schema_version TEXT NOT NULL,
    from_provider TEXT NOT NULL,
    to_provider TEXT NOT NULL,
    reason TEXT NOT NULL,
    bundle_json TEXT NOT NULL CHECK (json_valid(bundle_json)),
    integrity_hash TEXT NOT NULL CHECK (length(integrity_hash) = 64),
    acknowledgement_json TEXT CHECK (acknowledgement_json IS NULL OR json_valid(acknowledgement_json)),
    started_at TEXT NOT NULL,
    completed_at TEXT
) STRICT;

CREATE TABLE verification_results (
    verification_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    attempt_id TEXT REFERENCES task_attempts(attempt_id) ON DELETE RESTRICT,
    reviewer_provider TEXT,
    outcome TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    result_json TEXT NOT NULL CHECK (json_valid(result_json)),
    started_at TEXT NOT NULL,
    completed_at TEXT NOT NULL
) STRICT;

CREATE INDEX routing_decisions_task_time ON routing_decisions(task_id, decided_at DESC);
CREATE INDEX checkpoints_task_time ON checkpoints(task_id, created_at DESC);
CREATE INDEX handovers_task_time ON handovers(task_id, started_at DESC);
CREATE INDEX verification_task_time ON verification_results(task_id, completed_at DESC);

PRAGMA user_version = 2;
