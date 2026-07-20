use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    CorrelationId, EventId, IntegrityError, ProviderId, SchemaVersion, SessionId, TaskId,
    TaskState, canonical_sha256,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    TaskCreated,
    AssessmentCompleted,
    UsageCollected,
    RouteSelected,
    WorkerStarted,
    WorkerEvent,
    CheckpointCreated,
    HandoverStarted,
    HandoverCompleted,
    VerificationStarted,
    VerificationCompleted,
    TaskCompleted,
    TaskBlocked,
    ProviderExhausted,
    CompatibilityWarning,
    MigrationStarted,
    MigrationCompleted,
    RollbackPlanned,
    StateTransitioned,
    ControlRequested,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "provider", rename_all = "snake_case")]
pub enum EventActor {
    User,
    Administrator,
    Orchestrator,
    Provider(ProviderId),
    System,
}

/// Append-only audit event. `event_hash` covers every preceding field, including the
/// previous event hash, after replacing `event_hash` itself with the empty string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskEvent {
    pub schema_version: SchemaVersion,
    pub sequence: u64,
    pub event_id: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    pub task_id: Option<TaskId>,
    pub occurred_at: DateTime<Utc>,
    pub event_type: EventType,
    pub from_state: Option<TaskState>,
    pub to_state: Option<TaskState>,
    pub reason: Option<String>,
    pub actor: EventActor,
    pub correlation_id: CorrelationId,
    pub causation_id: Option<EventId>,
    pub payload: serde_json::Value,
    pub previous_hash: Option<String>,
    pub event_hash: String,
}

/// Compatibility alias used by persistence code.
pub type EventEnvelope = TaskEvent;

impl TaskEvent {
    /// Calculates and installs the hash for this event and its previous-hash link.
    ///
    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the event cannot be serialized.
    pub fn seal(mut self) -> Result<Self, IntegrityError> {
        self.refresh_event_hash()?;
        Ok(self)
    }

    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the event cannot be serialized.
    pub fn refresh_event_hash(&mut self) -> Result<(), IntegrityError> {
        self.event_hash.clear();
        self.event_hash = canonical_sha256(self)?;
        Ok(())
    }

    /// # Errors
    ///
    /// Returns [`IntegrityError`] when the event cannot be serialized for comparison.
    pub fn verify_hash(&self) -> Result<bool, IntegrityError> {
        let mut candidate = self.clone();
        let expected = std::mem::take(&mut candidate.event_hash);
        Ok(!expected.is_empty() && canonical_sha256(&candidate)? == expected)
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::*;

    #[test]
    fn chained_event_hash_is_tamper_evident() -> Result<(), Box<dyn std::error::Error>> {
        let mut event = TaskEvent {
            schema_version: SchemaVersion::state_current(),
            sequence: 1,
            event_id: EventId::new(),
            session_id: None,
            task_id: Some(TaskId::new()),
            occurred_at: Utc::now(),
            event_type: EventType::TaskCreated,
            from_state: None,
            to_state: Some(TaskState::Queued),
            reason: None,
            actor: EventActor::User,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({"objective": "safe change"}),
            previous_hash: None,
            event_hash: String::new(),
        }
        .seal()?;
        assert!(event.verify_hash()?);
        event.payload = json!({"objective": "tampered"});
        assert!(!event.verify_hash()?);
        Ok(())
    }

    #[test]
    fn absent_session_id_is_omitted_for_historical_event_hash_compatibility()
    -> Result<(), Box<dyn std::error::Error>> {
        let event = TaskEvent {
            schema_version: SchemaVersion::new(SchemaVersion::V3),
            sequence: 1,
            event_id: EventId::new(),
            session_id: None,
            task_id: None,
            occurred_at: Utc::now(),
            event_type: EventType::CompatibilityWarning,
            from_state: None,
            to_state: None,
            reason: None,
            actor: EventActor::System,
            correlation_id: CorrelationId::new(),
            causation_id: None,
            payload: json!({}),
            previous_hash: None,
            event_hash: String::new(),
        };
        assert!(serde_json::to_value(event)?.get("session_id").is_none());
        Ok(())
    }
}
