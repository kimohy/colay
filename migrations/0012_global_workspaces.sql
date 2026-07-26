CREATE TABLE workspaces (
    workspace_id TEXT PRIMARY KEY NOT NULL,
    kind TEXT NOT NULL CHECK (kind IN ('git', 'directory')),
    status TEXT NOT NULL CHECK (status IN ('active', 'detached', 'archived')),
    created_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL
);
CREATE TABLE workspace_paths (
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id),
    canonical_path TEXT NOT NULL,
    comparison_key TEXT NOT NULL,
    git_common_dir TEXT,
    is_current INTEGER NOT NULL CHECK (is_current IN (0, 1)),
    first_seen_at TEXT NOT NULL,
    last_seen_at TEXT NOT NULL,
    PRIMARY KEY (workspace_id, comparison_key)
);
CREATE UNIQUE INDEX workspace_paths_one_current_path
ON workspace_paths(comparison_key) WHERE is_current = 1;

PRAGMA user_version = 12;
