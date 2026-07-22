# Conversation-First Plan Mode Implementation Plan

> **Execution rule:** Implement each task test-first in this isolated cumulative worktree. Do not invoke real providers in tests, create pre-approval tasks/worktrees, or weaken exact-hash approval.

**Goal:** Make every session-level chat message enter a bounded read-only conversation/interview flow, and create writable task records only after complete requirements, deterministic repository validation, and explicit approval of the latest sealed proposal.

**Architecture:** Add provider-neutral conversation and requirement contracts to `orchestrator-domain`, a strict read-only conversation collector to `orchestrator-engine`, immutable SQLite projections in schema v10, and a daemon conversation processor that can hand a complete requirement revision to the existing graph planner. Extend the existing graph seal with requirement and repository validation evidence. Keep `run --plan-only` as a separate compatibility path.

**Tech stack:** Rust 2024, Tokio, async-trait, Serde/JSON, SHA-256 canonical sealing, SQLite/rusqlite, Ratatui, official CLI adapters, `orchestrator-test-support` fake binaries.

## Global invariants

- Ordinary chat may answer or interview without requiring Git and without creating a task, worktree, attempt, coordinator lease, or worker lease.
- Conversation and planning provider calls are read-only, bounded, redacted before persistence, and provider-neutral outside compatibility/provider crates.
- Requirement revisions are immutable. Any material user message supersedes the current proposal and invalidates its approval authority.
- Git readiness, graph semantics, write scopes, provider/profile eligibility, and verification feasibility pass before an approval card is exposed.
- Missing repository metadata or unborn `HEAD` preserves the conversation and returns actionable validation guidance.
- Approval binds session, requirement revision, graph revision, sealed base commit, validation evidence, proposal hash, approver, and timestamp.
- Existing graph materialization remains atomic and idempotent; scheduler worktrees are created only for approved ready tasks.
- Tests use fake binaries only. No merge, push, or worktree deletion is performed.

## Task 1: Provider-neutral conversation and requirement contracts

**Files:**
- Modify: `crates/orchestrator-domain/src/ids.rs`
- Modify: `crates/orchestrator-domain/src/session.rs`
- Modify: `crates/orchestrator-domain/src/graph.rs`
- Modify: `crates/orchestrator-domain/src/lib.rs`

- [ ] Add failing tests for the four outcomes (`answer_complete`, `more_information_needed`, `worktree_task_candidate`, `needs_attention`), completeness rules, immutable revision sealing, `SessionState::Validating`, and stale approval binding.
- [ ] Add `ConversationAttemptId`, `RequirementRevisionId`, typed conversation payloads/outcomes, requirement snapshots, and deterministic requirement hashes.
- [ ] Bind `TaskGraphProposal` and its canonical proposal hash to a requirement revision plus validation seal/base commit.
- [ ] Add `RequestConversationTurn` and exact approval payload fields without changing free-form chat into authority.
- [ ] Verify: `cargo test -p orchestrator-domain session graph` and domain clippy.

## Task 2: Strict read-only conversation engine boundary

**Files:**
- Create: `crates/orchestrator-engine/src/conversation.rs`
- Modify: `crates/orchestrator-engine/src/lib.rs`
- Modify: `crates/orchestrator-engine/Cargo.toml`

- [ ] Write failing collector tests for strict one-object JSON, identity mismatch, future schema, output/evidence limits, non-read-only requests, all four outcomes, and invalid candidate completeness.
- [ ] Implement `ConversationOrchestrator`, request/response lifecycle types, and `collect_conversation_response`.
- [ ] Preserve unknown usage and provider-neutral engine types.
- [ ] Verify engine conversation tests and clippy.

## Task 3: Immutable schema v10 conversation persistence

**Files:**
- Create: `migrations/0010_conversation_first_planning.sql`
- Create: `crates/orchestrator-state/src/conversations.rs`
- Modify: `crates/orchestrator-state/src/{lib,migrations,client_commands,graphs,sessions,workspace}.rs`
- Modify: `crates/orchestrator-state/tests/migration_contract.rs`
- Add: `crates/orchestrator-state/tests/conversations.rs`

- [ ] Write failing migration and state tests for v9→v10 preservation, strict constraints, immutable requirement rows, attempt recovery, session isolation, candidate invalidation, validation evidence, and zero task/worktree/lease rows before approval.
- [ ] Add `conversation_attempts` and `requirement_revisions`; rebuild the client-command action constraint for `request_conversation_turn`.
- [ ] Associate graph revisions with their requirement revision and sealed validation evidence/base commit.
- [ ] Implement atomic APIs to begin/finish/reconcile conversation attempts and supersede stale candidates on a material user message.
- [ ] Keep schema/event history append-only and expose only redacted projections.
- [ ] Verify state conversation, graph, workspace, and migration tests plus clippy.

## Task 4: Fake and official-CLI conversation adapters

**Files:**
- Create: `crates/orchestrator-cli/src/conversation_orchestrator.rs`
- Modify: `crates/orchestrator-cli/src/{main,task_planner}.rs`
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Add: `crates/orchestrator-cli/tests/chat_conversation_fake_provider.rs`

- [ ] Write failing fake-process tests for separated argv, read-only sandbox, bounded timeout/output, deterministic outcome fixtures, redaction, and zero worktree allocation.
- [ ] Reuse provider selection/capability evidence and official adapters while giving conversation its own strict prompt/response contract.
- [ ] Extend fake binaries with answer, interview, candidate, failure, and delay fixtures; never invoke real inference.
- [ ] Verify the feature-gated fake-provider suite and CLI clippy.

## Task 5: Durable daemon conversation, interview, and validation flow

**Files:**
- Create: `crates/orchestrator-daemon/src/conversation.rs`
- Modify: `crates/orchestrator-daemon/src/{commands,planning,lib,lifecycle}.rs`
- Modify: `crates/orchestrator-process/src/git.rs`

- [ ] Write failing daemon tests proving an ordinary session message automatically queues one idempotent conversation turn and does not create tasks/worktrees/leases.
- [ ] Write outcome tests: answer stays drafting; open questions append an assistant interview and immutable partial revision; complete candidate drafts a graph; provider failure appends redacted attention.
- [ ] Add deterministic validation tests for missing Git, unborn `HEAD`, repository drift, scope/provider/verification failure, and successful approval readiness.
- [ ] Process conversations asynchronously while daemon heartbeat and stop remain responsive; recover stale claimed attempts safely.
- [ ] Hand only complete requirement revisions to the graph planner, transition through `Planning` and `Validating`, and expose approval only after the sealed validation succeeds.
- [ ] Make a later material user message supersede the graph candidate and reject its old approval hash.
- [ ] Verify daemon conversation/planning/lifecycle tests and process Git tests plus clippy.

## Task 6: TUI conversation-first behavior and exact approval

**Files:**
- Modify: `crates/orchestrator-cli/src/chat_tui.rs`
- Modify: `crates/orchestrator-tui/src/chat/{model,state,render}.rs`
- Modify/Add: CLI and TUI chat tests

- [ ] Write failing UI/driver tests showing ordinary messages automatically receive conversation processing, `/plan` remains an explicit compatibility action, and no approval overlay exists before validation.
- [ ] Project requirement revision, open questions, validation evidence, Git root/base commit, risks, write scopes, and verification plan into the approval card.
- [ ] Bind approval dispatch to all latest validated authority fields; reject stale cards after message/repository/proposal changes.
- [ ] Render actionable missing-Git/unborn-HEAD guidance without losing the conversation.
- [ ] Verify TUI and CLI chat suites plus clippy.

## Task 7: End-to-end safety, docs, and Windows/WSL QA

**Files:**
- Add/modify fake-provider end-to-end tests under `crates/orchestrator-cli/tests`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`
- Modify relevant README/operator documentation

- [ ] Test answer-only, multi-turn interview, validated candidate→approval→task→scheduler worktree, stale approval rejection, provider/daemon crash recovery, missing Git, unborn `HEAD`, and repository drift.
- [ ] Assert exact zero pre-approval rows for tasks, task attempts, worktree allocations, coordinator leases, and worker leases.
- [ ] Run targeted tests on Windows and WSL where available; record platform evidence and any environmental limitations.
- [ ] Run `cargo fmt --all -- --check`.
- [ ] Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
- [ ] Run `cargo test --workspace --all-features` and the npm test suite.
- [ ] Update the QA tracker only from observed evidence, commit the cumulative branch, and leave worktrees/branches intact for review.

