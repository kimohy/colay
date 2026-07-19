use std::{
    fs::{self, OpenOptions},
    io::{Read as _, Seek as _, SeekFrom, Write as _},
    path::{Path, PathBuf},
};

use fs2::FileExt;
use orchestrator_domain::TaskEvent;
use serde::{Deserialize, Serialize};

use crate::{
    Database, StateError, StateResult, ensure_private_directory, ensure_private_file,
    reject_symlink_components,
};

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationReport {
    pub verified_events: u64,
    pub appended_events: u64,
    pub last_sequence: i64,
    pub last_hash: Option<String>,
}

#[derive(Clone, Debug)]
pub struct EventLog {
    path: PathBuf,
}

impl EventLog {
    pub fn open(path: impl Into<PathBuf>) -> StateResult<Self> {
        let path = path.into();
        let parent = path.parent().ok_or_else(|| StateError::InvalidEventChain {
            sequence: 0,
            reason: format!("event-log path has no parent: {}", path.display()),
        })?;
        ensure_private_directory(parent)?;
        reject_symlink_components(&path)?;
        if !path.exists() {
            let file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(|error| StateError::io(&path, error))?;
            file.sync_all()
                .map_err(|error| StateError::io(&path, error))?;
        }
        ensure_private_file(&path)?;
        Ok(Self { path })
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reconciles the append-only JSONL replica against `SQLite`, which is the source of
    /// truth. It never truncates or rewrites an invalid log.
    pub fn reconcile(&self, database: &Database) -> StateResult<ReconciliationReport> {
        let mut file = OpenOptions::new()
            .read(true)
            .append(true)
            .open(&self.path)
            .map_err(|error| StateError::io(&self.path, error))?;
        file.lock_exclusive()
            .map_err(|error| StateError::io(&self.path, error))?;
        let result = self.reconcile_locked(database, &mut file);
        let unlock = FileExt::unlock(&file).map_err(|error| StateError::io(&self.path, error));
        match (result, unlock) {
            (Ok(report), Ok(())) => Ok(report),
            (Err(error), _) | (Ok(_), Err(error)) => Err(error),
        }
    }

    #[allow(clippy::too_many_lines)]
    fn reconcile_locked(
        &self,
        database: &Database,
        file: &mut fs::File,
    ) -> StateResult<ReconciliationReport> {
        file.seek(SeekFrom::Start(0))
            .map_err(|error| StateError::io(&self.path, error))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| StateError::io(&self.path, error))?;
        if !bytes.is_empty() && bytes.last() != Some(&b'\n') {
            return Err(StateError::TornEventLogTail);
        }

        let mut report = ReconciliationReport::default();
        let mut previous_hash: Option<String> = None;
        for line in bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            let event: TaskEvent = serde_json::from_slice(line)?;
            let sequence =
                i64::try_from(event.sequence).map_err(|_| StateError::InvalidEventChain {
                    sequence: i64::MAX,
                    reason: "event sequence exceeds SQLite range".to_owned(),
                })?;
            if sequence != report.last_sequence + 1 {
                return Err(StateError::InvalidEventChain {
                    sequence,
                    reason: "JSONL sequence is not contiguous".to_owned(),
                });
            }
            if event.previous_hash != previous_hash {
                return Err(StateError::InvalidEventChain {
                    sequence,
                    reason: "JSONL previous_hash does not match predecessor".to_owned(),
                });
            }
            if !event
                .verify_hash()
                .map_err(|error| StateError::InvalidEventChain {
                    sequence,
                    reason: error.to_string(),
                })?
            {
                return Err(StateError::InvalidEventChain {
                    sequence,
                    reason: "JSONL event hash verification failed".to_owned(),
                });
            }
            let database_event =
                database
                    .event_at(sequence)?
                    .ok_or_else(|| StateError::InvalidEventChain {
                        sequence,
                        reason: "JSONL contains an event not present in SQLite".to_owned(),
                    })?;
            if database_event != event {
                return Err(StateError::InvalidEventChain {
                    sequence,
                    reason: "JSONL event differs from SQLite outbox".to_owned(),
                });
            }
            previous_hash = Some(event.event_hash.clone());
            report.verified_events += 1;
            report.last_sequence = sequence;
            report.last_hash.clone_from(&previous_hash);
        }

        // A crash can happen after the JSONL fsync and before the SQLite exported marker
        // transaction. Reaffirming the verified prefix makes that case idempotent.
        if let Some(hash) = &report.last_hash {
            database.mark_exported(report.last_sequence, hash)?;
        }

        file.seek(SeekFrom::End(0))
            .map_err(|error| StateError::io(&self.path, error))?;
        loop {
            let pending = database.outbox_after(report.last_sequence, 256)?;
            if pending.is_empty() {
                break;
            }
            for record in pending {
                if record.sequence != report.last_sequence + 1
                    || record.event.previous_hash != report.last_hash
                {
                    return Err(StateError::InvalidEventChain {
                        sequence: record.sequence,
                        reason: "SQLite outbox chain is not contiguous".to_owned(),
                    });
                }
                if !record
                    .event
                    .verify_hash()
                    .map_err(|error| StateError::InvalidEventChain {
                        sequence: record.sequence,
                        reason: error.to_string(),
                    })?
                {
                    return Err(StateError::InvalidEventChain {
                        sequence: record.sequence,
                        reason: "SQLite event hash verification failed".to_owned(),
                    });
                }
                serde_json::to_writer(&mut *file, &record.event)?;
                file.write_all(b"\n")
                    .map_err(|error| StateError::io(&self.path, error))?;
                report.last_sequence = record.sequence;
                report.last_hash = Some(record.event.event_hash.clone());
                report.appended_events += 1;
            }
            file.sync_all()
                .map_err(|error| StateError::io(&self.path, error))?;
            if let Some(hash) = &report.last_hash {
                database.mark_exported(report.last_sequence, hash)?;
            }
        }
        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use orchestrator_domain::{
        CorrelationId, EventActor, EventId, EventType, SchemaVersion, TaskEvent,
    };
    use serde_json::json;

    use crate::{Database, EventLog};

    #[test]
    fn reconciles_missing_outbox_events_idempotently() {
        let directory = crate::CanonicalTempDir::new("tempdir")
            .unwrap_or_else(|error| panic!("tempdir: {error}"));
        let database = Database::open(directory.path().join("state.db"))
            .unwrap_or_else(|error| panic!("db: {error}"));
        database
            .migrate_with_backup(directory.path())
            .unwrap_or_else(|error| panic!("migration: {error}"));
        let event = TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 0,
            event_id: EventId::new(),
            task_id: None,
            occurred_at: Utc::now(),
            event_type: EventType::CompatibilityWarning,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::System,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({"warning": "fixture"}),
            previous_hash: None,
            event_hash: String::new(),
        };
        database
            .append_event(event)
            .unwrap_or_else(|error| panic!("append: {error}"));
        let log = EventLog::open(directory.path().join("events.jsonl"))
            .unwrap_or_else(|error| panic!("log: {error}"));
        let first = log
            .reconcile(&database)
            .unwrap_or_else(|error| panic!("first reconcile: {error}"));
        assert_eq!(first.appended_events, 1);
        let second = log
            .reconcile(&database)
            .unwrap_or_else(|error| panic!("second reconcile: {error}"));
        assert_eq!(second.appended_events, 0);
        assert_eq!(second.verified_events, 1);
    }
}
