# Threat model

## Assets

- source code and uncommitted user changes;
- Enterprise CLI authenticated state;
- task, checkpoint, handover, usage, and audit records;
- administrator routing and quality policy.

## Trust boundaries

Provider output, task text, repository files, Git metadata, usage-probe output, and subprocess output are untrusted inputs. SQLite and artifact integrity are trusted only after schema, path, permission, and hash validation. A provider's success statement is never verification evidence.

## Principal threats and controls

| Threat | Control |
|---|---|
| Shell/argument injection | Executable and argv arrays only; no command strings |
| Credential disclosure | No token/key reads; environment allowlist; output redaction; state-directory access control |
| Workspace escape | Validated repository-relative paths, symlink checks, provider sandbox |
| Partial or conflicting edits | Unique durable writable lease, changed-file ownership, task worktree, safe checkpoint boundary |
| Daemon spoofing or split ownership | Repository-confined SQLite file, protected local permissions, UUID lease, `BEGIN IMMEDIATE`, expiry predicates; PID is never authority |
| Malicious local command replay | Unique idempotency keys, canonical payload/target comparison, atomic single-consumer claim, conservative stale recovery |
| Silent chat retargeting | Navigation and composer target are separate reducer fields; target changes require an explicit picker or one-message mention |
| Secret persistence through chat | Client-side redaction before command persistence, daemon-side redaction before projection, redacted-only TUI mapping |
| Mutation during daemon loss | Heartbeat-derived online/stale/offline state; stale and offline snapshots are explicitly read-only |
| Remote control-plane exposure | No listener, socket, HTTP service, or MCP server; CLI/daemon coordination is SQLite-only |
| Quota misclassification | Source/confidence/unit/scope retained; unknown remains unknown |
| Malicious provider output | Bounded parser, typed lifecycle, opaque optional events, independent verification |
| Audit corruption | Transactional event outbox, ordered fsync, hash chain, startup reconciliation |
| Unsafe upgrade | Exact capability fixtures, safe mode, backup-first migrations, two-phase rollback |
| Supply-chain execution in CI | Exact upstream tag/SHA, no secrets, read-only inspection job, no inference |

Checkpoint diffs contain authoritative source bytes and must be classified like the repository itself. A fail-closed persistence preflight gates checkpoint/handover/reviewer sharing, and completion performs a separate scan. On Windows the application installs and verifies a protected current-user/`SYSTEM`/Administrators DACL for local state; a host administrator remains outside the threat boundary.

The daemon PID is displayed for diagnostics but is not trusted for signaling or
ownership. A local process that can write the database is already inside the
repository-state trust boundary, so state-directory DACL/mode verification and
symlink confinement remain mandatory before every database open. Lease takeover
is time- and UUID-bound, and a stale non-replay-safe stop command requires manual
reconciliation.

Chat history and recent-task reads are bounded to prevent a large local database
from forcing unbounded terminal allocation. Historical message task IDs are
treated as relational identity, not parsed from display text. UI state stores
only a session ID, optional task ID, and timestamp; it contains no message text,
credential, provider session, or authority. `/admin` exits the chat terminal
guard before entering the compatibility dashboard and restores a fresh guard on
return.

The design does not attempt to defend against a host administrator who can replace binaries, read process memory, or rewrite all state and hashes. Enterprise endpoint security and code-signing policy remain required.
