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
5. Create a task branch and isolated worktree for a writable worker.
6. Run one bounded worker invocation and normalize its structured events.
7. At a safe boundary, collect Git and command evidence into a checkpoint.
8. Resume the same provider or pass a vendor-neutral bundle to another provider.
9. Independently verify the worktree and acceptance criteria before completion.

The current coordinator dispatches only one writable worker for a task and persists its worktree lease plus changed-file ownership. Parallel routing is limited to policy output; read-only fan-out is not yet dispatched concurrently by the CLI. Reviewers are read-only. Auto-merge, push, and worktree deletion are disabled.

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

Phase 1 establishes durable sessions and daemon lifecycle only. It does not yet
schedule task DAGs, run parallel agents, or replace the existing five-panel TUI.
