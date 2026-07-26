CREATE TABLE legacy_imports (
    source_fingerprint TEXT PRIMARY KEY NOT NULL CHECK (length(source_fingerprint) = 64),
    workspace_id TEXT NOT NULL REFERENCES workspaces(workspace_id) ON DELETE RESTRICT,
    manifest_hash TEXT NOT NULL CHECK (length(manifest_hash) = 64),
    imported_at TEXT NOT NULL,
    result_json TEXT NOT NULL CHECK (json_valid(result_json)),
    UNIQUE(source_fingerprint, workspace_id)
) STRICT;

CREATE INDEX legacy_imports_workspace_time
    ON legacy_imports(workspace_id, imported_at DESC);

CREATE TABLE legacy_import_id_mappings (
    source_fingerprint TEXT NOT NULL,
    workspace_id TEXT NOT NULL,
    entity_type TEXT NOT NULL CHECK (length(trim(entity_type)) > 0),
    source_id TEXT NOT NULL CHECK (length(trim(source_id)) > 0),
    target_id TEXT NOT NULL CHECK (length(trim(target_id)) > 0),
    PRIMARY KEY(source_fingerprint, entity_type, source_id),
    UNIQUE(workspace_id, entity_type, target_id),
    FOREIGN KEY(source_fingerprint, workspace_id)
        REFERENCES legacy_imports(source_fingerprint, workspace_id) ON DELETE RESTRICT
) STRICT;

PRAGMA user_version = 14;
