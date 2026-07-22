ALTER TABLE daemon_instances
    ADD COLUMN phase TEXT NOT NULL DEFAULT 'online'
    CHECK (phase IN ('booting', 'probing', 'online', 'failed'));

ALTER TABLE daemon_instances
    ADD COLUMN startup_error TEXT;

PRAGMA user_version = 9;
