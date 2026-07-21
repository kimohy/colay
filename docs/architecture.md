# Architecture

## Boundary

Colay is an independent Rust workspace. OpenAI Codex, Claude Code, and Gemini CLI are child processes behind compatibility adapters. No orchestration, usage, routing, persistence, or handover code is added to an upstream provider project.

Codex integration follows its public automation surfaces: [`codex exec`](https://learn.chatgpt.com/docs/non-interactive-mode) with JSONL is the default, while the version-gated [App Server](https://learn.chatgpt.com/docs/app-server) stdio protocol is isolated behind the compatibility adapter. Same-provider resume is opportunistic; cross-provider continuity always uses the vendor-neutral handover bundle.

The dependency direction is intentionally one-way:

```text
domain <- policy/state/process/codex-compat <- providers <- engine/daemon <- cli/tui
```

The domain crate contains no filesystem, database, process, or provider wire types. `codex-compat` converts versioned Codex wire events into domain `WorkerEvent` values. Provider-specific session identifiers may be persisted for same-provider resume, but are never treated as portable state.

## Execution lifecycle

1. Validate config, state schema, repository, and provider capabilities.
2. Assess scope, ambiguity, technical complexity, failure impact, and verification complexity.
3. Collect quota observations without scraping or reading credentials.
4. Exclude ineligible providers, score the rest, and persist all score components.
5. Atomically claim a dependency-ready task and its normalized write scope.
6. Create a task branch and isolated worktree for the writable worker.
7. Run one bounded official-CLI invocation and normalize its structured events.
8. At a safe boundary, collect Git and command evidence into a checkpoint.
9. Independently verify the worktree and acceptance criteria before completion.

The repository daemon schedules the current approved graph with bounded
parallelism. A task is ready only when every dependency is completed and its
latest verification passed. One short `BEGIN IMMEDIATE` transaction enforces
the global limit, the provider-specific limit, unique active task ownership,
and component-aware write-scope exclusion. Disjoint tasks execute concurrently
in isolated Git worktrees; conflicting scopes wait. Claims are renewable and
released with an explicit terminal reason, so a daemon restart cannot silently
duplicate an attempt. Reviewers remain read-only.

Result integration is a second exact-approval boundary. A read-only preview
recomputes managed worktree snapshots and validates checkpoint and verification
identity before sealing the graph revision, base, ordered sources, paths, diff
hashes, and blockers. Typed approval names one exact current preview hash.
Apply revalidates the seal, then creates a dedicated integration worktree and
applies patches in dependency order. User/task worktrees, remotes, and branches
are not mutated. Ambiguous interrupted applications become durable
`needs_attention` records and are never blindly replayed.

## Safe boundaries

The exec-style CLIs do not expose a portable transaction boundary. Proactive handover therefore waits for the managed invocation and all child processes to exit. The App Server transport is implemented, but it also refuses mid-turn handover unless the protocol has reached a terminal, non-mutating boundary. Quota errors and crashes preserve the worktree and trigger a recovery checkpoint only after process termination is confirmed; an unconfirmed termination retains the worker lease until expiry and blocks a replacement writer.

Worker-generated summaries are untrusted claims. The authoritative checkpoint is produced from Git status, a binary diff, bounded untracked-file snapshots, content hashes, and command evidence collected by the engine.

## Usage semantics

Quota values are comparable only within the same provider, quota scope, period, and unit. Token counts from a completed turn are execution ledger observations, not proof of remaining contract quota. Missing values remain unknown. A critical task cannot use an unknown budget as evidence of sufficient headroom.

## Local-only control plane

SQLite is the state projection and event outbox. `events.jsonl` is an append-only, hash-chained audit replica. CLI and TUI controls use an idempotent SQLite command queue; the project exposes no orchestration HTTP service and requires no MCP server.

One repository-local daemon owns a renewable SQLite lease. CLI clients reconnect
through persisted sessions, conversation messages, and idempotent client
commands rather than an in-memory process channel. The daemon opens no socket or
network listener; its only control plane is the repository-confined database.
PID is diagnostic metadata, not ownership authority—the UUID lease and its
unexpired database predicates decide ownership.

The chat TUI reads one bounded projection under a single database lock:
session identity, at most 200 newest messages, the current session graph (or at
most 100 recent repository tasks only when no graph exists), attention state,
and the selected task inspector. The newest-message SQL
query runs in descending order and is reversed before presentation, so long
histories remain bounded while the timeline stays chronological. A v5
`session_workspace_state` row restores the selected task without coupling the
domain model to presentation state.

The chat reducer keeps navigation selection and composer target as separate
state. Only `Ctrl+T`, its target picker, or an explicit one-message mention can
change a target. Returning from the legacy administration dashboard reuses the
same UI session state. A stale/offline daemon produces a readable snapshot with
an explicit mutation guard rather than an optimistic local queue.

Planning is a separate read-only provider boundary. A durable
`request_plan` command binds the newest eligible goal message to a UUID-v7 graph
revision and planning attempt. The official CLI receives separated argv, an
explicit read-only sandbox, bounded output/time, redacted repository evidence,
and a provider-neutral JSON schema. Domain validation deterministically checks
identity, schemas, acyclicity, dependencies, provider/profile eligibility,
parallel width, and independent write-scope overlap before sealing the proposal
and validation summary with SHA-256.

Invalid revisions and redacted errors remain inspectable but carry no approvable
hash. Approval is not inferred from conversation text: a typed command must name
the current revision and exact proposal hash. One short SQLite transaction
recomputes the seal, records the approver, creates ordered session tasks and
relational dependencies, and leaves every task `queued`. The scheduler then
claims ready tasks, creates isolated worktrees, applies ordered durable task
instructions, and runs the configured official CLI through the reusable
executor. Git evidence, checkpoint sealing, persistence preflight, and
independent verification must all succeed before `completed` is visible.
Failed siblings do not cancel an unrelated task.

Task-targeted chat is identity-bearing state, not text parsing. A message for a
current non-terminal graph task atomically appends both the redacted
conversation row and an ordered instruction. Restart reconciliation requeues
an interrupted instruction safely. `@all` expands into individually auditable
instructions for the current graph; it does not grant integration authority.
The existing five-panel dashboard remains available only through `/admin` as a
compatibility adapter.
