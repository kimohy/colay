use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Clone, Debug, Error, PartialEq, Eq)]
#[error("invalid UUID for {kind}: {source}")]
pub struct IdParseError {
    kind: &'static str,
    #[source]
    source: uuid::Error,
}

macro_rules! uuid_id {
    ($name:ident) => {
        #[derive(
            Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub struct $name(Uuid);

        impl $name {
            #[must_use]
            pub fn new() -> Self {
                Self(Uuid::now_v7())
            }

            #[must_use]
            pub const fn from_uuid(value: Uuid) -> Self {
                Self(value)
            }

            #[must_use]
            pub const fn as_uuid(&self) -> &Uuid {
                &self.0
            }

            #[must_use]
            pub const fn into_uuid(self) -> Uuid {
                self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.0.fmt(formatter)
            }
        }

        impl FromStr for $name {
            type Err = IdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Uuid::parse_str(value)
                    .map(Self)
                    .map_err(|source| IdParseError {
                        kind: stringify!($name),
                        source,
                    })
            }
        }

        impl From<Uuid> for $name {
            fn from(value: Uuid) -> Self {
                Self(value)
            }
        }
    };
}

uuid_id!(TaskId);
uuid_id!(AttemptId);
uuid_id!(EventId);
uuid_id!(CheckpointId);
uuid_id!(HandoverId);
uuid_id!(RoutingDecisionId);
uuid_id!(VerificationId);
uuid_id!(CorrelationId);
uuid_id!(CommandEvidenceId);
uuid_id!(SessionId);
uuid_id!(MessageId);
uuid_id!(ClientCommandId);
uuid_id!(DaemonInstanceId);
uuid_id!(GraphRevisionId);
uuid_id!(PlanningAttemptId);
uuid_id!(ConversationAttemptId);
uuid_id!(RequirementRevisionId);
uuid_id!(InstructionId);
uuid_id!(ResourceClaimId);
uuid_id!(ScheduleClaimId);
uuid_id!(IntegrationBatchId);
uuid_id!(IntegrationApplicationId);

#[cfg(test)]
mod tests {
    use super::TaskId;

    #[test]
    fn generated_ids_are_uuid_v7() {
        assert_eq!(TaskId::new().as_uuid().get_version_num(), 7);
    }
}
