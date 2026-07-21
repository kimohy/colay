use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{
    GraphRevisionId, MessageId, ModelProfile, ProviderId, RepoPath, RiskTag, SchemaVersion,
    SessionId, canonical_sha256,
};

pub const TASK_GRAPH_SCHEMA_VERSION: &str = SchemaVersion::V1;
pub const SUPPORTED_TASK_GRAPH_SCHEMA_VERSIONS: &[&str] = &[TASK_GRAPH_SCHEMA_VERSION];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGraphProposal {
    pub schema_version: SchemaVersion,
    pub revision_id: GraphRevisionId,
    pub session_id: SessionId,
    pub goal_message_id: MessageId,
    pub planner_provider: ProviderId,
    pub proposed_at: DateTime<Utc>,
    pub nodes: Vec<TaskGraphNode>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskGraphNode {
    pub key: String,
    pub title: String,
    pub objective: String,
    pub dependencies: Vec<String>,
    pub constraints: Vec<String>,
    pub acceptance_criteria: Vec<String>,
    pub provider: Option<ProviderId>,
    pub profile: ModelProfile,
    pub write_scopes: Vec<RepoPath>,
    pub repository_wide_write_scope: bool,
    pub risks: Vec<RiskTag>,
    pub parallel_safety: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphValidationPolicy {
    pub eligible_providers: BTreeSet<ProviderId>,
    pub eligible_profiles: BTreeSet<ModelProfile>,
    pub max_parallel_workers: usize,
    pub per_provider_limits: BTreeMap<ProviderId, usize>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphValidationSummary {
    pub node_count: usize,
    pub edge_count: usize,
    pub topological_order: Vec<String>,
    pub maximum_parallel_width: usize,
    pub configured_parallel_workers: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ValidatedTaskGraph {
    pub proposal: TaskGraphProposal,
    pub topological_order: Vec<String>,
    pub validation: GraphValidationSummary,
    pub proposal_hash: String,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum GraphValidationError {
    #[error("unsupported task graph schema version `{version}`")]
    UnsupportedSchema { version: String },
    #[error("task graph must contain at least one node")]
    EmptyGraph,
    #[error("max_parallel_workers must be greater than zero")]
    InvalidMaxParallelWorkers,
    #[error("parallel limit for provider `{provider}` must be greater than zero")]
    InvalidProviderLimit { provider: ProviderId },
    #[error("node at index {index} has a blank key")]
    BlankNodeKey { index: usize },
    #[error("duplicate node key `{key}`")]
    DuplicateNodeKey { key: String },
    #[error("node `{node_key}` must declare write scopes or repository-wide scope")]
    MissingWriteScope { node_key: String },
    #[error("node `{node_key}` mixes repository-wide and path write scopes")]
    MixedWriteScope { node_key: String },
    #[error("node `{node_key}` uses ineligible provider `{provider}`")]
    IneligibleProvider {
        node_key: String,
        provider: ProviderId,
    },
    #[error("node `{node_key}` uses ineligible model profile `{profile:?}`")]
    IneligibleProfile {
        node_key: String,
        profile: ModelProfile,
    },
    #[error("node `{node_key}` depends on itself")]
    SelfDependency { node_key: String },
    #[error("node `{node_key}` depends on missing node `{dependency_key}`")]
    MissingDependency {
        node_key: String,
        dependency_key: String,
    },
    #[error("node `{node_key}` repeats dependency `{dependency_key}`")]
    DuplicateDependency {
        node_key: String,
        dependency_key: String,
    },
    #[error("task graph contains cycle {path:?}")]
    Cycle { path: Vec<String> },
    #[error(
        "independent nodes `{left_node_key}` and `{right_node_key}` have overlapping scopes `{left_scope}` and `{right_scope}`"
    )]
    IndependentWriteScopeOverlap {
        left_node_key: String,
        right_node_key: String,
        left_scope: String,
        right_scope: String,
    },
    #[error("cannot seal validated graph: {message}")]
    Integrity { message: String },
}

#[derive(Serialize)]
struct GraphSeal<'a> {
    proposal: &'a TaskGraphProposal,
    validation: &'a GraphValidationSummary,
}

/// Validates a proposed task graph without performing I/O or modifying the proposal.
///
/// # Errors
///
/// Returns the first deterministic policy, node, dependency, cycle, or write-scope error.
pub fn validate_task_graph(
    proposal: TaskGraphProposal,
    policy: &GraphValidationPolicy,
) -> Result<ValidatedTaskGraph, GraphValidationError> {
    validate_policy(policy)?;
    if !proposal
        .schema_version
        .is_supported_by(SUPPORTED_TASK_GRAPH_SCHEMA_VERSIONS)
    {
        return Err(GraphValidationError::UnsupportedSchema {
            version: proposal.schema_version.to_string(),
        });
    }
    if proposal.nodes.is_empty() {
        return Err(GraphValidationError::EmptyGraph);
    }

    let mut node_indexes = BTreeMap::new();
    for (index, node) in proposal.nodes.iter().enumerate() {
        if node.key.trim().is_empty() {
            return Err(GraphValidationError::BlankNodeKey { index });
        }
        if node_indexes.insert(node.key.clone(), index).is_some() {
            return Err(GraphValidationError::DuplicateNodeKey {
                key: node.key.clone(),
            });
        }
        if node.write_scopes.is_empty() && !node.repository_wide_write_scope {
            return Err(GraphValidationError::MissingWriteScope {
                node_key: node.key.clone(),
            });
        }
        if !node.write_scopes.is_empty() && node.repository_wide_write_scope {
            return Err(GraphValidationError::MixedWriteScope {
                node_key: node.key.clone(),
            });
        }
        let provider = node.provider.unwrap_or(proposal.planner_provider);
        if !policy.eligible_providers.contains(&provider) {
            return Err(GraphValidationError::IneligibleProvider {
                node_key: node.key.clone(),
                provider,
            });
        }
        if !policy.eligible_profiles.contains(&node.profile) {
            return Err(GraphValidationError::IneligibleProfile {
                node_key: node.key.clone(),
                profile: node.profile,
            });
        }
    }

    for node in &proposal.nodes {
        let mut dependencies = BTreeSet::new();
        for dependency in &node.dependencies {
            if dependency == &node.key {
                return Err(GraphValidationError::SelfDependency {
                    node_key: node.key.clone(),
                });
            }
            if !node_indexes.contains_key(dependency) {
                return Err(GraphValidationError::MissingDependency {
                    node_key: node.key.clone(),
                    dependency_key: dependency.clone(),
                });
            }
            if !dependencies.insert(dependency) {
                return Err(GraphValidationError::DuplicateDependency {
                    node_key: node.key.clone(),
                    dependency_key: dependency.clone(),
                });
            }
        }
    }

    if let Some(path) = find_cycle(&proposal, &node_indexes) {
        return Err(GraphValidationError::Cycle { path });
    }

    validate_independent_scopes(&proposal, &node_indexes)?;
    let (topological_order, maximum_parallel_width) = topological_summary(&proposal, &node_indexes);
    let validation = GraphValidationSummary {
        node_count: proposal.nodes.len(),
        edge_count: proposal
            .nodes
            .iter()
            .map(|node| node.dependencies.len())
            .sum(),
        topological_order: topological_order.clone(),
        maximum_parallel_width,
        configured_parallel_workers: policy.max_parallel_workers,
    };
    let proposal_hash = task_graph_proposal_hash(&proposal, &validation)?;

    Ok(ValidatedTaskGraph {
        proposal,
        topological_order,
        validation,
        proposal_hash,
    })
}

/// Recomputes the canonical seal used to bind a proposal to its validation summary.
///
/// # Errors
///
/// Returns [`GraphValidationError::Integrity`] when canonical serialization fails.
pub fn task_graph_proposal_hash(
    proposal: &TaskGraphProposal,
    validation: &GraphValidationSummary,
) -> Result<String, GraphValidationError> {
    canonical_sha256(&GraphSeal {
        proposal,
        validation,
    })
    .map_err(|error| GraphValidationError::Integrity {
        message: error.to_string(),
    })
}

fn validate_policy(policy: &GraphValidationPolicy) -> Result<(), GraphValidationError> {
    if policy.max_parallel_workers == 0 {
        return Err(GraphValidationError::InvalidMaxParallelWorkers);
    }
    if let Some((&provider, _)) = policy
        .per_provider_limits
        .iter()
        .find(|(_, limit)| **limit == 0)
    {
        return Err(GraphValidationError::InvalidProviderLimit { provider });
    }
    Ok(())
}

fn find_cycle(
    proposal: &TaskGraphProposal,
    node_indexes: &BTreeMap<String, usize>,
) -> Option<Vec<String>> {
    let mut state = vec![0_u8; proposal.nodes.len()];
    let mut stack = Vec::new();
    for index in 0..proposal.nodes.len() {
        if state[index] == 0
            && let Some(path) = visit_cycle(index, proposal, node_indexes, &mut state, &mut stack)
        {
            return Some(path);
        }
    }
    None
}

fn visit_cycle(
    index: usize,
    proposal: &TaskGraphProposal,
    node_indexes: &BTreeMap<String, usize>,
    state: &mut [u8],
    stack: &mut Vec<usize>,
) -> Option<Vec<String>> {
    state[index] = 1;
    stack.push(index);
    for dependency in &proposal.nodes[index].dependencies {
        let dependency_index = node_indexes[dependency];
        if state[dependency_index] == 0 {
            if let Some(path) = visit_cycle(dependency_index, proposal, node_indexes, state, stack)
            {
                return Some(path);
            }
        } else if state[dependency_index] == 1 {
            let cycle_start = stack
                .iter()
                .position(|candidate| *candidate == dependency_index)?;
            let mut path: Vec<_> = stack[cycle_start..]
                .iter()
                .map(|candidate| proposal.nodes[*candidate].key.clone())
                .collect();
            path.push(proposal.nodes[dependency_index].key.clone());
            return Some(path);
        }
    }
    stack.pop();
    state[index] = 2;
    None
}

fn validate_independent_scopes(
    proposal: &TaskGraphProposal,
    node_indexes: &BTreeMap<String, usize>,
) -> Result<(), GraphValidationError> {
    for left_index in 0..proposal.nodes.len() {
        for right_index in (left_index + 1)..proposal.nodes.len() {
            if transitively_depends(left_index, right_index, proposal, node_indexes)
                || transitively_depends(right_index, left_index, proposal, node_indexes)
            {
                continue;
            }
            let left = &proposal.nodes[left_index];
            let right = &proposal.nodes[right_index];
            if let Some((left_scope, right_scope)) = overlapping_scope(left, right) {
                return Err(GraphValidationError::IndependentWriteScopeOverlap {
                    left_node_key: left.key.clone(),
                    right_node_key: right.key.clone(),
                    left_scope,
                    right_scope,
                });
            }
        }
    }
    Ok(())
}

fn transitively_depends(
    start: usize,
    target: usize,
    proposal: &TaskGraphProposal,
    node_indexes: &BTreeMap<String, usize>,
) -> bool {
    let mut pending = vec![start];
    let mut visited = BTreeSet::new();
    while let Some(index) = pending.pop() {
        if !visited.insert(index) {
            continue;
        }
        for dependency in &proposal.nodes[index].dependencies {
            let dependency_index = node_indexes[dependency];
            if dependency_index == target {
                return true;
            }
            pending.push(dependency_index);
        }
    }
    false
}

fn overlapping_scope(left: &TaskGraphNode, right: &TaskGraphNode) -> Option<(String, String)> {
    if left.repository_wide_write_scope || right.repository_wide_write_scope {
        return Some((
            scope_label(left.repository_wide_write_scope),
            scope_label(right.repository_wide_write_scope),
        ));
    }
    for left_scope in &left.write_scopes {
        for right_scope in &right.write_scopes {
            if left_scope.as_path().starts_with(right_scope.as_path())
                || right_scope.as_path().starts_with(left_scope.as_path())
            {
                return Some((left_scope.to_string(), right_scope.to_string()));
            }
        }
    }
    None
}

fn scope_label(repository_wide: bool) -> String {
    if repository_wide {
        "<repository>".to_owned()
    } else {
        "<path-scoped>".to_owned()
    }
}

fn topological_summary(
    proposal: &TaskGraphProposal,
    node_indexes: &BTreeMap<String, usize>,
) -> (Vec<String>, usize) {
    let mut remaining_dependencies: Vec<_> = proposal
        .nodes
        .iter()
        .map(|node| node.dependencies.len())
        .collect();
    let mut ready: Vec<_> = remaining_dependencies
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect();
    let mut order = Vec::with_capacity(proposal.nodes.len());
    let mut maximum_parallel_width = 0;

    while !ready.is_empty() {
        maximum_parallel_width = maximum_parallel_width.max(ready.len());
        let current = std::mem::take(&mut ready);
        for index in &current {
            order.push(proposal.nodes[*index].key.clone());
        }
        for completed in current {
            for (candidate, node) in proposal.nodes.iter().enumerate() {
                if node
                    .dependencies
                    .iter()
                    .any(|dependency| node_indexes[dependency] == completed)
                {
                    remaining_dependencies[candidate] -= 1;
                    if remaining_dependencies[candidate] == 0 {
                        ready.push(candidate);
                    }
                }
            }
        }
        ready.sort_unstable();
        ready.dedup();
    }
    (order, maximum_parallel_width)
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use chrono::{TimeZone, Utc};

    use crate::{
        GraphRevisionId, GraphValidationError, GraphValidationPolicy, MessageId, ModelProfile,
        ProviderId, RepoPath, RiskTag, SchemaVersion, SessionId, TaskGraphNode, TaskGraphProposal,
        validate_task_graph,
    };

    fn node(key: &str, dependencies: &[&str], scopes: &[&str]) -> TaskGraphNode {
        TaskGraphNode {
            key: key.to_owned(),
            title: format!("{key} title"),
            objective: format!("Implement {key}"),
            dependencies: dependencies
                .iter()
                .map(|value| (*value).to_owned())
                .collect(),
            constraints: vec!["remain local".to_owned()],
            acceptance_criteria: vec![format!("{key} passes")],
            provider: Some(ProviderId::Codex),
            profile: ModelProfile::Standard,
            write_scopes: scopes
                .iter()
                .map(|path| RepoPath::try_from(*path).expect("valid fixture path"))
                .collect(),
            repository_wide_write_scope: false,
            risks: vec![RiskTag::Concurrency],
            parallel_safety: "isolated write scope".to_owned(),
        }
    }

    fn proposal(nodes: Vec<TaskGraphNode>) -> TaskGraphProposal {
        TaskGraphProposal {
            schema_version: SchemaVersion::v1(),
            revision_id: GraphRevisionId::new(),
            session_id: SessionId::new(),
            goal_message_id: MessageId::new(),
            planner_provider: ProviderId::Codex,
            proposed_at: Utc
                .with_ymd_and_hms(2026, 7, 21, 0, 0, 0)
                .single()
                .expect("valid fixture timestamp"),
            nodes,
        }
    }

    fn policy() -> GraphValidationPolicy {
        GraphValidationPolicy {
            eligible_providers: BTreeSet::from([ProviderId::Codex, ProviderId::Gemini]),
            eligible_profiles: BTreeSet::from([ModelProfile::Standard, ModelProfile::Premium]),
            max_parallel_workers: 4,
            per_provider_limits: BTreeMap::from([(ProviderId::Codex, 2)]),
        }
    }

    #[test]
    fn validates_a_diamond_dag_deterministically() {
        let graph = validate_task_graph(
            proposal(vec![
                node("root", &[], &["src/root"]),
                node("left", &["root"], &["src/left"]),
                node("right", &["root"], &["src/right"]),
                node("join", &["left", "right"], &["src/join"]),
            ]),
            &policy(),
        )
        .expect("diamond should validate");

        assert_eq!(graph.topological_order, ["root", "left", "right", "join"]);
        assert_eq!(graph.validation.node_count, 4);
        assert_eq!(graph.validation.edge_count, 4);
        assert_eq!(graph.validation.maximum_parallel_width, 2);
        assert_eq!(graph.proposal_hash.len(), 64);
    }

    #[test]
    fn rejects_blank_and_duplicate_keys_in_input_order() {
        let blank = validate_task_graph(proposal(vec![node(" ", &[], &["src/a"])]), &policy());
        assert_eq!(blank, Err(GraphValidationError::BlankNodeKey { index: 0 }));

        let duplicate = validate_task_graph(
            proposal(vec![
                node("same", &[], &["src/a"]),
                node("same", &[], &["src/b"]),
            ]),
            &policy(),
        );
        assert_eq!(
            duplicate,
            Err(GraphValidationError::DuplicateNodeKey {
                key: "same".to_owned(),
            })
        );
    }

    #[test]
    fn rejects_missing_dependency_and_self_edge() {
        let missing = validate_task_graph(
            proposal(vec![node("api", &["missing"], &["src/api"])]),
            &policy(),
        );
        assert_eq!(
            missing,
            Err(GraphValidationError::MissingDependency {
                node_key: "api".to_owned(),
                dependency_key: "missing".to_owned(),
            })
        );

        let self_edge = validate_task_graph(
            proposal(vec![node("api", &["api"], &["src/api"])]),
            &policy(),
        );
        assert_eq!(
            self_edge,
            Err(GraphValidationError::SelfDependency {
                node_key: "api".to_owned(),
            })
        );
    }

    #[test]
    fn reports_a_deterministic_cycle_path() {
        let result = validate_task_graph(
            proposal(vec![
                node("a", &["c"], &["src/a"]),
                node("b", &["a"], &["src/b"]),
                node("c", &["b"], &["src/c"]),
            ]),
            &policy(),
        );
        assert_eq!(
            result,
            Err(GraphValidationError::Cycle {
                path: vec![
                    "a".to_owned(),
                    "c".to_owned(),
                    "b".to_owned(),
                    "a".to_owned()
                ],
            })
        );
    }

    #[test]
    fn requires_an_explicit_writable_scope() {
        let result = validate_task_graph(proposal(vec![node("api", &[], &[])]), &policy());
        assert_eq!(
            result,
            Err(GraphValidationError::MissingWriteScope {
                node_key: "api".to_owned(),
            })
        );

        let mut repository_wide = node("api", &[], &[]);
        repository_wide.repository_wide_write_scope = true;
        assert!(validate_task_graph(proposal(vec![repository_wide]), &policy()).is_ok());
    }

    #[test]
    fn rejects_ineligible_provider_and_profile() {
        let mut provider = node("api", &[], &["src/api"]);
        provider.provider = Some(ProviderId::Claude);
        assert_eq!(
            validate_task_graph(proposal(vec![provider]), &policy()),
            Err(GraphValidationError::IneligibleProvider {
                node_key: "api".to_owned(),
                provider: ProviderId::Claude,
            })
        );

        let mut profile = node("api", &[], &["src/api"]);
        profile.profile = ModelProfile::Economy;
        assert_eq!(
            validate_task_graph(proposal(vec![profile]), &policy()),
            Err(GraphValidationError::IneligibleProfile {
                node_key: "api".to_owned(),
                profile: ModelProfile::Economy,
            })
        );
    }

    #[test]
    fn rejects_invalid_concurrency_policy() {
        let mut invalid = policy();
        invalid.max_parallel_workers = 0;
        assert_eq!(
            validate_task_graph(proposal(vec![node("api", &[], &["src/api"])]), &invalid),
            Err(GraphValidationError::InvalidMaxParallelWorkers)
        );

        let mut invalid_limit = policy();
        invalid_limit
            .per_provider_limits
            .insert(ProviderId::Codex, 0);
        assert_eq!(
            validate_task_graph(
                proposal(vec![node("api", &[], &["src/api"])]),
                &invalid_limit,
            ),
            Err(GraphValidationError::InvalidProviderLimit {
                provider: ProviderId::Codex,
            })
        );
    }

    #[test]
    fn independent_overlapping_scopes_are_rejected() {
        let result = validate_task_graph(
            proposal(vec![
                node("api", &[], &["src/api"]),
                node("tests", &[], &["src/api/tests"]),
            ]),
            &policy(),
        );
        assert_eq!(
            result,
            Err(GraphValidationError::IndependentWriteScopeOverlap {
                left_node_key: "api".to_owned(),
                right_node_key: "tests".to_owned(),
                left_scope: "src/api".to_owned(),
                right_scope: "src/api/tests".to_owned(),
            })
        );
    }

    #[test]
    fn path_prefix_comparison_is_component_aware() {
        assert!(
            validate_task_graph(
                proposal(vec![
                    node("api", &[], &["src/api"]),
                    node("api_v2", &[], &["src/api-v2"]),
                ]),
                &policy(),
            )
            .is_ok()
        );
    }

    #[test]
    fn dependency_ordered_scope_reuse_is_allowed() {
        assert!(
            validate_task_graph(
                proposal(vec![
                    node("api", &[], &["src/api"]),
                    node("tests", &["api"], &["src/api/tests"]),
                ]),
                &policy(),
            )
            .is_ok()
        );
    }
}
