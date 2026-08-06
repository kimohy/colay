# Testing

## Fake-only rule

Tests and CI must never invoke real Codex, Claude, Gemini, or Agy inference. Provider integration tests use the compiled `fake-provider-cli` or `FakeAdapterRuntime`. The fake runtime canonicalizes its configured executable and rejects any basename other than `fake-provider-cli`, so accidentally passing `codex`, `claude`, `gemini`, or `agy` is a test failure.

CI clears common provider API-key variables and sets `COLAY_TEST_FAKE_PROVIDERS_ONLY=1` at job scope. Compatibility workflows may build an exact official Codex source revision and run only the explicit version/help/schema probe allowlist; they never pass a prompt.

Configuration, resolver, and rollback tests also use local fixtures and fake binaries only. They do not invoke provider inference: Windows and Unix executable-resolution cases exercise fixture files, and rollback cases validate persisted execution evidence without resolving a live provider binary from the current `PATH`. WSL resolver tests simulate `drvfs`, 9p `aname=drvfs`, lexical Windows-drive mounts, and PE signatures with local inert fixture files; the candidates are classified without being invoked.

Daemon tests are also inference-free. State tests race independent SQLite
connections for command claims and lease acquisition, while runtime tests use
short Tokio intervals to cover heartbeat, cancellation, stop, and release. The
CLI lifecycle test launches only the compiled `colay daemon serve` child in a
temporary repository, verifies start/status/restart/stop, and checks that no
child remains. Crash recovery is exercised through expired leases and stale
claimed commands; no provider binary or network listener is involved.

Chat TUI tests use Ratatui's in-memory backend and scripted key events. They
cover exact wide/medium/narrow/compact thresholds, pane traversal, command and
target pickers, the no-silent-retarget invariant, administration round trips,
terminal restoration, and a bounded tail from 1,000 messages. CLI reconnect
tests launch only `colay daemon serve` in a temporary repository, require
session/message completion within 500ms, reopen SQLite, verify double redaction,
confirm the daemon survives the client, and stop the child during cleanup.

Phase 3 graph tests cover deterministic DAG validation, cycles, dependency and
scope errors, provider/profile eligibility, stable proposal hashes, immutable
valid/invalid revisions, exact-hash idempotent approval, session-isolated graph
projection, and stale approval overlays. `chat_plan_approval.rs` launches the
real local daemon plus only the compiled fake official CLI. It proves a single
read-only planner invocation, heartbeat-backed command completion, zero
tasks/worktrees/worker leases before approval, wrong-hash rejection, exact
queued-task/dependency materialization, SQLite reconnect, and child cleanup.

Phase 4 scheduling tests race two independent SQLite connections, enforce exact
global/provider capacity, dependency verification, component-aware scope
ownership, repository-wide exclusion, idempotent release, and ordered
instruction recovery. The daemon test runs two slow disjoint executor futures
concurrently while continuing to renew and release claims. The
`parallel_task_execution.rs` integration test creates a real temporary Git
repository and launches only real fake official-CLI subprocesses. It proves
overlapping invocation intervals, distinct retained worktrees, target-exact
instruction application, sealed completion evidence, and restart without a
duplicate attempt or mutation of the user's branch.

Phase 5 domain/engine/state tests cover stable preview seals, exact approval,
overlap and source mutation, immutable persistence, idempotent resolution-task
creation, and interrupted-application reconciliation. The daemon
`result_integration.rs` test uses a real temporary Git repository: typed preview
remains read-only, exact approval applies two verified sources only to the
dedicated integration worktree, session state completes, and the user plus task
worktrees remain unchanged.

## User-global state platform matrix

Run this matrix before rolling out a user-global state change. Every CLI fixture
creates an absolute temporary `COLAY_HOME`, clears its child environment, sets
`COLAY_TEST_FAKE_PROVIDERS_ONLY=1`, and configures only the compiled
`colay-e2e-fake-provider`. The 32-client test uses separated executable/argument
arrays and rejects every failed client, `database is locked`, `database is busy`,
`SQLITE_BUSY`, or `SQLITE_LOCKED`. It also requires exactly one database, one
workspace/path, zero plan-only tasks, and one live daemon. Before a successful
test returns, it explicitly stops that daemon and waits at most ten seconds for
the live lease row, IPC endpoint, and OS process to disappear. `Drop` cleanup is
only a failure-path fallback and is not accepted as successful cleanup evidence.

Resume coverage is deliberately split at deterministic boundaries. The CLI
integration test `resume_reports_the_latest_published_revision` proves that an
attachment projects the latest revision already committed before the request.
The daemon tests `task_status_stream_emits_new_revisions_and_closes_after_terminal_state`,
`task_status_stream_never_pairs_terminal_event_with_stale_status`, and
`task_status_stream_replays_intervening_events_after_reconnect_cursor` exercise
same-connection live updates and reconnect replay without wall-clock sleeps.

Windows-native PowerShell:

```text
cargo test -p colay --bin colay ipc_client::tests --all-features
cargo test -p orchestrator-state --test global_workspace_state -- --nocapture
cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_plan_first --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_resume --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_doctor --features test-fixtures doctor_reports_global_workspace_and_operational_checks -- --nocapture --exact
cargo test -p orchestrator-state --test legacy_import -- --nocapture
cargo test -p colay --test global_concurrency --features test-fixtures -- --nocapture
```

This covers Windows native path validation, Unicode/case-equivalent workspace
registration, the current-SID-only named-pipe DACL, singleton ownership,
non-Git plan-first behavior, all four fake provider doctor fixtures, idempotent
legacy import, junction redirect rejection, and the 32-client cold-start fan-in.

### LocalSystem state-artifact ACL correction

Implementation commit `24cd2743398affd74bc74d0edab8590022f0dbd0`
(`fix: normalize LocalSystem state ACL principals`) has the following focused source evidence:

```text
cargo test -p orchestrator-windows-ipc state_artifact::tests::required_principals_ -- --nocapture --test-threads=1  # 2 passed
cargo test -p orchestrator-windows-ipc state_artifact::tests::bounded_localsystem_ -- --nocapture --test-threads=1  # 1 passed
cargo test -p orchestrator-windows-ipc state_artifact --all-features -- --nocapture --test-threads=1                 # 28 passed
cargo test -p orchestrator-state permissions::tests::windows_ --all-features -- --nocapture --test-threads=1         # 8 passed
cargo fmt --all -- --check                                                                                             # passed
```

The synthetic collision tests are
`required_principals_only_normalize_local_system_collision` and
`required_principals_reject_other_role_collisions`; they permit only `user == SYSTEM` and reject
all administrator-role collisions. `bounded_localsystem_acl_requires_each_unique_trustee_once`
requires exactly one ACE for SYSTEM and Administrators, accepts either trustee order, and rejects
missing, duplicate, and unknown trustees. The normal-user three-ACE regression is
`owned_acl_builds_exact_normal_and_localsystem_principal_sets`.

`retained_handle_localsystem_file_and_directory_fast_paths_when_applicable` is a conditional native
test: on a LocalSystem host it verifies file and directory `ensure -> verify -> ensure` and that the
second ensure performs no security write; on this normal-user Windows host the LocalSystem branch
was skipped. Bounded native LocalSystem release QA is therefore required before `WIN-006` can close;
source tests alone do not establish native or nightly closure.

### Native Windows state ACL registration stress

Run the native acceptance harness from PowerShell 7.2 or newer after building
the exact test-fixtures binaries. The host must provide Python with SQLite
3.37 or newer so the harness can create schema-v8 sources and inspect
schema-17 `STRICT` tables without invoking a SQLite command shell. The harness
accepts only `colay-e2e-fake-provider.exe`, clears provider credential variables
from every child, and passes every process argument with
`ProcessStartInfo.ArgumentList.Add`. Run it only from the exact clean commit that
was used to build both binaries; the mandatory commit argument makes source drift
fail closed.

```powershell
cargo build -p colay --bins --features test-fixtures
$sourceCommit = (git rev-parse HEAD).Trim()
if (-not [string]::IsNullOrWhiteSpace((git status --porcelain=v1 --untracked-files=all))) {
  throw 'authoritative Windows stress requires a clean worktree'
}
New-Item -ItemType Directory -Force artifacts\qa\windows-state-acl | Out-Null
& scripts\qa\windows-state-acl-stress.ps1 `
  -ColayExe (Resolve-Path target\debug\colay.exe) `
  -FakeProviderExe (Resolve-Path target\debug\colay-e2e-fake-provider.exe) `
  -EvidenceRoot (Resolve-Path artifacts\qa\windows-state-acl) `
  -ExpectedSourceCommit $sourceCommit
```

The run is a failure unless five sequential incumbent registrations have
nearest-rank p95 at or below 5,000 ms, four simultaneous registrations each
finish within 8,000 ms, and the source still declares the unchanged 10,000 ms
IPC response timeout. Each source is a distinct non-empty schema-v8 database.
Successful command timing is the operating-system process lifetime from
`StartTime` through `ExitTime`; synchronous CIM observation and debug-event
processing do not run inside that measured interval. The latency environment
keeps the aggregate inspection marker but omits the attributed-marker environment
key entirely. It must record exactly 18 aggregate events for the nine imports and
leave its attributed sentinel directory empty.

Immediately after `daemon start`, the main harness anchors the exact schema-v1
daemon UUID, integral PID, and resolved Colay executable path, then permits only
identity-preserving `booting`/`probing` status transitions until exact `online`.
This readiness gate uses one cleanup-inclusive monotonic 5,000 ms deadline and
finishes before any serial or concurrent registration timer starts. Its evidence
is recorded as `measurement_diagnostics.main_daemon_readiness` and is explicitly
excluded from the 5,000/8,000 ms latency thresholds.

After the latency phase and main-daemon shutdown, a separate fresh state root runs
the functional `DEBUG_PROCESS` audit with attributed markers enabled. Its time is
explicitly excluded from the 5,000/8,000 ms acceptance limits. That correctness
phase must record exactly two aggregate events and one attributed group containing
two distinct empty events, and the group must equal the durable
`source_root_hash`. The audit covers only the controlled child tree rooted at the
exact PowerShell process launched by the harness; it is neither a host-wide
process monitor nor evidence against Administrator or `SYSTEM` activity.
The generated child serializes its complete readiness transition with an explicit
JSON depth. The parent rejects stderr, missing or truncated nested readiness data,
identity drift, invalid states, and any command budget that exceeds the actual
remaining cleanup-inclusive deadline before accepting the child result.

The harness compares every source SQLite-family hash before and after import,
validates workspace/path/import/session and publication-ledger cardinality,
runs SQLite integrity and foreign-key checks, and requires zero tasks, task
attempts, worktrees, coordinator leases, and worker leases. The functional
child-tree audit must find no `whoami.exe` or `icacls.exe` launch. Post-stop
identity-checked residue observation separately requires the endpoint, live
leases, and attributable Colay, fake-provider, and utility processes to be
absent. The harness writes schema-2 timestamped JSON plus `summary.json`, with
separate latency/correctness marker-phase evidence and failure evidence when safe.

#### Historical source-fixed baseline at `a52945d` — not current Task 5 acceptance evidence

The earlier source-fixed baseline (native ACL commits
`c83200d`, `83181f0`, `ab5617e`, `361391a`, `4e5e408`, `d1efe8e`, and
`3b6b8ec`; receipt/concurrency commits through `c841b75`, `77936e9`, and
`a52945d`) used run `20260805T093708005Z`. The five sequential times were
`[4273, 4261, 1336, 1343, 1420]` ms, with nearest-rank p95 `4273` ms. The four
concurrent times were `[5844, 5750, 5653, 5579]` ms, with maximum `5844` ms.
The run recorded exactly 18 inspections, durable cardinality
workspaces/paths/imports/sessions `10/10/9/9`, SQLite `integrity_check = ok`,
zero foreign-key violations, zero writable rows, zero utility launches, and
zero post-stop process/live-lease/endpoint residue. The nine unchanged source
database SHA-256 values were
`5ef31365ff98b5ad813874a844f6d58dfe3cfe66f831664d0c978b494744a6fe`,
`61c89c13eaaedae9bc97ff26931675ecfd543a1ca2f23e6ece90dcfcb0ca3861`,
`460a5a36f40ba61a07350267e1c44c0dd523aa38934a570fd5277007636e46ee`,
`48e8c07fdbde2d07f0743dfecd30daf060fdee7ded54409b53bb82802ce95892`,
`2d52603dc8d97280c5b02534ad55f140798e8ee5240a04d0e86a1b10b025c75f`,
`d05fa9fe8351a01ed0048bc20f2c73df8063ffd966f62418013c105058ba2af8`,
`307d8b9e8871c7cda832305d3fc0c81aec5f439712713a36f73bca766f86b200`,
`05fa6d8b15123f9a5bb04e16ad23993e6fc7dca1e915ba77fddb6619d08b9f87`,
and `b1d706bdff8411b89e7c4183e05fc7c7fd36a1a0981d82d91121e3a9cb69951a`.
The exact debug binaries were SHA-256
`2197cef5b4120ce60072b9d657ba42f19f32d2933d9dfc899fc1efcf50724196`
(`colay.exe`) and
`9b146633d2914023021f695837572c4a5d9c013e887b0c2b1990b67787ddbba3`
(`colay-e2e-fake-provider.exe`). This superseded harness run is retained as a
historical baseline only and does not establish acceptance for the current
source or current harness.

At `a52945d`, the original failures each passed 10/10 exact serial repetitions.
The `live_doctor_` group passed 4/4 serial in 42.147 seconds and 4/4 with default
threads in 19.478 seconds. Full `global_doctor` passed 33/33 serial in 156.932
seconds and 33/33 with default threads in 45.997 seconds. These historical
source checks do not establish current acceptance.

#### Failed Task 5 attempt `20260805T111210158Z` — superseded harness, not accepted

The evidence file
`windows-state-acl-stress-20260805T111210158Z.json` has SHA-256
`17c62be3578f4d34ed2500ee757e18380d74a131a71a2b2f44f5e0cd501bd914`.
Source was `244ee2b1901a516dcabf56e747f6d78a7abeaaae`; harness SHA-256 was
`c8c052458aed79317235ae6aba192e1f3589b6b9ac5ec5fbbbeabbcdc5209ce0`.
The exact binary SHA-256 values were
`21563398d0e5c8d658b1c0e1499bb04a22c12621b03d2213be0c4850b4a6913d`
for `colay.exe` and
`280fb96d1130f4cb69af5bc3d84053f4c17695a5e2a9ed3d8b2da8767fe5f83d`
for the fake provider. Serial times were
`[5020, 1963, 2080, 1763, 2029]` ms. For five samples, the nearest-rank p95 is
the maximum, `5020` ms, which exceeded the unchanged `5000` ms limit. The old
harness stopped at that threshold, so concurrent registration, durable-state
acceptance, marker acceptance, SQLite acceptance, and the functional process
audit did not run and must not be inferred as passing. Cleanup did complete:
the daemon and endpoint were stopped, live leases and residual processes were
zero, cleanup errors were zero, and minimum free space was `7.537 GiB`.

Follow-up characterization identified synchronous `Win32_Process` CIM
observation inside the timed wait path as measurement interference. That
observation was removed from latency measurement and the functional process
audit was separated. This diagnosis does not convert the failed run into a
pass; current acceptance remains pending. Published Windows CI and nightly
verification also remain required; neither the historical source checks nor the
failed attempt close `WIN-005`, `WSL-022`, or `WSL-023`.

Run the Linux commands inside WSL with Cargo build output on the Linux-native
filesystem, not under `/mnt/<drive>`. A native checkout is preferred; when the
source checkout is mounted read-only or used only as source, set
`CARGO_TARGET_DIR` to a WSL-native temporary directory:

```text
export COLAY_TEST_FAKE_PROVIDERS_ONLY=1
mkdir -p /home/<user>/.cache/colay-task8-target
export CARGO_TARGET_DIR=/home/<user>/.cache/colay-task8-target
cargo test -p orchestrator-state --test global_workspace_state -- --nocapture
cargo test -p colay --test global_concurrency --test global_plan_first --features test-fixtures -- --nocapture --test-threads=1
cargo test -p orchestrator-state --test legacy_import -- --nocapture
```

This covers XDG defaults and Windows-mount refusal, a mode-`0600` Unix socket
owned by the same user as `COLAY_HOME`, a non-Git home plan, idempotent import,
Unix symlink redirect rejection, and the same 32-client stress contract. Windows
and WSL evidence is valid only when their resolved databases are under separate
native temporary roots; a WSL database under `/mnt/<drive>` is a test failure.

After a nightly is published, repeat WSL QA from an isolated npm prefix and
`COLAY_HOME`. For Codex, Claude, Gemini, and Agy, record the resolved native path
and verify an ELF binary or Linux-native script identity as appropriate before
allowing any public version/help probe. A Windows-backed or PE candidate must be
rejected before that probe and must never be invoked. For every bounded
plan-only turn, verify the requested provider identity, canonical outcome,
schema-17 bounded/redacted successful evidence, and zero rows in `tasks`,
`task_attempts`, `worktrees`, `coordinator_leases`, and `worker_leases`. Finish
with SQLite `integrity_check`, foreign-key checks, daemon stop, socket removal,
and confirmation that no QA Colay process remains. Automated tests and CI remain
fake-only; authorized real-provider turns belong only to this post-release
operator QA.

## Required local verification

```text
npm test
node --test scripts/release/test/workflow-contract.test.mjs
python scripts/generate_codex_matrix.py --check
git diff --check
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

`npm test` uses only the dependency-free Node.js built-in test runner. It
checks the npm package templates, launcher behavior, release version/channel
classification, staging allowlists and checksums, retry-safe publication logic,
and workflow contracts without contacting npm or GitHub.

## Release package smoke tests

The release workflow packs all four staged tarballs locally and, on each native
runner, installs the root tarball and its selected platform tarball into an
isolated npm prefix with `--offline --ignore-scripts`. It then runs only
`colay --version` from that isolated installation. On Windows, the smoke
invokes npm's generated `colay.ps1` global command shim through the known
Windows PowerShell executable with separated arguments and `shell: false`.
This proves package versions, exact optional dependencies, and the embedded
Rust version agree without a registry publish or provider process. Linux x64
uses a musl-linked binary and
has no npm `libc` selector, so the package remains installable on both musl and
glibc hosts.

No provider credentials are needed. If an integration test asks for a provider login or consumes Enterprise quota, stop: that test violates the repository contract. For lower disk pressure, the Rust verification suite may be run with `CARGO_INCREMENTAL=0`.

## Contract coverage

- `codex-compat/tests/contracts.rs` validates exact N/N-1 help/schema/event fixtures, unknown optional preservation, fail-closed lifecycle events, quota classification, resume events, and the committed compatibility matrix.
- `orchestrator-test-support/tests/provider_e2e.rs` runs each adapter through fake structured streams or Agy's bounded plain-text bridge, malformed/error/quota paths, cancellation, redaction, bounded process execution, and executable/argv usage probes.
- `orchestrator-test-support/tests/multi_provider_handover_e2e.rs` drives the vendor-neutral lifecycle from a fake Gemini daily quota event through a sealed checkpoint, Codex implementation, a monthly-headroom warning carried into the Claude handover, Claude read-only review, and the independent completion gate.
- `orchestrator-cli/tests/fake_cli_handover_e2e.rs` launches the compiled `colay` and gated `colay-e2e-fake-provider` binaries. It proves a Codex quota failure preserves a partial Git diff, Claude exactly acknowledges the sealed bundle before writing, local fmt/clippy/check/test evidence reaches `Completed`, the original branch remains untouched, and no merge/push/cleanup occurs. A second scenario exercises sealed, explicitly approved SQLite restore, recovery backup retention, and the post-restore JSONL hash chain.
- `orchestrator-state/tests/migration_contract.rs` starts at SQLite schema v1, verifies the sequential plan through v17 and historical event hashes, and rebuilds constrained tables without losing rows. It checks the v16 failure-outcome constraints and the v17 successful-evidence constraints: v16 and other historical succeeded rows migrate with `evidence_redacted = NULL`, while running, failed, and cancelled evidence remains `NULL`. It also proves dry-run non-mutation, inspects backups, and rejects checksum/future-schema tampering. `orchestrator-state/tests/config_migration.rs` separately verifies config v1 -> v2 -> v3 -> v4, legacy state-path materialization, explicit-path preservation, and the `.colay` v4 default.
- `orchestrator-cli/tests/daemon_lifecycle.rs` proves public help, hidden internal serve, absent-state status, single-instance start, idempotent start, restart ownership transfer, graceful stop, and child cleanup.
- `orchestrator-cli/tests/chat_tui_reconnect.rs` proves chat help/docs, durable daemon command processing, redacted persistence, a second SQLite connection restoring the session, and daemon survival/cleanup.
- `orchestrator-cli/tests/chat_plan_approval.rs` proves the full goal -> read-only fake planner -> validated revision -> exact typed approval path through a real daemon process, with no pre-approval writable artifact and no real provider.
- `orchestrator-daemon/tests/conversation_flow.rs` proves automatic answers, interview revisions, redacted provider failure, non-Git promotion blocking, exact approval, and Git-HEAD drift rejection. It asserts tasks, task attempts, worktrees, and coordinator/worker leases remain empty before approval.
- `orchestrator-cli/tests/parallel_task_execution.rs` proves approved disjoint tasks overlap through fake official CLI processes in isolated Git worktrees, task instructions stay target-exact, claims are released, restart is idempotent, and no integration/merge/push/cleanup occurs.
- `orchestrator-daemon/tests/result_integration.rs` proves typed exact-hash integration through the daemon with real Git worktrees and no mutation of the user or source task worktrees.

The multi-provider test deliberately uses synthetic Git evidence, the persistence secret preflight, and an in-memory fake runtime; it validates orchestration contracts without executing a real model or mutating a user repository. Engine worktree tests separately exercise actual temporary Git repositories.

## Adding a Codex release

1. Run the non-inference compatibility workflow for the exact release tag.
2. Review command/option/schema changes in the uploaded report.
3. Add a version directory with manifest, public metadata, and reviewed redacted JSONL contract fixtures.
4. Add the exact version to `codex-version.toml`, run `python scripts/generate_codex_matrix.py`, and commit the regenerated `codex-matrix.json` in the same change.
5. Run all required verification commands.
6. Merge only after human review; CI never auto-enables or auto-merges a new writable adapter.
