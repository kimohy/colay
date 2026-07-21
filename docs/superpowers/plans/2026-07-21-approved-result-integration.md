# Phase 5 implementation plan: approved result integration and recovery

Date: 2026-07-21  
Input boundary: completed Phase 4 tasks with retained worktrees, sealed checkpoints, and passing verification  
Output boundary: approved results applied only to a retained dedicated integration worktree

## Non-negotiable boundary

- Previewing is read-only and cannot create an integration worktree.
- Every preview binds the current approved graph revision, base commit, ordered task/checkpoint/verification identities, exact changed paths, binary diff hashes, and blocker set into one canonical SHA-256 hash.
- Approval is typed authority for one exact preview hash. Chat text such as “yes” has no authority.
- Apply recomputes every source and the preview hash before creating or mutating the dedicated integration worktree.
- Integration order is deterministic dependency order with graph display order as the stable tie-breaker.
- Missing evidence, failed verification, changed/tampered worktrees, stale user base, path overlap, or patch failure stops closed.
- The user's branch, remote refs, task worktrees, and task branches are never merged, pushed, deleted, or rewritten.
- Tests use real temporary Git repositories and compiled fake providers only.

## Task 1: provider-neutral integration contracts

Create domain contracts for integration source evidence, blockers, canonical previews, approvals, application outcomes, and conflict-resolution links. Unit tests cover canonical hash stability, source mutation, deterministic dependency order, overlap/stale/missing-evidence blockers, exact approval validation, and terminal transitions.

Commit: `feat: define approved integration contracts`

## Task 2: schema v8 and durable integration batches

Add `migrations/0008_result_integration.sql`. Rebuild `client_commands` to add `request_integration`, `approve_integration`, and `create_resolution_task`. Add immutable `integration_batches`, ordered `integration_sources`, `integration_approvals`, `integration_applications`, and `integration_resolution_tasks`. Extend migration contracts from v1/v7, preserve event hashes, and require backup-first application.

Commit: `feat: persist exact integration previews and approvals`

## Task 3: read-only preview engine

Add an engine integration planner that validates managed task worktrees, recomputes Git snapshots, verifies checkpoint integrity and latest verification, calculates binary diff hashes, topologically orders graph tasks, and reports overlap, stale base, missing evidence, or source mutation. Previewing performs no Git mutation.

Commit: `feat: build sealed integration previews`

## Task 4: exact approved application

Add a Git integration manager. It revalidates the approved preview, creates one dedicated branch/worktree at the sealed base, applies exact binary patches in preview order, and records the resulting tree/head/status hashes. Any failed patch retains the integration worktree and records `needs_attention`; no partial application is reported as success.

Commit: `feat: apply approved results in an integration worktree`

## Task 5: daemon commands and crash recovery

Process typed preview/approval commands through the daemon. Preview requests are replay-safe after projection reconciliation; application is never blindly replayed. A claimed approval with no terminal application is reconciled from the integration worktree and sealed application journal. Session state follows `running -> integrating -> verifying -> completed` only after exact application verification, otherwise `needs_attention`.

Commit: `feat: orchestrate approved integration and recovery`

## Task 6: text TUI integration approval

Add `/integrate` and `/resolve`. Project an integration card containing preview hash, ordered sources, changed paths, blockers, base, and retained-worktree destination. `/approve` opens the applicable graph or integration confirmation; only `y` submits the exact typed authority. Stale hash refresh closes the overlay. Blocked previews cannot be approved and expose conflict-resolution attention.

Commit: `feat: approve result integration from chat`

## Task 7: conflict-resolution tasks

For an overlap or patch blocker, `/resolve` creates one idempotent generated task bound to the blocked batch and source evidence. It is visible in the session, uses the existing official-CLI execution path in an isolated worktree, and requires a new preview plus final approval before its output can be applied. Creation alone grants no integration authority.

Commit: `feat: create auditable integration resolution tasks`

## Task 8: real Git and restart E2E

In a temporary Git repository, execute disjoint fake-provider tasks, request a preview, reject wrong-hash approval, approve the exact hash, and apply into a dedicated integration worktree. Prove the user branch and task worktrees are unchanged. Add stale-source, overlap, patch-failure, daemon-restart, and no-duplicate-application scenarios.

Commit: `test: prove exact approved result integration`

## Task 9: documentation and audit

Update README and architecture/operations/testing/threat-model/security/migrations/release docs. Create `docs/superpowers/audits/2026-07-21-approved-result-integration-phase5.md` with direct evidence for preview purity, hash binding, source revalidation, deterministic apply, conflict recovery, crash reconciliation, retained worktrees, and the explicit no-merge/no-push/no-cleanup boundary.

Run:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
git diff --check
```

Commit: `docs: describe approved result integration`

## Phase 5 exit criteria

1. Previewing is read-only, canonical, and source-complete.
2. Only an exact current preview hash can authorize application.
3. Apply revalidates every source and uses deterministic dependency order.
4. Success exists only in a dedicated retained integration worktree.
5. Overlap, stale base, evidence loss, source mutation, and patch failure stop closed.
6. Conflict-resolution work is a separately auditable task and requires a new final approval.
7. Crash/restart cannot duplicate or silently continue an ambiguous application.
8. User branch, remotes, task worktrees, and task branches remain unchanged.
9. Full repository gates pass with fake-only provider coverage.
