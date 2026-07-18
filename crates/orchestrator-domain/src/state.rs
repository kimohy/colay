use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Queued,
    Analyzing,
    Planned,
    Running,
    CheckpointRequested,
    Checkpointing,
    Checkpointed,
    HandoverRequested,
    HandingOver,
    Resuming,
    Verifying,
    Completed,
    Blocked,
    Failed,
    Cancelled,
}

impl TaskState {
    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }

    /// Validates both the explicit transition matrix and its evidence guards.
    ///
    /// # Errors
    ///
    /// Returns [`TransitionError`] when the edge is not listed or required evidence is absent.
    #[allow(clippy::too_many_lines)]
    pub fn validate_transition(
        self,
        next: Self,
        guards: &TransitionGuards,
    ) -> Result<(), TransitionError> {
        if self == next {
            return Err(TransitionError::NoOp(self));
        }
        let allowed = match self {
            Self::Queued => matches!(next, Self::Analyzing | Self::Cancelled | Self::Failed),
            Self::Analyzing => matches!(
                next,
                Self::Planned | Self::Blocked | Self::Cancelled | Self::Failed
            ),
            Self::Planned => matches!(
                next,
                Self::Running
                    | Self::CheckpointRequested
                    | Self::Blocked
                    | Self::Cancelled
                    | Self::Failed
            ),
            Self::Running => {
                matches!(
                    next,
                    Self::CheckpointRequested | Self::Verifying | Self::Failed
                ) || (next == Self::Blocked && guards.process_tree_termination_unconfirmed)
            }
            Self::CheckpointRequested => {
                matches!(next, Self::Checkpointing | Self::Failed)
            }
            Self::Checkpointing => {
                matches!(next, Self::Checkpointed | Self::Blocked | Self::Failed)
            }
            Self::Checkpointed => matches!(
                next,
                Self::HandoverRequested
                    | Self::Resuming
                    | Self::Verifying
                    | Self::Blocked
                    | Self::Cancelled
                    | Self::Failed
            ),
            Self::HandoverRequested => matches!(
                next,
                Self::HandingOver
                    | Self::Checkpointed
                    | Self::Blocked
                    | Self::Cancelled
                    | Self::Failed
            ),
            Self::HandingOver => matches!(
                next,
                Self::Resuming | Self::Checkpointed | Self::Blocked | Self::Failed
            ),
            Self::Resuming => matches!(
                next,
                Self::Analyzing
                    | Self::Planned
                    | Self::Running
                    | Self::Verifying
                    | Self::Blocked
                    | Self::Cancelled
                    | Self::Failed
            ),
            Self::Verifying => matches!(
                next,
                Self::Completed
                    | Self::CheckpointRequested
                    | Self::Blocked
                    | Self::Cancelled
                    | Self::Failed
            ),
            Self::Blocked => {
                matches!(next, Self::Cancelled | Self::Failed)
                    || guards.resume_point.is_some_and(|point| point == next)
            }
            Self::Completed | Self::Failed | Self::Cancelled => false,
        };

        if !allowed {
            return Err(TransitionError::NotAllowed {
                from: self,
                to: next,
            });
        }
        if next == Self::Checkpointed && !guards.checkpoint_integrity_verified {
            return Err(TransitionError::CheckpointNotVerified);
        }
        if self == Self::HandingOver
            && next == Self::Resuming
            && !(guards.handover_integrity_verified && guards.handover_acknowledged)
        {
            return Err(TransitionError::HandoverNotVerified);
        }
        if next == Self::Completed {
            if !guards.verification_passed {
                return Err(TransitionError::VerificationNotPassed);
            }
            if guards.independent_review_required && !guards.independent_review_satisfied {
                return Err(TransitionError::IndependentReviewMissing);
            }
            if guards.pending_approval {
                return Err(TransitionError::PendingApproval);
            }
        }
        Ok(())
    }
}

// Independent evidence flags are intentionally explicit so callers cannot accidentally
// satisfy one safety gate with evidence collected for another.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransitionGuards {
    pub resume_point: Option<TaskState>,
    pub checkpoint_integrity_verified: bool,
    pub handover_integrity_verified: bool,
    pub handover_acknowledged: bool,
    pub verification_passed: bool,
    pub independent_review_required: bool,
    pub independent_review_satisfied: bool,
    pub pending_approval: bool,
    /// Allows a running task to become blocked without a checkpoint only when a reaped
    /// direct child may still have live descendants. The worker lease remains retained.
    pub process_tree_termination_unconfirmed: bool,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum TransitionError {
    #[error("task is already in state {0:?}")]
    NoOp(TaskState),
    #[error("transition from {from:?} to {to:?} is not allowed")]
    NotAllowed { from: TaskState, to: TaskState },
    #[error("checkpoint integrity has not been verified")]
    CheckpointNotVerified,
    #[error("handover integrity and acknowledgement have not both been verified")]
    HandoverNotVerified,
    #[error("independent verification has not passed")]
    VerificationNotPassed,
    #[error("required independent provider review is missing")]
    IndependentReviewMissing,
    #[error("an approval remains pending")]
    PendingApproval,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn terminals_never_transition() {
        for terminal in [
            TaskState::Completed,
            TaskState::Failed,
            TaskState::Cancelled,
        ] {
            for next in all_states() {
                assert!(
                    terminal
                        .validate_transition(next, &TransitionGuards::default())
                        .is_err()
                );
            }
        }
    }

    #[test]
    fn running_cancel_must_go_through_checkpoint() {
        assert!(
            TaskState::Running
                .validate_transition(TaskState::Cancelled, &TransitionGuards::default())
                .is_err()
        );
        assert_eq!(
            TaskState::Running
                .validate_transition(TaskState::CheckpointRequested, &TransitionGuards::default()),
            Ok(())
        );
    }

    #[test]
    fn running_can_block_when_process_tree_termination_is_unconfirmed() {
        assert!(
            TaskState::Running
                .validate_transition(TaskState::Blocked, &TransitionGuards::default())
                .is_err()
        );
        assert_eq!(
            TaskState::Running.validate_transition(
                TaskState::Blocked,
                &TransitionGuards {
                    process_tree_termination_unconfirmed: true,
                    ..TransitionGuards::default()
                },
            ),
            Ok(())
        );
    }

    #[test]
    fn blocked_resumes_only_at_recorded_point() {
        let guards = TransitionGuards {
            resume_point: Some(TaskState::Running),
            ..TransitionGuards::default()
        };
        assert_eq!(
            TaskState::Blocked.validate_transition(TaskState::Running, &guards),
            Ok(())
        );
        assert!(
            TaskState::Blocked
                .validate_transition(TaskState::Planned, &guards)
                .is_err()
        );
    }

    #[test]
    fn completion_requires_all_applicable_guards() {
        let guards = TransitionGuards {
            verification_passed: true,
            independent_review_required: true,
            independent_review_satisfied: true,
            ..TransitionGuards::default()
        };
        assert_eq!(
            TaskState::Verifying.validate_transition(TaskState::Completed, &guards),
            Ok(())
        );
    }

    #[test]
    fn transition_matrix_rejects_every_unlisted_edge() {
        let guards = TransitionGuards {
            resume_point: Some(TaskState::Analyzing),
            checkpoint_integrity_verified: true,
            handover_integrity_verified: true,
            handover_acknowledged: true,
            verification_passed: true,
            independent_review_required: true,
            independent_review_satisfied: true,
            pending_approval: false,
            process_tree_termination_unconfirmed: true,
        };
        for from in all_states() {
            for to in all_states() {
                let expected = listed_edge(from, to);
                assert_eq!(
                    from.validate_transition(to, &guards).is_ok(),
                    expected,
                    "unexpected matrix result for {from:?} -> {to:?}"
                );
            }
        }
    }

    fn listed_edge(from: TaskState, to: TaskState) -> bool {
        match from {
            TaskState::Queued => matches!(
                to,
                TaskState::Analyzing | TaskState::Cancelled | TaskState::Failed
            ),
            TaskState::Analyzing => matches!(
                to,
                TaskState::Planned | TaskState::Blocked | TaskState::Cancelled | TaskState::Failed
            ),
            TaskState::Planned => matches!(
                to,
                TaskState::Running
                    | TaskState::CheckpointRequested
                    | TaskState::Blocked
                    | TaskState::Cancelled
                    | TaskState::Failed
            ),
            TaskState::Running => matches!(
                to,
                TaskState::CheckpointRequested
                    | TaskState::Verifying
                    | TaskState::Blocked
                    | TaskState::Failed
            ),
            TaskState::CheckpointRequested => {
                matches!(to, TaskState::Checkpointing | TaskState::Failed)
            }
            TaskState::Checkpointing => matches!(
                to,
                TaskState::Checkpointed | TaskState::Blocked | TaskState::Failed
            ),
            TaskState::Checkpointed => matches!(
                to,
                TaskState::HandoverRequested
                    | TaskState::Resuming
                    | TaskState::Verifying
                    | TaskState::Blocked
                    | TaskState::Cancelled
                    | TaskState::Failed
            ),
            TaskState::HandoverRequested => matches!(
                to,
                TaskState::HandingOver
                    | TaskState::Checkpointed
                    | TaskState::Blocked
                    | TaskState::Cancelled
                    | TaskState::Failed
            ),
            TaskState::HandingOver => matches!(
                to,
                TaskState::Resuming
                    | TaskState::Checkpointed
                    | TaskState::Blocked
                    | TaskState::Failed
            ),
            TaskState::Resuming => matches!(
                to,
                TaskState::Analyzing
                    | TaskState::Planned
                    | TaskState::Running
                    | TaskState::Verifying
                    | TaskState::Blocked
                    | TaskState::Cancelled
                    | TaskState::Failed
            ),
            TaskState::Verifying => matches!(
                to,
                TaskState::Completed
                    | TaskState::CheckpointRequested
                    | TaskState::Blocked
                    | TaskState::Cancelled
                    | TaskState::Failed
            ),
            TaskState::Blocked => matches!(
                to,
                TaskState::Analyzing | TaskState::Cancelled | TaskState::Failed
            ),
            TaskState::Completed | TaskState::Failed | TaskState::Cancelled => false,
        }
    }

    fn all_states() -> [TaskState; 15] {
        [
            TaskState::Queued,
            TaskState::Analyzing,
            TaskState::Planned,
            TaskState::Running,
            TaskState::CheckpointRequested,
            TaskState::Checkpointing,
            TaskState::Checkpointed,
            TaskState::HandoverRequested,
            TaskState::HandingOver,
            TaskState::Resuming,
            TaskState::Verifying,
            TaskState::Completed,
            TaskState::Blocked,
            TaskState::Failed,
            TaskState::Cancelled,
        ]
    }
}
