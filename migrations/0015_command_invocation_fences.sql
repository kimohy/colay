CREATE TABLE client_command_invocations (
    workspace_id TEXT NOT NULL DEFAULT (current_workspace())
        REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    command_id TEXT NOT NULL,
    root_command_id TEXT NOT NULL,
    plan_only INTEGER NOT NULL CHECK (plan_only IN (0, 1)),
    recorded_at TEXT NOT NULL,
    PRIMARY KEY(workspace_id, command_id),
    FOREIGN KEY(workspace_id, command_id)
        REFERENCES client_commands(workspace_id, command_id) ON DELETE RESTRICT,
    FOREIGN KEY(workspace_id, root_command_id)
        REFERENCES client_commands(workspace_id, command_id) ON DELETE RESTRICT
) STRICT;

CREATE INDEX client_command_invocations_root
    ON client_command_invocations(workspace_id, root_command_id, command_id);

INSERT INTO client_command_invocations(
    workspace_id, command_id, root_command_id, plan_only, recorded_at
)
SELECT workspace_id, command_id, command_id,
       CASE WHEN requested_by = 'local-cli-run-plan-only' THEN 1 ELSE 0 END,
       requested_at
FROM client_commands;

PRAGMA user_version = 15;
