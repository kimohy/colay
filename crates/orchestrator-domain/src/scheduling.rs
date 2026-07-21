use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    GraphRevisionId, ProviderId, RepoPath, SessionId, TaskId, TaskState, repo_paths_overlap,
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceScope {
    pub paths: Vec<RepoPath>,
    pub repository_wide: bool,
}

impl ResourceScope {
    #[must_use]
    pub fn overlaps(&self, other: &Self) -> bool {
        self.repository_wide
            || other.repository_wide
            || self.paths.iter().any(|left| {
                other
                    .paths
                    .iter()
                    .any(|right| repo_paths_overlap(left, right))
            })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DependencyState {
    pub task_id: TaskId,
    pub state: TaskState,
    pub verification_passed: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleCandidate {
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub revision_id: GraphRevisionId,
    pub graph_is_current: bool,
    pub graph_order: u64,
    pub ready_since: DateTime<Utc>,
    pub state: TaskState,
    pub paused: bool,
    pub provider: ProviderId,
    pub dependencies: Vec<DependencyState>,
    pub scope: ResourceScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveResourceClaim {
    pub task_id: TaskId,
    pub scope: ResourceScope,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScheduleCapacity {
    pub global_limit: usize,
    pub active_global: usize,
    pub provider_limits: BTreeMap<ProviderId, usize>,
    pub active_by_provider: BTreeMap<ProviderId, usize>,
}

impl ScheduleCapacity {
    fn provider_limit(&self, provider: ProviderId) -> usize {
        self.provider_limits
            .get(&provider)
            .copied()
            .unwrap_or(self.global_limit)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ReadinessBlocker {
    SupersededGraph,
    TaskNotQueued { state: TaskState },
    Paused,
    DependencyNotVerified { task_id: TaskId, state: TaskState },
    GlobalCapacity,
    ProviderCapacity { provider: ProviderId },
    ResourceConflict { task_id: TaskId },
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum SchedulingError {
    #[error("global scheduling limit must be greater than zero")]
    InvalidGlobalLimit,
    #[error("parallel limit for provider `{0}` must be greater than zero")]
    InvalidProviderLimit(ProviderId),
}

/// Returns the first deterministic reason a candidate cannot run.
///
/// # Errors
///
/// Returns [`SchedulingError`] when global or provider capacity is configured as zero.
pub fn readiness_blocker(
    candidate: &ScheduleCandidate,
    capacity: &ScheduleCapacity,
    active_claims: &[ActiveResourceClaim],
) -> Result<Option<ReadinessBlocker>, SchedulingError> {
    validate_capacity(capacity)?;
    if !candidate.graph_is_current {
        return Ok(Some(ReadinessBlocker::SupersededGraph));
    }
    if candidate.state != TaskState::Queued {
        return Ok(Some(ReadinessBlocker::TaskNotQueued {
            state: candidate.state,
        }));
    }
    if candidate.paused {
        return Ok(Some(ReadinessBlocker::Paused));
    }
    if let Some(dependency) = candidate.dependencies.iter().find(|dependency| {
        dependency.state != TaskState::Completed || !dependency.verification_passed
    }) {
        return Ok(Some(ReadinessBlocker::DependencyNotVerified {
            task_id: dependency.task_id,
            state: dependency.state,
        }));
    }
    if capacity.active_global >= capacity.global_limit {
        return Ok(Some(ReadinessBlocker::GlobalCapacity));
    }
    if capacity
        .active_by_provider
        .get(&candidate.provider)
        .copied()
        .unwrap_or_default()
        >= capacity.provider_limit(candidate.provider)
    {
        return Ok(Some(ReadinessBlocker::ProviderCapacity {
            provider: candidate.provider,
        }));
    }
    if let Some(claim) = active_claims
        .iter()
        .find(|claim| claim.scope.overlaps(&candidate.scope))
    {
        return Ok(Some(ReadinessBlocker::ResourceConflict {
            task_id: claim.task_id,
        }));
    }
    Ok(None)
}

/// Selects candidates in ready-time and graph order while consuming capacity and scopes.
///
/// # Errors
///
/// Returns [`SchedulingError`] when global or provider capacity is configured as zero.
pub fn select_ready_tasks(
    candidates: &[ScheduleCandidate],
    capacity: &ScheduleCapacity,
    active_claims: &[ActiveResourceClaim],
) -> Result<Vec<TaskId>, SchedulingError> {
    validate_capacity(capacity)?;
    let mut ordered = candidates.iter().collect::<Vec<_>>();
    ordered.sort_by_key(|candidate| {
        (
            candidate.ready_since,
            candidate.graph_order,
            candidate.task_id,
        )
    });
    let mut capacity = capacity.clone();
    let mut claims = active_claims.to_vec();
    let mut selected = Vec::new();
    for candidate in ordered {
        if readiness_blocker(candidate, &capacity, &claims)?.is_some() {
            continue;
        }
        capacity.active_global = capacity.active_global.saturating_add(1);
        *capacity
            .active_by_provider
            .entry(candidate.provider)
            .or_default() += 1;
        claims.push(ActiveResourceClaim {
            task_id: candidate.task_id,
            scope: candidate.scope.clone(),
        });
        selected.push(candidate.task_id);
    }
    Ok(selected)
}

fn validate_capacity(capacity: &ScheduleCapacity) -> Result<(), SchedulingError> {
    if capacity.global_limit == 0 {
        return Err(SchedulingError::InvalidGlobalLimit);
    }
    if let Some((provider, _)) = capacity
        .provider_limits
        .iter()
        .find(|(_, limit)| **limit == 0)
    {
        return Err(SchedulingError::InvalidProviderLimit(*provider));
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskInstructionState {
    Queued,
    Applying,
    Applied,
    Rejected,
    Interrupted,
}

impl TaskInstructionState {
    #[must_use]
    pub const fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (
                Self::Queued | Self::Interrupted,
                Self::Applying | Self::Rejected
            ) | (
                Self::Applying,
                Self::Applied | Self::Rejected | Self::Interrupted
            )
        )
    }

    #[must_use]
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Applied | Self::Rejected)
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{TimeZone as _, Utc};

    use super::*;

    fn timestamp(second: u32) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 7, 21, 12, 0, second)
            .single()
            .unwrap_or_default()
    }

    fn scope(path: &str) -> ResourceScope {
        ResourceScope {
            paths: RepoPath::try_from(path).ok().into_iter().collect(),
            repository_wide: false,
        }
    }

    fn candidate(order: u64, provider: ProviderId, path: &str) -> ScheduleCandidate {
        ScheduleCandidate {
            task_id: TaskId::new(),
            session_id: SessionId::new(),
            revision_id: GraphRevisionId::new(),
            graph_is_current: true,
            graph_order: order,
            ready_since: timestamp(1),
            state: TaskState::Queued,
            paused: false,
            provider,
            dependencies: Vec::new(),
            scope: scope(path),
        }
    }

    fn capacity(global: usize, codex: usize) -> ScheduleCapacity {
        ScheduleCapacity {
            global_limit: global,
            active_global: 0,
            provider_limits: BTreeMap::from([(ProviderId::Codex, codex)]),
            active_by_provider: BTreeMap::new(),
        }
    }

    #[test]
    fn selects_ready_tasks_in_fair_graph_order_with_exact_limits() {
        let late_first = candidate(1, ProviderId::Codex, "src/a");
        let second = candidate(2, ProviderId::Codex, "src/b");
        let third = candidate(3, ProviderId::Claude, "src/c");
        let selected = select_ready_tasks(
            &[third.clone(), second.clone(), late_first.clone()],
            &capacity(2, 1),
            &[],
        )
        .unwrap_or_default();
        assert_eq!(selected, vec![late_first.task_id, third.task_id]);
    }

    #[test]
    fn blocks_unverified_dependencies_and_superseded_graphs() {
        let dependency_id = TaskId::new();
        let mut blocked = candidate(1, ProviderId::Codex, "src/a");
        blocked.dependencies.push(DependencyState {
            task_id: dependency_id,
            state: TaskState::Completed,
            verification_passed: false,
        });
        assert_eq!(
            readiness_blocker(&blocked, &capacity(2, 2), &[]),
            Ok(Some(ReadinessBlocker::DependencyNotVerified {
                task_id: dependency_id,
                state: TaskState::Completed,
            }))
        );
        blocked.dependencies.clear();
        blocked.graph_is_current = false;
        assert_eq!(
            readiness_blocker(&blocked, &capacity(2, 2), &[]),
            Ok(Some(ReadinessBlocker::SupersededGraph))
        );
    }

    #[test]
    fn resource_scopes_are_component_aware_and_repository_wide() {
        assert!(!scope("src/a").overlaps(&scope("src/ab")));
        assert!(scope("src/a").overlaps(&scope("src/a/nested")));
        assert!(
            ResourceScope {
                paths: Vec::new(),
                repository_wide: true,
            }
            .overlaps(&scope("docs"))
        );
    }

    #[test]
    fn resource_claim_and_invalid_capacity_block_admission() {
        let candidate = candidate(1, ProviderId::Codex, "src/a/nested");
        let owner = TaskId::new();
        assert_eq!(
            readiness_blocker(
                &candidate,
                &capacity(2, 2),
                &[ActiveResourceClaim {
                    task_id: owner,
                    scope: scope("src/a"),
                }],
            ),
            Ok(Some(ReadinessBlocker::ResourceConflict { task_id: owner }))
        );
        assert_eq!(
            readiness_blocker(&candidate, &capacity(0, 1), &[]),
            Err(SchedulingError::InvalidGlobalLimit)
        );
    }

    #[test]
    fn instruction_transitions_are_one_way_and_recoverable() {
        assert!(TaskInstructionState::Queued.can_transition_to(TaskInstructionState::Applying));
        assert!(
            TaskInstructionState::Applying.can_transition_to(TaskInstructionState::Interrupted)
        );
        assert!(
            TaskInstructionState::Interrupted.can_transition_to(TaskInstructionState::Applying)
        );
        assert!(TaskInstructionState::Applied.is_terminal());
        assert!(!TaskInstructionState::Applied.can_transition_to(TaskInstructionState::Applying));
    }
}
