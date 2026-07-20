CREATE TABLE session_workspace_state (
    session_id TEXT PRIMARY KEY NOT NULL REFERENCES sessions(session_id) ON DELETE RESTRICT,
    selected_task_id TEXT REFERENCES tasks(task_id) ON DELETE RESTRICT,
    updated_at TEXT NOT NULL
) STRICT;

PRAGMA user_version = 5;
