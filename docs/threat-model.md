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
| Quota misclassification | Source/confidence/unit/scope retained; unknown remains unknown |
| Malicious provider output | Bounded parser, typed lifecycle, opaque optional events, independent verification |
| Audit corruption | Transactional event outbox, ordered fsync, hash chain, startup reconciliation |
| Unsafe upgrade | Exact capability fixtures, safe mode, backup-first migrations, two-phase rollback |
| Supply-chain execution in CI | Exact upstream tag/SHA, no secrets, read-only inspection job, no inference |

Checkpoint diffs contain authoritative source bytes and must be classified like the repository itself. A fail-closed persistence preflight gates checkpoint/handover/reviewer sharing, and completion performs a separate scan. On Windows the application installs and verifies a protected current-user/`SYSTEM`/Administrators DACL for local state; a host administrator remains outside the threat boundary.

The design does not attempt to defend against a host administrator who can replace binaries, read process memory, or rewrite all state and hashes. Enterprise endpoint security and code-signing policy remain required.
