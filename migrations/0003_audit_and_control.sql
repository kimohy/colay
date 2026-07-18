CREATE TABLE task_events (
    sequence INTEGER PRIMARY KEY AUTOINCREMENT,
    event_id TEXT NOT NULL UNIQUE,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    event_type TEXT NOT NULL,
    schema_version TEXT NOT NULL,
    occurred_at TEXT NOT NULL,
    event_json TEXT NOT NULL CHECK (json_valid(event_json)),
    previous_hash TEXT,
    event_hash TEXT NOT NULL CHECK (length(event_hash) = 64),
    exported_at TEXT
) STRICT;

CREATE TABLE event_log_state (
    singleton INTEGER PRIMARY KEY NOT NULL CHECK (singleton = 1),
    last_exported_sequence INTEGER NOT NULL DEFAULT 0 CHECK (last_exported_sequence >= 0),
    last_exported_hash TEXT,
    updated_at TEXT NOT NULL
) STRICT;

INSERT INTO event_log_state(singleton, last_exported_sequence, last_exported_hash, updated_at)
VALUES (1, 0, NULL, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'));

CREATE TABLE task_controls (
    control_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    action TEXT NOT NULL,
    payload_json TEXT NOT NULL CHECK (json_valid(payload_json)),
    requested_by TEXT NOT NULL,
    requested_at TEXT NOT NULL,
    claimed_at TEXT,
    completed_at TEXT,
    outcome TEXT
) STRICT;

CREATE TABLE worktrees (
    worktree_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    repo_root TEXT NOT NULL,
    worktree_path TEXT NOT NULL UNIQUE,
    branch_name TEXT NOT NULL UNIQUE,
    base_revision TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at TEXT NOT NULL,
    cleanup_approved_at TEXT,
    archived_at TEXT
) STRICT;

CREATE TABLE coordinator_leases (
    lease_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    worktree_id TEXT REFERENCES worktrees(worktree_id) ON DELETE RESTRICT,
    owner_id TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    renewed_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT
) STRICT;

CREATE UNIQUE INDEX one_active_coordinator_lease_per_task
    ON coordinator_leases(task_id)
    WHERE released_at IS NULL;

CREATE TABLE worker_leases (
    lease_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    worktree_id TEXT REFERENCES worktrees(worktree_id) ON DELETE RESTRICT,
    coordinator_lease_id TEXT REFERENCES coordinator_leases(lease_id) ON DELETE RESTRICT,
    provider_id TEXT NOT NULL,
    mode TEXT NOT NULL,
    acquired_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    released_at TEXT
) STRICT;

CREATE UNIQUE INDEX one_active_writable_lease_per_task
    ON worker_leases(task_id)
    WHERE mode = 'writable' AND released_at IS NULL;

CREATE TABLE changed_files (
    task_id TEXT NOT NULL REFERENCES tasks(task_id) ON DELETE RESTRICT,
    worktree_id TEXT NOT NULL REFERENCES worktrees(worktree_id) ON DELETE RESTRICT,
    relative_path TEXT NOT NULL,
    owner_lease_id TEXT REFERENCES worker_leases(lease_id) ON DELETE RESTRICT,
    sha256 TEXT,
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    PRIMARY KEY (task_id, relative_path)
) STRICT;

CREATE TABLE approval_records (
    approval_id TEXT PRIMARY KEY NOT NULL,
    task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    action TEXT NOT NULL,
    scope_json TEXT NOT NULL CHECK (json_valid(scope_json)),
    approved_by TEXT NOT NULL,
    approved_at TEXT NOT NULL,
    expires_at TEXT,
    revoked_at TEXT
) STRICT;

CREATE TABLE compatibility_runs (
    run_id TEXT PRIMARY KEY NOT NULL,
    provider_id TEXT NOT NULL,
    detected_version TEXT,
    classification TEXT NOT NULL,
    capabilities_json TEXT NOT NULL CHECK (json_valid(capabilities_json)),
    fixture_fingerprint TEXT,
    report_artifact_id TEXT REFERENCES artifacts(artifact_id) ON DELETE RESTRICT,
    checked_at TEXT NOT NULL
) STRICT;

CREATE INDEX task_events_task_sequence ON task_events(task_id, sequence);
CREATE INDEX task_controls_pending ON task_controls(task_id, requested_at)
    WHERE claimed_at IS NULL;
CREATE INDEX worker_leases_expiry ON worker_leases(expires_at)
    WHERE released_at IS NULL;
CREATE INDEX coordinator_leases_expiry ON coordinator_leases(expires_at)
    WHERE released_at IS NULL;
CREATE INDEX worker_leases_coordinator ON worker_leases(coordinator_lease_id)
    WHERE released_at IS NULL;

PRAGMA user_version = 3;
