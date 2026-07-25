# Task 8 report: Windows/WSL concurrency and rollout verification

Date: 2026-07-26 (Asia/Seoul)

Worktree: `C:\Users\kimoh\Documents\Codex\2026-07-18\principal-rust-engineer-ai-agent-orchestration\.worktrees\spec-provider-compatibility-policy`

Branch: `codex/user-global-state-provider-compatibility`

## Delivered contract

- Thirty-two cold-start CLI clients use one global database, one workspace/path
  registration, zero plan-only task rows, and one live daemon. Every client must
  succeed and none may report SQLite busy/locked diagnostics.
- Startup fan-in launches at most one daemon contender per client during the
  bounded readiness window. Windows named-pipe clients retry only error 231
  (`ERROR_PIPE_BUSY`) for a bounded interval.
- Windows coverage includes Unicode/case-equivalent paths, current-SID-only pipe
  DACL, singleton ownership, non-Git planning, import, junction redirect refusal,
  and stress. WSL coverage includes XDG/native-filesystem separation, socket
  mode/ownership, non-Git planning, import, symlink redirect refusal, and stress.
- Doctor fixtures resolve Codex, Claude, Gemini, and Agy to the compiled fake
  provider. No installed provider inference was invoked.

## Strict RED/GREEN evidence

### Four-provider doctor fixture

The first row-presence assertion passed immediately because doctor emits rows for
default providers even when their fake executable is not configured. The test was
tightened to assert the independently resolved configured executable for each
provider.

RED:

```text
cargo test -p colay --test global_doctor --features test-fixtures doctor_reports_global_workspace_and_operational_checks -- --nocapture --exact
```

Exited 1, 0/1 passed: `provider_gemini did not resolve a fake provider`.

GREEN after adding Gemini and Agy to the fake-only fixture: the identical command
exited 0, 1/1 passed in 4.16 seconds.

### 32-client cold-start fan-in

An initial `Command::output` harness used anonymous pipes and timed out after
304.1 seconds because the background daemon retained the Windows output lifecycle.
The verified test-owned `colay.exe daemon serve` process was terminated, and the
harness was changed to the repository's established temporary-file capture pattern.
This harness failure is not counted as the product RED.

Product RED 1:

```text
cargo test -p colay --test global_concurrency --features test-fixtures -- --nocapture
```

Exited 101, 0/2 passed. The stress client reported `user daemon contenders exited
before IPC readiness; last exit: exit code: 1`. Root cause: every one of the 32
clients respawned a losing `daemon serve` contender every 25 milliseconds until
readiness. The minimal fix records one startup attempt per client; the winning
child publishes IPC while losing clients wait for it.

Product RED 2 after that fix: the exact stress still exited 101, 0/1 passed in
17.21 seconds with Windows error 231, `all pipe instances are busy`. Root cause:
the Windows client attempted a named-pipe open only once after readiness. The
minimal fix retries only raw OS error 231 at 25-millisecond intervals until the
existing 10-second response deadline; absent endpoints and all other errors remain
immediate failures.

GREEN:

```text
cargo test -p colay --test global_concurrency --features test-fixtures concurrent_clients_never_observe_sqlite_busy_or_duplicate_rows -- --nocapture --exact
```

Exited 0, 1/1 passed in 37.44 seconds. The later exact focused binary, including
the Windows Unicode/case regression, exited 0 in 48.7 seconds.

### WSL path-validation order

RED on native WSL:

```text
cargo test -p orchestrator-state --test global_workspace_state -- --nocapture
```

Exited 101, 17/18 passed. `relative_colay_home_is_rejected` received WSL mount
classification failure instead of the stable absolute-path error because mount
classification ran first. Absolute validation now runs first. Windows-hosted WSL
simulation treats leading `/` as portable absolute only when WSL/kernel/mountinfo
evidence is present, so normal Windows rooted-path validation is not relaxed.

GREEN: native WSL exact 1/1 then full 18/18 passed; Windows full 14/14 passed.

## Windows-native evidence

Environment: Windows 11 Home `10.0.26100`, x86-64, Rust 1.95.0. Every CLI fixture
used an absolute temporary `COLAY_HOME`, `COLAY_TEST_FAKE_PROVIDERS_ONLY=1`, and
the compiled fake provider.

- `cargo test -p orchestrator-state --test global_workspace_state -- --nocapture`
  passed 14/14 in 6.30 seconds after the cross-platform path fix.
- `cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture --test-threads=1`
  passed 7/7 in 31.65 seconds. This includes the current-SID-only pipe DACL,
  one daemon/database across two repositories, and second-owner rejection.
- `cargo test -p colay --test global_plan_first --features test-fixtures -- --nocapture --test-threads=1`
  passed 2/2 in 27.50 seconds. Non-Git planning created no task/worktree.
- `cargo test -p orchestrator-state --test legacy_import legacy_repository_state_imports_once_without_source_mutation -- --nocapture --exact`
  passed 1/1 in 20.39 seconds.
- `cargo test -p orchestrator-state --test legacy_import nested_link_in_artifact_path_is_refused_before_source_read -- --nocapture --exact`
  passed 1/1 in 9.31 seconds and created a real Windows junction fixture.
- `cargo test -p colay --test global_concurrency --features test-fixtures -- --nocapture`
  exited 0 in 48.7 seconds. The two tests cover 32-client fan-in plus a Unicode
  global home and case-equivalent workspace path.

## WSL-native evidence

Environment: Ubuntu 24.04 on WSL2,
`6.18.33.2-microsoft-standard-WSL2`, x86-64, Git 2.43.0, GCC 13.3.0. WSL initially
had no Rust installation, so Rust 1.95.0 was installed without changing normal
user configuration under the isolated Linux-native cache
`/home/kimohy/.cache/colay-task8-20260726`. Cargo registry, toolchain, target,
temporary `COLAY_HOME`, databases, and sockets stayed on the WSL-native filesystem;
the source checkout alone was read from `/mnt/c`.

- Native path/XDG matrix passed 18/18 in 10.10 seconds after the RED/GREEN fix.
  A fresh rerun after the portable absolute-path helper change also passed 18/18
  in 12.10 seconds.
  It includes WSL `/mnt/<drive>` state rejection and independent native DB roots.
- Clean combined CLI command:

  ```text
  cargo test -p colay --test global_concurrency --test global_plan_first --features test-fixtures -- --nocapture --test-threads=1
  ```

  passed `global_concurrency` 2/2 in 62.36 seconds and `global_plan_first` 2/2
  in 23.66 seconds. The Unix socket was mode `0600`, its UID matched the
  `COLAY_HOME` UID, all 32 clients passed, and the non-Git plan created no
  writable rows.
- Exact idempotent import passed 1/1 in 32.12 seconds.
- Exact nested Unix symlink redirect refusal passed 1/1 in 24.18 seconds.

Observed WSL retries are retained as evidence: the first concurrency run after
the large Linux compile passed socket ownership but timed out one stress client
at readiness. A later full run passed the socket test but one plan client returned
raw `ENOENT`. After adding persisted daemon phase/startup diagnostics, two exact
stress reruns and the clean combined matrix passed; the failures did not reproduce,
so no speculative product change was made. A full 22-case WSL legacy-import batch
was terminated after more than five minutes waiting on a futex with a source DB
journal open; the two rollout-required exact import cases both passed independently.

## Baseline and required gates

The initial unchanged `cargo test --workspace --all-features` reached
`global_doctor` and failed two live-daemon tests at the ten-second readiness
deadline; 11/13 in that binary passed and all prior binaries passed. Both exact
tests then passed individually (5.41 and 7.04 seconds), and the unchanged complete
`global_doctor` binary passed 13/13 in 16.09 seconds. This was treated as timing
evidence, not an assumed product defect.

Final required gates on the committed candidate source:

- `cargo fmt --all -- --check`: exited 0.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`:
  exited 0; the final incremental run completed in 2.15 seconds.
- `cargo test --workspace --all-features`: exited 0 in 588.7 seconds. The
  rollout binaries passed `global_concurrency` 2/2 in 34.34 seconds,
  `global_daemon` 7/7 in 24.78 seconds, `global_doctor` 13/13 in 15.94 seconds,
  `global_plan_first` 2/2 in 23.43 seconds, and `global_resume` 3/3 in 14.82
  seconds. The daemon's three deterministic task-status stream tests passed in
  the same run.

### Full-gate resume-fixture stabilization

The first post-change full workspace gate passed the new concurrency 2/2,
global daemon 7/7, doctor 13/13, and plan-first 2/2 binaries, then failed the
pre-existing
`resume_reports_a_revision_published_after_the_initial_attachment` assertion at
revision 0 instead of 1. An exact rerun passed once and failed once, proving a
repeatable fixture race.

Timing instrumentation showed the direct test transition began at 500 ms but
finished at 2.482 seconds because it unnecessarily called
`resolve_repository_workspace`, which refreshes registry rows, before changing
the task. The daemon stream's idle deadline is two seconds, so the test-side
registry write could finish just after the stream closed. Caching the workspace
ID removed that unrelated write and initially passed three exact reruns, but a
later run under concurrent matrix load still completed the transition at 2.124
seconds and missed the two-second stream deadline. That proved the fixed
500-millisecond sleep was not a valid synchronization contract.

The CLI integration boundary is now named
`resume_reports_the_latest_published_revision`: it commits revision 1 before the
request, then proves `resume --json` projects revision 1 and `verifying` without
starting a duplicate worker attempt. Same-connection live updates, terminal
snapshot consistency, and reconnect replay remain covered by the deterministic
daemon tests `task_status_stream_emits_new_revisions_and_closes_after_terminal_state`,
`task_status_stream_never_pairs_terminal_event_with_stale_status`, and
`task_status_stream_replays_intervening_events_after_reconnect_cursor`.
No product timeout changed. The complete three-test `global_resume` binary then
passed three consecutive runs: 3/3 in 27.62, 27.29, and 27.37 seconds.

### Full-gate fixture diagnostics

The next full run reached the new stress test after all 32 clients returned, but
the test attempted to open `daemon_instances` before evaluating client outputs
and failed with SQLite error 14. That ordering masked whether a client had failed
to initialize the database. The test now preserves an unavailable-diagnostics
message and evaluates every client stdout/stderr first. No SQLite retry or other
speculative product change was added because the error did not reproduce: the
exact 32-client stress passed three consecutive runs in 37.50, 36.18, and 36.18
seconds, then passed in both subsequent full workspace runs.

The second full run passed concurrency 2/2, then exposed a separate pre-existing
fixture race in `old_schema_migrates_before_untrusted_provider_is_evaluated`.
The fake provider's observation path existed during `fs::write` before the JSON
was readable, while the test waited only for existence and parsed zero bytes.
The test now polls for valid JSON within its existing five-second deadline. Its
exact regression passed three consecutive runs in 7.41, 5.03, and 5.43 seconds,
and the final full workspace run passed the complete `global_daemon` binary 7/7.

## Documentation and tracker changes

- `docs/testing.md` now contains reproducible Windows and WSL matrix commands,
  temporary-root/fake-only rules, and the exact concurrency invariants.
- `docs/release.md` makes the platform matrix a release gate for any global-state,
  IPC, import, or plan-first change.
- `docs/qa/wsl-nightly-error-tracker.md` moves `WSL-010` to fixed and records the
  OS/toolchain identities, passing counts, strict RED causes, retries, and WSL
  batch observation.
- Deferred coverage is closed: doctor covers all four fake providers, and the
  platform matrices run real Unix symlink and Windows junction redirect artifacts.

## Self-review

- Product and test subprocesses use `Command` with a separate executable and
  argument collection; no Rust shell interpolation was introduced.
- Normal IPC still uses one daemon writer. The fan-in fix does not create a new
  direct SQLite write path or expand retry behavior beyond bounded startup and
  Windows `ERROR_PIPE_BUSY`.
- The Windows pipe ACL implementation is unchanged and reverified. Unix socket
  mode/ownership is asserted from filesystem metadata.
- Missing provider usage remains unknown; no quota units, credentials, usage
  pages, unofficial endpoint, or provider inference were touched.
- Provider wire details remain outside `orchestrator-domain`; the state path fix
  remains I/O-free with respect to database behavior and preserves WSL fail-closed
  mount classification.
- Tests create no writable worktree and perform no merge, push, or worktree
  deletion. Legacy redirect fixtures are test-owned temporary artifacts.

## Remaining concerns

- The two non-reproducible WSL concurrency failures and the broad import-batch
  futex stall are recorded above and in the tracker. Exact rollout cases and the
  final clean WSL matrix passed; a repeated occurrence should be promoted to a
  dedicated tracker ID with the now-persisted daemon diagnostics.
- The existing `WIN-003` transient `icacls.exe` access-denied tracker item remains
  outside this task. No ACL relaxation or retry was added.
