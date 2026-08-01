# Task 4 final WSL report: clean-HEAD conversation QA refresh

## Status

The requested WSL build, four non-Git fake-provider scenarios, database checks, and daemon cleanup
passed on exact source HEAD `9779bd601159fc910ccf2b6614a17fd8cd20bb16`. The tracker now records
the fresh build and evidence paths. No production code, migration, fixture, provider account,
external endpoint, Docker runtime, credential, push, merge, or worktree deletion was used.

## Reviewed inputs

Before execution, the approved design and implementation plan, the original Task 4 report, both
Windows named-pipe fix reports, and the complete WSL nightly tracker were reviewed. The clean linked
worktree was already isolated on `codex/fix-real-provider-conversation`; `git status --short` was
empty and `git rev-parse HEAD` returned the required SHA.

The prior reviewer minor was addressed in the tracker: three noncontiguous response/evidence lines
are now explicitly labeled selected excerpts rather than one diagnostic value. Fresh failure
evidence records exact stored values and byte lengths in `final-invariants.json`.

## Linux-native release build

Environment: WSL 2 Ubuntu 24.04, kernel `6.18.33.2-microsoft-standard-WSL2`, x86-64, Linux
Rust/Cargo 1.95.0. Cargo used the existing isolated Linux registry/toolchain cache and a brand-new
ext4 target:

```text
RUSTUP_HOME=/home/kimohy/.cache/colay-task8-20260726/rustup
CARGO_HOME=/home/kimohy/.cache/colay-task8-20260726/cargo
CARGO_TARGET_DIR=/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-target
CARGO_INCREMENTAL=0
cargo build --locked --offline --release --features test-fixtures \
  --bin colay --bin colay-e2e-fake-provider
```

Result: exit 0 in 255.7 seconds.

- Colay: `/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-target/release/colay`
  - version: `colay 0.1.0`
  - x86-64 GNU/Linux ELF Build ID: `58e434090b273ad6ba71645005aeb2918f8f0efb`
  - SHA-256: `ebe7578edf7cd9a72c4d0b7d78fc4c889803a4516f6473ad27dd994704a7eb94`
- Fake provider:
  `/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-target/release/colay-e2e-fake-provider`
  - SHA-256: `8f08d163f99f16f9f8c57dacacb91678bdf90a6ec89575d5bfb4d74d1b6c9170`

The Linux build emitted four unused/dead-code warnings for Windows legacy endpoint identity code in
`ipc_client.rs`. They did not fail the release build or any requested runtime QA. The previously
reviewed pipe-order report records exact-head workspace Clippy and full-suite success on Windows;
this refresh did not rerun Linux Clippy or the full workspace suite.

## Isolated QA environment

- QA root: `/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-qa`
- `COLAY_HOME`: `.../home-evidence`
- `TEMP` and `TMP`: `.../tmp-evidence`
- wrapper logs: `.../logs-evidence`
- non-Git workspaces: `.../workspaces/{eligible,codex,fallback,failure}`

Every Colay command set `COLAY_TEST_FAKE_PROVIDERS_ONLY=1` and used the absolute fresh release
binary. The only configured provider executables were local test-only wrappers delegating to the
fresh `colay-e2e-fake-provider`. Public capability probes and conversation invocations therefore
reached only the fake binary. No real Codex, Claude, Gemini, or Agy inference ran.

`colay --json migrate apply` exited 0. It created the user-global database at
`.../home-evidence/state/state.db`, applied schema 0 through 16 with committed checksums, and
reported no pending versions.

## Four non-Git scenarios

| Scenario | CLI | Durable attempt | Provider evidence |
| --- | --- | --- | --- |
| Requested Claude | exit 0 | Claude, succeeded, `answer_complete` | one Claude conversation argv with `--permission-mode plan` |
| Requested Codex | exit 0 | Codex, succeeded, `answer_complete` | Codex argv contains `--skip-git-repo-check` |
| Disabled Gemini | exit 0 | selected Codex, succeeded, `answer_complete` | fallback notice retained; Gemini log absent; Codex started |
| Codex crash | exit 1 | Codex, failed, `needs_attention` | marker 3 to 4; exactly one additional fake start |

The selected direct-Codex argv was:

```text
exec --skip-git-repo-check --json --sandbox read-only \
  -C /home/kimohy/.cache/colay-task4-final-9779bd6-20260727-qa/workspaces/codex \
  --model gpt-5.6-terra -c model_reasoning_effort="medium" -
```

All three final Codex conversation argv records (direct, fallback, and failure) included the public
Git bypass. All four workspaces contained neither `.git` nor `.colay` after QA.

The fallback response retained the exact notice:

```text
Requested provider gemini is unavailable; using codex for this read-only turn.
```

No `fake-gemini-cli.log` existed. The Codex wrapper logged the fallback conversation process, so
the provider changed before process start and not as runtime failover.

## Failure and database evidence

The crash command exited 1 and increased the fake conversation marker from 3 to 4. The failed
attempt stored:

- `provider_id = codex`;
- `status = failed`;
- `outcome = needs_attention`;
- exact `response_redacted = "codex process failed. Review the redacted evidence, then retry this
  conversation."`;
- exact `error_redacted` size 792 UTF-8 bytes; and
- exact `evidence_redacted` size 701 UTF-8 bytes.

The complete contiguous stored error and evidence values are recorded verbatim in
`evidence/final-invariants.json`; this report does not join selected fragments and call them one
diagnostic. The corresponding `request_conversation_turn` command state was `failed`.

Final read-only database checks returned:

- `PRAGMA user_version = 16`;
- `PRAGMA integrity_check = ok`;
- zero rows from `PRAGMA foreign_key_check`;
- four sessions and four conversation attempts; and
- zero rows in `tasks`, `task_attempts`, `worktrees`, `coordinator_leases`, and `worker_leases`.

## Daemon cleanup

Before cleanup, daemon status reported online PID 1183, executable
`/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-target/release/colay`, build version
`0.1.0`, and target `linux/x86_64`.

- `colay --json daemon stop`: exit 0, `state = stopped`.
- Follow-up `daemon status`: exit 0, `state = stopped`.
- `/proc/1183`: absent.
- Isolated `runtime/daemon.sock`: absent.
- Exact `/proc/*/cmdline` inspection: no process whose executable was the fresh Colay or fake
  provider artifact.

No Docker command was used. `wsl.exe` intermittently printed a pre-existing systemd user-session
startup warning, but direct Linux commands ran and returned their expected exit codes; it did not
affect the isolated daemon lifecycle or state evidence.

## Evidence paths

Primary captured outputs:

- `/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-qa/evidence/migrate.json`
- `.../evidence/eligible-claude.json`
- `.../evidence/codex-nongit.json`
- `.../evidence/fallback-gemini.json`
- `.../evidence/failure-codex.json`
- `.../evidence/database.json`
- `.../evidence/daemon-online.json`
- `.../evidence/daemon-stop.json`
- `.../evidence/daemon-stopped.json`
- `.../evidence/final-invariants.json`

Provider identity evidence is in `.../logs-evidence/fake-claude-cli.log` and
`.../logs-evidence/fake-codex-cli.log`; the exact marker is
`.../tmp-evidence/colay-fake-conversation-starts.json`.

## Documentation scope

This refresh changes only `docs/qa/wsl-nightly-error-tracker.md` and this final report. Migrations,
production code, fixtures, tests, and prior reports are unchanged. The final verification and commit
are recorded in the task handoff.
