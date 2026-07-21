# Phase 5 audit: approved result integration and recovery

Date: 2026-07-21

## Decision

Phase 5 is accepted. Colay can produce a read-only, canonical integration
preview from completed verified task worktrees and, only after typed approval
of the exact current preview hash, apply those sources to a dedicated retained
integration worktree. No implementation path merges, pushes, publishes, cleans
up, or mutates the user's worktree.

## Direct evidence

| Property | Evidence |
|---|---|
| Preview purity | `GitIntegrationManager::preview` reads managed snapshots, checkpoints, and verification evidence without creating an integration branch or worktree. Engine and daemon real-Git tests assert the destination is absent after preview. |
| Canonical authority | `IntegrationPreview::seal` hashes schema, batch/session/graph identity, base, ordered sources, and blockers. `IntegrationApproval::validate_for` accepts only the same approvable batch/hash. |
| Source revalidation | Apply reruns preview construction and requires an exact preview match before worktree creation. Changed files, missing/tampered checkpoints, failed verification, stale base, or changed diff hash stop closed. |
| Deterministic application | Candidates are dependency ordered with graph order and task ID as stable tie-breakers. Exact binary patches are applied only in `.colay/integration/<batch-id>` on `orchestrator/integration-<batch-id>`. |
| TUI approval | `/integrate` submits a typed preview request. The card shows base, hash, destination, ordered task/checkpoint/verification/diff identities, paths, and blockers. `/approve` requires `y`; refresh closes a stale overlay. Chat text has no authority. |
| Conflict recovery | Blocked or failed batches cannot be approved. For path overlap or failed application, `/resolve` creates one idempotent queued task linked to the batch, with source tasks as dependencies and an audited `TaskCreated` event. Missing evidence or failed verification instead requires source remediation. A resolution output is eligible only for a new preview and approval. |
| Crash reconciliation | Daemon startup requeues replay-safe commands. `approved`/`applying` batches without a terminal application become `needs_attention`; an applying journal becomes `interrupted`. Application is not blindly replayed. An already recorded applied batch can finish deterministic session-state reconciliation. |
| Retention boundary | Real-Git E2E proves the user files remain at the base, both source task worktrees still exist, and only the integration destination contains the combined result. |
| Persistence | Schema v8 stores immutable batches and sources, exact approvals, application journals, and resolution links. Migration tests cover sequential upgrade, backup, row preservation, foreign keys, checksums, and future-schema rejection. |

## Session lifecycle

An approvable preview leaves the running session unchanged. Exact approval
transitions `running|needs_attention -> integrating`; a successful recorded
application transitions through `verifying -> completed`. A blocked preview,
failed apply, or interrupted startup reconciliation ends in `needs_attention`.
Creating a valid resolution task returns the session to `running` so the normal
approved-graph scheduler can execute it.

## Verification

The final gates passed:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git diff --check
```

The full Rust suite includes the real temporary-Git daemon E2E
`typed_preview_and_approval_apply_only_to_dedicated_integration_worktree`, plus
domain, engine, state, migration, TUI reducer/render, typed-command, scheduling,
parallel-process, reconnect, and restart coverage. No real provider credentials,
network inference, merge, push, or publication are used.

## Residual boundary

The integration worktree is a retained review artifact, not the user's branch.
Colay does not merge or cherry-pick it into the active branch, push any ref,
publish an artifact, delete a task/integration worktree, or implement `/retry`.
Those actions require a separate future authority and lifecycle design.
