CREATE TABLE schema_migrations (
    version INTEGER PRIMARY KEY NOT NULL CHECK (version > 0),
    name TEXT NOT NULL,
    checksum TEXT NOT NULL CHECK (length(checksum) = 64),
    applied_at TEXT NOT NULL
) STRICT;

CREATE TABLE tasks (
    task_id TEXT PRIMARY KEY NOT NULL,
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
    archived_at TEXT
) STRICT;

CREATE TABLE task_attempts (
    attempt_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    ordinal INTEGER NOT NULL CHECK (ordinal > 0),
    provider_id TEXT,
    worker_mode TEXT,
    started_at TEXT NOT NULL,
    ended_at TEXT,
    outcome TEXT,
    worker_result_json TEXT CHECK (worker_result_json IS NULL OR json_valid(worker_result_json)),
    UNIQUE (task_id, ordinal)
) STRICT;

CREATE TABLE provider_usage_snapshots (
    snapshot_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
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

CREATE INDEX provider_usage_provider_time
    ON provider_usage_snapshots(provider_id, collected_at DESC);

CREATE TABLE provider_health (
    health_id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL,
    status TEXT NOT NULL,
    consecutive_failures INTEGER NOT NULL DEFAULT 0 CHECK (consecutive_failures >= 0),
    details_json TEXT NOT NULL CHECK (json_valid(details_json)),
    checked_at TEXT NOT NULL
) STRICT;

CREATE INDEX provider_health_provider_time
    ON provider_health(provider_id, checked_at DESC);

PRAGMA user_version = 1;
