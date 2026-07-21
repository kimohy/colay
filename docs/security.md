# Security model

## Prohibited behavior

The project does not rotate accounts or identities, evade quotas, scrape provider usage pages, call unofficial provider endpoints, purchase credits, extract authentication material, or emit external telemetry by default. It reuses only the login state already managed by each approved Enterprise CLI.

## Process boundary

- Executable and argument arrays are passed directly to `tokio::process::Command`; user text is sent on stdin and is never interpolated into a shell command.
- Config validation rejects empty/NUL-containing executable and argument values. Executables may be administrator-configured paths or names resolved by the operating system `PATH`; production execution does not currently pin their file digest or canonical path.
- Each child receives an allowlisted environment. Token/API-key variables are excluded; the CLI-specific home-directory variables needed to reuse an approved login may be inherited.
- Stdout and stderr are separate, bounded streams. The provider runtime exposes redacted frames/results to higher layers; unredacted capture bytes remain transient inside the process layer.
- Timeout and cancellation terminate the Unix process group or use the canonical fixed-System32 `taskkill.exe /T /F` process-tree fallback on Windows, then wait for and reap the managed child with a bounded deadline. A reaped direct child is not treated as sufficient when descendant termination cannot be confirmed: the attempt is recorded as `termination_unconfirmed`, the task becomes `Blocked`, and the writable lease remains held until expiry.
- Writable providers use task worktrees. Review requests use provider plan/read-only modes and verification rejects a reviewer that changed the snapshot.
- Permission-bypass flags such as `--yolo` or `--dangerously-skip-permissions` are not generated.

## Filesystem and state

Persisted repository paths reject absolute paths, `..`, NUL, and Windows prefixes. State/artifact operations reject symlink components, and worktree evidence validates paths beneath the managed root.

Configuration may be layered from personal defaults or environment-selected files, but effective state paths are constrained beneath the repository. Automatic discovery of both current and legacy repository configuration fails closed unless an explicit `--config` selects one. Normal runtime commands fail rather than fall back when an explicitly selected `$COLAY_CONFIG` or `--config` file is missing; `init` treats a missing explicit selector as the destination for a new minimal override.

On Unix, state directories/files are set to `0700`/`0600`. On Windows, state creation removes inheritance and broad grants, then verifies a protected DACL restricted to the current SID, `SYSTEM`, and built-in Administrators. The multi-command ACL mutation is serialized within the process so concurrent state opens cannot observe an intentional intermediate descriptor. The implementation invokes only canonical System32 `whoami.exe`/`icacls.exe` with separated arguments, an empty environment except the Windows root, bounded output, and a timeout; any unverifiable ACL fails closed.

`--task-file` is read only when it is a non-symlink regular file inside the current repository, no larger than 1 MiB, and already has the same private Unix mode or verified Windows DACL. The orchestrator checks this policy without changing the input file.

Artifact writes use create-new temporary files, flush, content hashes, and
rename. Task events form an append-only hash chain replicated from the SQLite
outbox. Writable attempts acquire a durable task/worktree lease plus an atomic
schedule claim and normalized resource claims. Global and per-provider limits,
current-graph membership, verified dependency readiness, and scope conflicts
are evaluated inside one immediate SQLite transaction. Changed-file ownership
is recorded before leases are released. Artifact hashes and event hashes detect
accidental/torn changes; they are not signatures against a privileged host
attacker.

## Redaction and secret scanning

Built-in output/log rules cover common API keys, bearer tokens, private keys, credential URLs, and sensitive JSON fields. Administrators may add validated regexes under `[orchestrator.redaction]`; these rules are applied to provider streams, capability probes, usage probes, diagnostics, and verification commands. Literal credential values are intentionally not accepted by persisted configuration. Stored provider output and command evidence use the redacted representation. Raw task source and Git diff/checkpoint artifacts can legitimately contain sensitive repository content and are stored locally under state-directory permissions; the authoritative diff is not rewritten because doing so would invalidate Git evidence.

Before checkpoint persistence, restart recovery, handover, or reviewer sharing, a preflight scans every diff byte (including removed/context lines) and changed files for common secret forms and rejects unscanned oversized files. Findings omit the value and block automatic persistence/sharing. Completion verification independently scans added lines and current changed files. Pattern scanning is not a substitute for repository classification or encryption, so operators must still treat checkpoint and handover artifacts as sensitive.

## Approval boundaries

Completion is blocked by failed verification, out-of-scope files, secret
findings, inconclusive large-file scans, or a missing required independent
review. Task instructions have no integration authority: they are accepted only
for relationally valid current-graph targets and move through an auditable
one-way lifecycle at provider safe boundaries. Integration requires a second
typed approval bound to a canonical preview hash and is confined to a dedicated
retained worktree. Worktree deletion, merge to the user's branch, push, and
publication are not automated. Release and
database-migration rollback both require an explicit `--approved-by` identity
and an exact plan-bound integrity hash. Quality-floor downgrade approval records
exist in the state schema; this release does not silently lower critical-task
quality.

A release rollback that replaces the Codex executable also requires validated, persisted evidence from the latest completed writable Codex attempt. The stored attempt/task/provider identity, actual resolved executable, and trusted worktree must agree; missing, malformed, or mismatched execution evidence fails closed. Current configuration or `PATH` resolution is not substituted for that persisted identity.

## Residual trust

Provider output, task text, repository content, Git metadata, usage-probe JSON, and subprocess output are untrusted. The design does not defend against an administrator who can replace binaries, read process memory, or rewrite the entire state directory. Use endpoint protection, code-signing policy, repository access controls, and backup retention appropriate to the source classification.
