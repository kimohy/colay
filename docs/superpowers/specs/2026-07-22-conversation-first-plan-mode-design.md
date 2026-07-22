# Conversation-First Plan Mode Design

Date: 2026-07-22
Status: Approved for implementation

## Context

Colay currently has two planning concepts with different limitations:

- `colay run --plan-only` creates a durable task and records static assessment
  and routing, but does not invoke a provider; and
- the TUI `/plan` command invokes a read-only planner provider and withholds
  writable tasks until graph approval, but it converts the newest user goal
  directly into a task graph rather than conducting a general interview.

Neither behavior matches the desired product model. A user must be able to ask
a question, receive a provider-backed answer, clarify an ambiguous request,
and validate a proposed course of action before Colay creates any task or
worktree. Worktree execution begins only after the user gives final approval to
the exact validated proposal.

This design refines the approved chat orchestration design from 2026-07-20. It
keeps the existing durable session, graph revision, explicit approval,
scheduler, worktree, and integration boundaries while adding a pre-task
conversation and interview layer.

## Goal

Make the durable TUI session the primary conversation-first entry point:

```text
question or request
  -> read-only provider answer and interview
  -> immutable requirement snapshot
  -> task graph draft when implementation is needed
  -> deterministic and read-only validation
  -> final approval bound to the exact validated hash
  -> atomic task materialization
  -> worktree creation and provider execution
```

A question that can be answered without repository changes ends in the session
without creating a task. An implementation request remains a session-only
interview until its requirements and validation evidence are ready for final
approval.

## Product Decisions

- Starting a conversation never creates a row in `tasks`.
- The orchestrator uses an approved official provider CLI in read-only mode to
  answer questions and conduct the interview.
- The interview may take multiple turns. It captures objective, scope,
  exclusions, constraints, acceptance criteria, verification, risks, and open
  questions.
- The orchestrator may judge that worktree work is required and prepare a
  proposal, but that judgment alone never authorizes task creation.
- Validation occurs before final approval and includes Git readiness, graph
  correctness, write scopes, provider capability, and verification feasibility.
- Final approval is a typed action bound to the latest validated revision and
  proposal hash. Free-form chat text such as `yes` is never approval.
- Only final approval materializes tasks and dependencies. Worktrees are
  created later when the scheduler claims a dependency-ready task.
- Any interview answer, scope change, repository change relevant to validation,
  or proposal change invalidates the prior approval candidate.
- A missing Git repository or unborn `HEAD` does not prevent conversation. It
  prevents promotion to an approvable worktree plan and returns actionable
  guidance while retaining the session.

## Terminology

- **Conversation turn:** one user message plus a bounded read-only orchestrator
  provider response.
- **Interview:** one or more conversation turns that remove material ambiguity
  and build a requirement snapshot.
- **Requirement snapshot:** an immutable, redacted, versioned summary of the
  current objective, boundaries, acceptance criteria, and open questions.
- **Task graph draft:** a versioned proposal derived from a ready requirement
  snapshot. It is not a set of durable tasks.
- **Validated proposal:** a graph draft plus deterministic validation evidence
  sealed to one hash.
- **Final approval:** explicit user authority for one validated proposal hash.
- **Materialization:** the atomic creation of durable task and dependency rows
  from an approved proposal.

The current `run --plan-only` remains a static assessment compatibility command
in the initial delivery. Documentation must not call it the conversation-first
mode. Deprecating or redefining that CLI contract is outside this design so
existing noninteractive automation is not silently changed.

## Considered Approaches

### Extend durable TUI sessions and graph approval — selected

Add provider-backed conversation turns and immutable requirement snapshots to
the existing session pipeline. Reuse graph revisions, deterministic graph
validation, hash-bound approval, task materialization, and scheduling.

This preserves the existing audit and safety model, avoids a second state
machine, and minimizes compatibility risk.

### Add a separate `colay interview` wizard

A dedicated command would make the interview explicit but would duplicate
session persistence, provider invocation, validation, approval, and recovery
behavior. TUI and CLI interviews could diverge.

### Redefine `colay run`

Making every `run` invocation interactive would give one obvious entry point,
but it would break noninteractive callers and make command completion semantics
ambiguous. This is not selected.

## User Experience

The composer targets `orchestrator` before tasks exist. Every ordinary session
message creates a durable conversation command. The daemon invokes the selected
read-only provider and displays its response in the conversation timeline.

Each response has one provider-neutral outcome:

- `answer_complete`: the question is answered; the session remains available
  for another question and no plan is created;
- `more_information_needed`: the response contains focused follow-up questions
  and an updated requirement snapshot;
- `worktree_task_candidate`: requirements are sufficiently concrete to draft a
  graph; or
- `needs_attention`: the provider or deterministic boundary cannot safely
  continue and the user receives a specific recovery message.

When a worktree candidate is ready, Colay shows the current requirement summary
before graph drafting. The user may answer another question or request a
revision at any time. A new material answer creates a new immutable requirement
revision and invalidates downstream graph and approval candidates.

The validated approval card shows:

- objective, in-scope and out-of-scope work;
- task nodes, dependencies, write scopes, and proposed provider profiles;
- acceptance criteria and verification commands;
- Git root and sealed base commit;
- risks, destructive or scope-expanding actions, and required approvals;
- validation results; and
- the exact proposal hash.

Only the existing explicit confirmation overlay can submit final approval.
Typing affirmative prose in the composer remains an ordinary interview message.

## State Model

The persisted session state remains separate from task state. Existing
serialized values are preserved where possible:

```text
Drafting (interviewing)
  -> Planning (requirement snapshot and graph draft)
  -> Validating
  -> AwaitingApproval
  -> Running
  -> Integrating
  -> Verifying
  -> Completed
```

`Validating` is an additive session state. `AwaitingApproval` now means final
approval of a validated proposal, not approval of an unverified draft.

Alternate transitions are:

- `Drafting -> Completed` for a session explicitly closed after an
  `answer_complete` result;
- `Planning|Validating -> Drafting` when more information is required;
- `AwaitingApproval -> Drafting` when the user changes requirements;
- `AwaitingApproval -> Validating` when repository or provider evidence becomes
  stale;
- any nonterminal pre-task state to `NeedsAttention` on a fail-closed error; and
- `NeedsAttention -> Drafting` after the user resolves the reported condition.

No pre-approval state transition creates `TaskState` records. `Running` is
entered only after approved materialization succeeds.

## Components and Boundaries

### Conversation Orchestrator

Add a provider-neutral conversation contract to `orchestrator-engine`. Its
request includes the redacted session history window, current requirement
snapshot, repository summary when available, provider capability policy, and a
read-only sandbox requirement. Its structured response contains display text,
the interview outcome, requirement updates, follow-up questions, and evidence
references.

Provider-specific wire formats remain in provider or compatibility crates. The
domain stores only versioned vendor-neutral results. Every invocation uses the
official configured CLI with separated executable and arguments, bounded
output, redaction, timeout, cancellation, and confirmed process-tree handling.

### Interview Manager

The daemon owns interview command claiming, requirement revision construction,
session transitions, and stale-command recovery. It never writes repository
files and never acquires a task coordinator or worker lease.

The manager deterministically rejects a provider response that attempts to
skip required questions, widens scope without user evidence, embeds an approval
decision, or returns an unsupported outcome. Rejected responses become bounded
redacted warnings and leave the session recoverable.

### Graph Planner

The existing read-only graph planner consumes only a requirement snapshot with
no material open questions. It produces the existing vendor-neutral task graph
proposal. A graph revision records the source requirement revision so a newer
interview answer makes the graph stale.

### Validation Pipeline

Validation is split into deterministic gates:

1. requirement completeness and acceptance-criteria presence;
2. task graph schema, dependency, and cycle validation;
3. normalized write scopes and concurrency overlap checks;
4. trusted repository root, supported worktree, and valid `HEAD^{commit}`;
5. eligible provider, model profile, sandbox, and configured concurrency;
6. verification command safety and feasibility; and
7. approval requirements for destructive, repository-wide, or scope-expanding
   work.

Pre-approval validation does not execute proposed build, test, formatter, or
other repository commands. It verifies that commands are represented as
separated executable and arguments, resolve within the approved policy, target
the intended repository, and can be scheduled in the declared task sandbox.
Actual verification commands run only inside an approved task worktree.

Read-only provider critique may add risk evidence, but it cannot override a
deterministic validation failure. Missing usage remains unknown, and validation
never compares raw quota units across providers.

The successful validation artifact seals the requirement revision, graph
revision, repository root, base commit, normalized scopes, provider policy,
verification plan, and validation results into one proposal hash.

### Approval and Materializer

Final approval binds action type, session, requirement revision, graph
revision, proposal hash, approval identity, and timestamp. Approval is rejected
if any bound input is no longer current.

Materialization runs once under an idempotent durable command and one database
transaction. It creates queued task envelopes, session-task links,
dependencies, approval records, and append-only audit events. A complete replay
returns the existing result; an ambiguous partial outcome fails closed for
reconciliation.

Materialization does not create worktrees or invoke providers. The scheduler
creates a worktree only after dependencies, approval, resource claims,
provider eligibility, and concurrency are ready.

## Persistence

Add immutable pre-task records rather than overloading task attempts:

- `conversation_attempts`: session-level read-only provider attempt metadata,
  terminal outcome, bounded result reference, and timestamps;
- `requirement_revisions`: immutable redacted requirement snapshots, source
  message range, completeness result, open questions, and content hash; and
- validation evidence associated with the existing graph revision and approval
  proposal hash.

Existing `conversation_messages`, `client_commands`, `graph_revisions`,
`planning_attempts`, graph approval, `sessions`, and `session_tasks` remain the
authoritative surrounding records. `task_attempts`, `coordinator_leases`,
`worker_leases`, `worktrees`, and `tasks` must have no row for a session before
final approval materialization.

Schema changes are sequential and versioned. New events preserve append-only
ordering, redaction, and historical event-hash verification. Recovery never
deletes or rewrites prior interview revisions.

## Command and Data Flow

```text
user message
  -> idempotent append-message command
  -> durable final user message
  -> idempotent request-conversation-turn command
  -> read-only provider attempt
  -> orchestrator response + interview outcome
  -> requirement revision when knowledge changes
  -> graph draft when outcome is worktree_task_candidate
  -> deterministic validation
  -> immutable approval card and hash
  -> explicit final approval command
  -> atomic task materialization
  -> normal scheduler and isolated worktree execution
```

Appending a message and invoking a provider are separate durable commands so a
daemon crash cannot duplicate the user message or silently lose whether a
provider turn was attempted. Completed commands are idempotent; ambiguous
provider attempts become interrupted and require retry rather than blind
replay.

## Error Handling and Recovery

- **Provider unavailable or terminal error:** finalize the conversation attempt,
  append a redacted warning, leave the session pre-task, and release all
  session-level execution ownership. No task lease exists to strand.
- **Provider output malformed:** reject the structured result, preserve bounded
  diagnostic evidence, and return `NeedsAttention` without adopting requirement
  changes.
- **Git repository missing or unborn:** continue the interview, mark validation
  failed with a specific remedy, and do not expose final approval.
- **Repository changes after validation:** mark the proposal stale and validate
  a new revision before approval.
- **Provider capability or profile changes:** invalidate provider-bound
  validation evidence and rerun validation.
- **Daemon crash during a turn:** mark partial output interrupted. Requeue only
  when no provider process ownership is ambiguous.
- **Daemon crash during approval:** reconcile the exact proposal hash and
  materialization transaction before retry.
- **Audit, migration, or redaction failure:** enter safe mode and disable final
  approval and materialization.

## Security and Safety

Conversation and planning providers are always read-only. They receive only
redacted, bounded context and never access credentials, authentication stores,
unofficial endpoints, or usage pages. No default telemetry or network service
is added.

Free-form provider output cannot authorize execution, widen write scope,
approve a graph, or bypass deterministic validation. Writable changes remain
isolated in task worktrees, reviewers remain read-only, and integration still
requires its existing separate approval. Colay does not automatically merge,
push, or delete worktrees.

## Compatibility and Rollout

The TUI becomes the primary conversation-first surface. Existing explicit
administration and read-only CLI commands retain their behavior. `colay run`
and `run --plan-only` remain compatibility paths during the initial rollout and
must clearly identify themselves as direct-task execution and static task
assessment respectively.

Delivery is staged:

1. add durable conversation attempts and provider-backed ordinary session
   replies, without automatic graph drafting;
2. add immutable requirement revisions and interview outcomes;
3. bind graph drafting and validation to a ready requirement revision;
4. make final approval atomically materialize tasks; and
5. update TUI presentation, recovery diagnostics, and compatibility wording.

Each stage is independently releasable and preserves the explicit approval
boundary.

## Testing

All tests use `orchestrator-test-support` fake provider binaries. Tests and CI
must never invoke real Codex, Claude, Gemini, or Agy inference.

Unit coverage includes:

- conversation outcome parsing and validation;
- requirement revision hashing, completeness, and invalidation;
- additive session transitions including `Validating`;
- validation gates and proposal-hash determinism;
- final approval binding and stale approval rejection; and
- atomic, idempotent task materialization.

State and daemon integration coverage includes:

- a simple question producing an answer with zero task, worktree, attempt, and
  task-lease rows;
- a multi-turn interview creating immutable requirement revisions;
- requirement changes invalidating graph and approval candidates;
- missing Git and unborn `HEAD` preserving the conversation while blocking
  final approval;
- provider error, timeout, cancellation, and daemon restart finalizing the
  conversation attempt without stranded task leases;
- exact approval producing tasks and dependencies once; and
- scheduler worktree creation only after approved materialization.

End-to-end fake-provider coverage includes:

```text
question -> answer_complete -> no task
ambiguous request -> interview -> revision -> validated proposal
validated proposal -> explicit approval -> task -> worktree -> fake worker
proposal mutation -> stale approval rejection
```

Before completion, run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Acceptance Criteria

1. An ordinary question receives a provider-backed answer without creating a
   task, worktree, task attempt, coordinator lease, or worker lease.
2. An ambiguous implementation request remains in a multi-turn interview until
   material open questions are resolved.
3. Every graph proposal identifies the immutable requirement revision from
   which it was derived.
4. Git and deterministic validation complete before final approval is enabled.
5. Missing Git or an unborn `HEAD` produces actionable validation guidance and
   never creates a task.
6. Only an explicit approval bound to the latest validated proposal hash can
   materialize tasks.
7. Materialization is atomic and idempotent and creates no worktree itself.
8. The scheduler creates isolated worktrees only for approved, ready tasks.
9. Provider and daemon failures before approval cannot leave a running task or
   task lease.
10. Historical task, session, event, and approval records remain readable and
    hash-verifiable.
11. The complete workflow is reproducible with fake providers and no real
    inference.
12. Repository formatting, lint, and workspace tests pass.

## Documentation Impact

Implementation updates must revise README command semantics, the operations
guide, architecture and state diagrams, migrations, security boundaries,
testing policy, and release notes. The error tracker must retain the distinction
between static `run --plan-only` and conversation-first plan mode until the
legacy name is migrated or removed through a separately approved compatibility
change.
