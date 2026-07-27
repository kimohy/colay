# Task 4 Windows daemon endpoint identity fix report

## Status

Verified and ready for the required `fix: unify Windows daemon endpoint identity` commit. Windows
daemon ownership and the primary named pipe now use one canonical, SID-qualified, lossless
identity. Current clients can authenticate and retain a legacy schema-v1 pipe route, and current
daemons safely serve both current and legacy clients for Unicode state roots. Unix endpoint
behavior is unchanged.

No real provider executable, external endpoint, credential, telemetry, push, merge, worktree
deletion, or persisted-schema mutation was used.

## Root causes

- The protected Windows bootstrap mutex used canonical UTF-16 root identity plus the current SID,
  while the pipe used SHA-256 over `Path::to_string_lossy()`. Case and 8.3 aliases could therefore
  address one physical state root through different pipe names, and ill-formed UTF-16 could
  collide.
- A current client had no authenticated fallback to the prior lossy schema-v1 pipe. Merely probing
  that name is unsafe because distinct non-Unicode roots can have the same lossy spelling.
- A current daemon served only its new endpoint, so prior clients could not reach it during a
  rolling local upgrade.
- The first direct read-only legacy-identity implementation copied a live database and WAL. Under
  32-client startup pressure that produced `database disk image is malformed`. Replacing the copy
  with a direct read transaction exposed the migration window: schemas v1-v3 do not yet contain
  `daemon_instances`, while phase is introduced only in v9.

## Production behavior

### Shared Windows identity and endpoint candidates

- One versioned SHA-256 input now supplies both the `Global\\ColayDaemonOwner-v2-*` mutex and the
  `\\\\.\\pipe\\colay-v2-*` primary pipe. It contains length-prefixed canonical physical-root
  UTF-16 units and length-prefixed current-user SID bytes. No lossy conversion participates in the
  primary identity.
- Existing secure root creation, reparse rejection, canonicalization, current-user ACL checks, and
  owner-lock ordering remain in force. Missing roots are still created securely before identity is
  derived.
- `GetShortPathNameW` is wrapped in the audited Windows FFI crate. The regression uses an actual
  alternate 8.3 spelling when the volume supplies one and passes both spellings through the normal
  reparse rejection and canonicalization path. On this verification host an alternate spelling was
  available, and both forms produced the same primary endpoint.
- Endpoint candidates expose the primary route and the exact former v1 route. A current daemon
  serves the legacy listener only when the raw state root is Unicode; a non-Unicode root keeps the
  candidate available to an authenticating current client but never exposes a collision-prone
  legacy server listener.

### Authenticated legacy client route

- Discovery probes the primary endpoint first. Only when it is unavailable does the client inspect
  the expected state database using read-only SQLite flags and one deferred read transaction.
- Schemas v1-v3 mean no legacy online owner. Schemas v4-v8 read only the stable instance, PID,
  lease, and release fields. Schema v9 and later additionally require phase `online`. Future
  schemas, invalid identifiers/PIDs/timestamps/phases, zero PIDs, or multiple unreleased owners fail
  closed. No migration, backup, write, or snapshot copy occurs.
- A candidate legacy pipe must return schema-v1 `daemon.status` with the same online instance ID
  and PID as that read-only database identity. The status preface and the caller's actual request
  are sent on the same connection, eliminating a validation/use pipe swap. The selected identity is
  pinned, re-read before each subsequent request, and revalidated on that same connection.
- The selected route is stored on `DaemonClient` and is used for workspace registration, global
  requests, normal requests, streams, status fallback, and shutdown. A healthy authenticated old
  daemon prevents a new contender from being spawned. Existing modern owner-PID classification,
  legacy owner fallback, exact-child cleanup, and loser reaping remain unchanged.

### Dual-listener lifecycle

- A Windows server starts independent primary and legacy accept loops sharing the established
  writer/database state. Both listeners preserve current-user-only DACLs and schema-v1 framing.
- Cancellation drains both listener tasks and their connection tasks. If either listener cannot be
  created or fails while accepting, shared cancellation stops the other and the first error is
  returned, so a collision cannot leave a partially reachable daemon.
- Unix continues to use its single runtime `daemon.sock` endpoint and existing cleanup path.

## RED evidence

- Identity and client tests initially failed to compile before the shared digest, endpoint
  candidates, pinned route, and same-connection validation seams existed.
- The 32-client regression failed with `database disk image is malformed` while the implementation
  copied a live WAL snapshot.
- The direct-read revision then made the lifecycle test exit 101 and leave its spawned daemon
  online. An isolated invocation captured `no such table: daemon_instances`; the focused v1-v3
  state regression reproduced the same error before the schema-aware read was implemented.
- Focused lint runs caught only local formatting/size/panic-style test issues. They were refactored
  without allowances or behavior changes.

## GREEN evidence

| Command / contract | Result |
| --- | --- |
| Read-only identity schema/WAL matrix | 5 passed; v1-v3, v4-v8 active/stale/released/duplicate, v9 phases, current WAL |
| Focused `ipc_client::tests::` | 23 passed; includes actual Windows legacy fake pipe and pinned same-connection route |
| Exact lifecycle start/status/stop/restart | 1 passed |
| Exact 32-client contention regression | 2 consecutive passes, 38.83s and 29.33s test bodies |
| `COLAY_HOME` case-alias process regression | 1 passed; one database and one unreleased daemon owner |
| Primary + legacy schema-v1/DACL process regression | 1 passed |
| Actual 8.3 short-root alias regression | 1 passed; alternate spelling available and canonical primary identities matched |
| `cargo test -p orchestrator-daemon --all-features -- --nocapture --test-threads=1` | 31 unit + 15 conversation + 1 integration passed before the 8.3 addition; the new focused test also passed |
| `cargo fmt --all -- --check` | exit 0 |
| `cargo clippy --workspace --all-targets --all-features -- -D warnings` | exit 0 |
| `cargo test --workspace --all-features` | final exact run exit 0; all workspace/doc tests passed in 636.7s |
| `git diff --check` | exit 0 |

The first full-suite attempt saw one transient, untouched Windows state test fail because Windows
denied execution of `C:\\Windows\\System32\\icacls.exe`. That exact scheduler test immediately
passed in isolation (1/1), and the fresh exact full-suite run passed it and every other test. No
production or test code was changed for the transient environment failure.

## Self-review and cleanup

- Missing usage semantics, provider boundaries, domain neutrality, append-only audit behavior,
  redaction, approvals, and persisted schema version 16 are unchanged.
- Legacy compatibility uses only documented schema-v1 requests and the expected local state
  database. It adds no process enumeration, unofficial endpoint, credential access, or external
  transport.
- No timeout increase, arbitrary retry/sleep, ignored test, reduced concurrency, shell
  interpolation, lossy primary identity, lint allowance, or weakened security assertion was added.
- Test and diagnostic daemons were stopped by exact isolated route or exact checked executable;
  final process inspection found no daemon from this worktree.
- The ignored diagnostic `.colay/config.toml` accidentally created in the worktree was removed; no
  repository configuration or tracker documentation was changed.

No open correctness or security concern remains for this scope.
