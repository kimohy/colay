# Colay WSL/Windows Nightly Error Tracker

## 2026-08-02 PR #16 deployed-nightly verification

- PR #16 passed duplicate push/pull-request CI matrices on Ubuntu, macOS, and Windows, then merged
  as `8f2654ac43d2f8260cd529790f62357f7059523a`. Release run `30749716792` passed classify,
  all three native builds, immutable bundle validation, all three platform smoke jobs,
  attestation, and npm publication. Public `nightly` resolved to
  `0.1.1-nightly.20260802.8f2654a`, matching the merge commit prefix.
- WSL 2 Ubuntu 24.04 clean-installed that exact nightly into the fresh timestamped npm prefix
  `/home/kimohy/.cache/colay-nightly-8f2654a.RxBSYz/prefix`, using the user's existing Node
  `22.23.1` only as the launcher runtime. The installed Linux package was the matching version and
  selected an x86-64 static PIE native binary. The existing global npm installation was not
  changed.
- A private copy of the affected schema-8 legacy store and an isolated `COLAY_HOME` reproduced the
  upgrade boundary without mutating user state. Pre-migration `doctor` exited 0, reported
  `legacy_import.pending = true`, source schema 8, and `inference_requests = 0`. `migrate apply`
  created schema 16. `daemon start` then reached IPC `online` from the deployed nightly instead of
  returning `user daemon contenders exited before IPC readiness`.
- Read-only database inspection found one completed import ledger row, 51 imported rows, and the
  historical graph preserved with `status = invalid`. Source and global integrity checks returned
  `ok` with zero foreign-key violations. The original and copied legacy database SHA-256 remained
  identical before and after:
  `d6a7c0dbd90b0109fa500c80ef77963726a6659eb87e52520c05e0b57aed22bc`.
- Bounded manual provider QA exposed two follow-ups. Real Codex `0.146.0` completed inference but
  returned an `answer_complete` object with `response` rather than the strict
  `response_redacted` field, so Colay correctly failed closed but the conversation was unusable
  (`WSL-020`). Claude reached its CLI and reported quota/billing unavailable; Gemini reached its
  CLI and reported authentication unavailable. Those two account states are not Colay defects.
- After the completed import and daemon stop, offline `doctor` still reported the same fingerprint
  as `legacy_import.pending = true`, despite the matching durable ledger row (`WSL-021`). Final
  daemon status was stopped; all QA state remains isolated under the timestamped QA root.

## 2026-08-02 deployed-nightly and real-provider QA refresh

- PR #13 merged as `7380e7ee94c3fc32c730d112834d37fb977d6d5c`. Its first release run failed
  because the release staging test still expected state schema 15 while the generated manifest
  correctly reported schema 16. PR #14 changed that contract to 16 and merged as
  `4dc8708c5dd79152c868dcf1a5549a741d76396d` after all six push/PR Ubuntu, macOS, and Windows CI
  jobs passed.
- Release run `30705747645` passed classify, all three native builds, validation, all three smoke
  jobs, and attestation. Its first npm publication attempt published the Darwin package but failed
  closed because npm did not expose the immutable integrity within the six 2-second retry delays.
  A failed-job rerun safely recognized the existing matching package, published the remaining
  packages, and completed successfully. Public `nightly` then resolved to
  `0.1.1-nightly.20260801.4dc8708`, and the root plus all three native packages reported that exact
  version and a registry integrity.
- `REL-001` (medium, source-fixed; next-nightly verification pending): the production registry
  visibility policy allowed only about 12 seconds of propagation. The release remains fail-closed
  and idempotent, but the fixed bounded policy keeps seven reads/six retries and increases the
  delay to 20 seconds, for a maximum 120-second propagation window. A unit contract fixes the
  attempts, delay, and total window.
- Clean WSL 2 Ubuntu 24.04 QA installed the deployed nightly and an isolated Node `22.23.2` under
  `/home/kimohy/.colay-qa/20260802-0042`; the host Node `18.19.1` was intentionally excluded because
  Colay requires Node 22 or newer. `colay --version` matched the npm nightly. The workspace was a
  Linux-native non-Git directory and never gained `.colay` or any other file.
- With a fresh isolated `COLAY_HOME`, `doctor` and `compatibility` exited 0 with
  `inference_requests = 0`. Claude `2.1.217` and Agy `1.1.8` were binary-compatible while account
  readiness remained explicitly unverified. `migrate apply` created only the user-global database
  and applied schema 1 through 16 without a local config file. Daemon start, status, restart, final
  status, and stop all succeeded; restart replaced both the instance ID and PID.
- Three bounded manual real-provider calls were made outside automated tests/CI. Requested Claude
  reached the provider and returned the concise quota/billing-unavailable classification. Requested
  Agy reached the provider and returned the concise incompatible read-only protocol classification.
  Plain `colay run hello` in the non-Git workspace entered conversation routing and reached Claude,
  rather than returning a Git repository or revision error. No raw provider stack, evidence dump,
  unsafe permission bypass, credential, task, or worktree was produced.
- After daemon cleanup, SQLite reported `integrity_check = ok`, zero foreign-key violations, and
  schema 16. One workspace, one session, and three redacted failed conversation attempts all used
  the same `workspace_id`; tasks, worktrees, and command evidence were zero. The global state
  directory and database modes were `0700` and `0600`, and final daemon status was stopped.

## Cross-platform legacy IPC lint refresh: 2026-07-27

The legacy named-pipe endpoint identity path is Windows-only.  Source commit
`5ef9ed7bb07353226f9aa3966c0011bc78c0f955` now conditionally compiles its
endpoint variant, state identity lookup, and legacy response validation only on
Windows (while retaining the validation helper for unit tests). Unix retains
only schema-v1 primary-socket readiness. This removes the Linux/macOS dead-code
warnings without lint allowances or a transport behaviour change.

- Windows: `cargo fmt --all -- --check`, full workspace/all-target/all-feature
  Clippy with `-D warnings`, and `cargo test --workspace --all-features` passed
  (the final full suite completed in 620.8 seconds).
- WSL Ubuntu 24.04 using the existing Rust/Cargo 1.95 cache: exact
  `cargo clippy --workspace --all-targets --all-features -- -D warnings` passed.
- A fresh release build in
  `/home/kimohy/.cache/colay-cross-platform-fix-20260727-target` produced
  `colay` SHA-256
  `e86e22ea689ebcefcd5dd71cfc6711e8b7e16be8532a1fee5c7811211e490bf0`,
  Build ID `01921686334714a54f4c39bb229c05e946c80563`, and target
  `linux/x86_64`. An isolated fake-provider-only runtime check migrated state,
  started an online daemon from that binary, stopped it, and confirmed the
  socket was absent afterwards. No real provider executable was invoked.

## WSL-014: non-Git plan-only Codex invocation omits the public Git-check bypass

- Severity: high
- Status: fixed
- Observed nightly: `0.1.1-nightly.20260726.46acc8d`

### Evidence and root cause

Real-provider QA in an isolated non-Git WSL workspace selected Codex `0.145.0`, but the
conversation ended as `Crashed { exit_code: Some(1) }`. Replaying Colay's exact public CLI
arguments reproduced the provider error:

```text
codex exec --json --sandbox read-only -C <non-git-workspace> -
Not inside a trusted directory and --skip-git-repo-check was not specified.
```

`CodexInvocation::exec` emits `exec --json --sandbox ... -C ... -` but does not emit
`--skip-git-repo-check`. This conflicts with Colay's conversation-first contract, which allows
read-only plan conversations before a Git repository exists. Adding that public option passed the
Git gate and reached authentication, proving the immediate cause; the subsequent request was
blocked by the local Codex login's expired/reused refresh token.

### Expected correction and verification

- Gate `--skip-git-repo-check` by observed capability and use it only for read-only conversation
  or planning. It is safe to include for Git workspaces and avoids a second Git probe.
- Keep writable task Git preflight unchanged.
- Add fake-provider contract coverage for the exact argv in Git and non-Git workspaces.
- In manual WSL QA, a valid Codex login must produce an `answer_complete` conversation attempt.

### Verified correction: 2026-07-27

Source HEAD `d88cc2e30c64ad91e7edae3a5edcad3e99e01eba` was built with Linux Rust 1.95
into a new ext4 target. In a non-Git ext4 workspace, the fake Codex executable recorded:

```text
exec --skip-git-repo-check --json --sandbox read-only -C \
  /home/kimohy/.cache/colay-task4-d88cc2e-qa/workspaces/codex \
  --model gpt-5.6-terra -c model_reasoning_effort="medium" -
```

The CLI exited 0, and the schema-v16 attempt was `provider_id = codex`,
`status = succeeded`, `outcome = answer_complete`. The workspace contained neither `.git` nor
`.colay`, and the database contained zero tasks, task attempts, worktrees, coordinator leases, and
worker leases. The option was advertised by the test-only capability wrapper and the process was the
compiled `colay-e2e-fake-provider`; no real provider or account state was used.

## WSL-015: `run --plan-only --provider` records a preference but ignores it for execution

- Severity: high
- Status: fixed
- Observed nightly: `0.1.1-nightly.20260726.46acc8d`

### Evidence and root cause

With all default providers enabled, `colay --json run --plan-only --provider claude PING` returned
`requested_provider: "claude"`, while the durable `conversation_attempts.provider_id` for the new
attempt was `codex`. `OfficialCliConversationOrchestrator::converse` always calls
`self.planner.primary_provider()`; the requested provider stored in the requirement envelope is not
part of `ConversationRequest` provider selection. Disabling Codex and raising Claude's configured
priority made the next attempt use Claude, confirming that configured priority, not `--provider`,
controls execution.

### Expected correction and verification

- Carry the requested provider as typed routing input into the conversation request.
- Honor an eligible requested provider. When it is disabled, missing, or incompatible before process
  start, fall back by configured priority and report the requested and selected providers.
- Never fall back to another provider after the selected provider process starts.
- Assert CLI output, attempt `provider_id`, and spawned fake binary identity all agree.

### Verified correction: 2026-07-27

`colay --json run --plan-only --provider claude 'qa eligible route'` exited 0 with
`requested_provider = claude`; the durable attempt was `provider_id = claude`,
`status = succeeded`, `outcome = answer_complete`, and the Claude fake marker recorded exactly one
`--permission-mode plan` invocation. In a separate non-Git workspace,
`--provider gemini 'qa fallback route'` requested a disabled provider and exited 0 with:

```text
Requested provider gemini is unavailable; using codex for this read-only turn.
```

That attempt was persisted as `provider_id = codex` and `status = succeeded`. No Gemini wrapper log
was created, while the Codex fake marker recorded the selected process, proving fallback occurred
before provider start rather than as runtime failover.

## WSL-016: distinct provider failures collapse to success plus generic needs-attention

- Severity: high
- Status: fixed
- Observed nightly: `0.1.1-nightly.20260726.46acc8d`

### Evidence

Real Codex, Claude, and Gemini attempts all exited 1 for different reasons:

- Codex: non-Git trust check; after bypass, local authentication returned HTTP 401 with
  `refresh_token_reused`/`token_expired`.
- Claude: the CLI initialized successfully, then returned `billing_error` with
  `Credit balance is too low`.
- Gemini: the CLI returned `IneligibleTierError` / `UNSUPPORTED_CLIENT` for the current individual
  Code Assist tier.

Colay persisted every attempt as `status = succeeded`, `error_redacted = NULL`, with the same
`outcome = needs_attention` and only `conversation lifecycle ended in Crashed { exit_code: Some(1) }`
as evidence. The CLI itself exited 0 and displayed the same generic message for all three cases.

### Expected correction and verification

- Persist a terminal failure status when the provider process crashes before a valid
  `ConversationOutcome` is parsed.
- Retain a bounded, redacted provider error classification and actionable message (authentication,
  quota/billing, unsupported account/client, compatibility, or process failure).
- Keep secrets and raw credentials out of state and output.
- Add fake JSONL/stream-JSON fixtures for these terminal cases and assert CLI exit semantics,
  attempt status, and redacted diagnostics.

### Verified correction: 2026-07-27

`colay --json run --plan-only --provider codex 'scenario:crash qa terminal'` exited 1. The fake
conversation marker changed from 3 to 4, so exactly one provider conversation process started. The
schema-v16 attempt was `provider_id = codex`, `status = failed`, and persisted a
`needs_attention` recovery outcome. The following are selected excerpts from the stored response
and evidence; they are not one contiguous diagnostic value:

```text
codex process failed. Review the redacted evidence, then retry this conversation.
conversation lifecycle ended in Crashed { exit_code: Some(17) }
fake conversation provider crash
```

The corresponding `request_conversation_turn` command was also `failed`; no secret value or raw
credential appeared. Database integrity was `ok`, foreign-key violations were zero, and tasks, task
attempts, worktrees, coordinator leases, and worker leases all remained at zero.

## WSL-014 through WSL-016 closure record: 2026-07-27

- Source identity: `d88cc2e30c64ad91e7edae3a5edcad3e99e01eba`, local source version
  `colay 0.1.0`. No nightly was published. The issue fixes are anchored by `afba6c3` (Codex argv),
  `24e2f61` and `cddd95b` (provider routing/preflight), and `f1e8a18` plus `6e5ff83` (terminal
  failure persistence and validation); the closure build includes all reviewed follow-up commits
  through `d88cc2e`.
- Final exact-HEAD Windows gates passed:
  `cargo fmt --all -- --check`;
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`; and
  `cargo test --workspace --all-features` (all workspace and doc tests, 560.3 seconds). Focused
  exact-HEAD checks also passed:
  `cargo test -p colay --test global_concurrency --all-features -- --nocapture` (6),
  `cargo test -p colay --test global_daemon --all-features -- --nocapture` (7),
  `cargo test -p colay --test daemon_lifecycle --all-features -- --nocapture` (5), and
  `cargo test -p orchestrator-daemon --all-features -- --nocapture` (29 unit, 15 conversation,
  1 integration). The issue-focused Windows matrix had previously passed 161 tests with zero
  failures.
- Linux-native build command:
  `cargo build --locked --offline --release --features test-fixtures --bin colay --bin
  colay-e2e-fake-provider`, with `CARGO_TARGET_DIR=/home/kimohy/.cache/colay-task4-d88cc2e-target`.
  It completed in 257.6 seconds. The x86-64 GNU/Linux ELF `colay` SHA-256 was
  `67665fee4e779beb3732497656ff93e6da1188e391c40c0c2dde6df5623c6657`; the fake-provider SHA-256
  was `8f08d163f99f16f9f8c57dacacb91678bdf90a6ec89575d5bfb4d74d1b6c9170`.
- WSL environment: Ubuntu 24.04 on WSL 2 kernel `6.18.33.2-microsoft-standard-WSL2`, Linux Rust
  1.95.0, isolated ext4 `COLAY_HOME=/home/kimohy/.cache/colay-task4-d88cc2e-qa/home-evidence`,
  ext4 cache/target, and four separate non-Git workspaces. `COLAY_TEST_FAKE_PROVIDERS_ONLY=1` was
  set for every Colay command. `colay --json migrate apply` migrated a fresh database from schema 0
  through 16 with the committed checksums and no pending versions.
- Final database evidence: four sessions and four attempts in order—Claude succeeded
  `answer_complete`, Codex succeeded `answer_complete`, disabled-Gemini request selected Codex and
  succeeded `answer_complete`, and Codex crash failed with `needs_attention`. The fake conversation
  marker was 4 total; the failure-only delta was one. No real Codex, Claude, Gemini, or Agy inference
  ran, and no credential or provider-account state was accessed.
- Cleanup: online daemon PID 1735 reported executable
  `/home/kimohy/.cache/colay-task4-d88cc2e-target/release/colay` and target `linux/x86_64`.
  `colay --json daemon stop` returned `state = stopped`; follow-up status remained stopped, PID 1735
  no longer existed, and the isolated Unix socket was released.

### Clean final-HEAD WSL refresh: 2026-07-27

- Exact source HEAD: `9779bd601159fc910ccf2b6614a17fd8cd20bb16`. The worktree was clean
  before the build. No nightly was published.
- Fresh Linux-native release command:
  `cargo build --locked --offline --release --features test-fixtures --bin colay --bin
  colay-e2e-fake-provider`, using Linux Rust/Cargo 1.95.0, `CARGO_INCREMENTAL=0`, and the new ext4
  target `/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-target`. It exited 0 in 255.7
  seconds.
- The resulting `colay 0.1.0` x86-64 GNU/Linux ELF has Build ID
  `58e434090b273ad6ba71645005aeb2918f8f0efb` and SHA-256
  `ebe7578edf7cd9a72c4d0b7d78fc4c889803a4516f6473ad27dd994704a7eb94`. The compiled
  `colay-e2e-fake-provider` SHA-256 is
  `8f08d163f99f16f9f8c57dacacb91678bdf90a6ec89575d5bfb4d74d1b6c9170`.
- The clean QA root was `/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-qa`, with isolated
  `COLAY_HOME=.../home-evidence`, `TEMP`/`TMP=.../tmp-evidence`, and four separate non-Git
  workspaces (`eligible`, `codex`, `fallback`, `failure`). None contained `.git` or `.colay` after
  QA. Every Colay command set `COLAY_TEST_FAKE_PROVIDERS_ONLY=1`; configured executable paths were
  test-only wrappers delegating to the freshly compiled fake binary. No real provider, credential,
  account, endpoint, Docker command, push, or merge was used.
- `colay --json migrate apply` exited 0 and created the per-user schema-v16 database with all 16
  committed checksums and no pending versions. Final read-only checks reported integrity `ok`, zero
  foreign-key failures, four sessions, four conversation attempts, and zero tasks, task attempts,
  worktrees, coordinator leases, and worker leases.
- `--provider claude 'qa eligible route final 9779bd6'` exited 0, persisted
  `provider_id=claude`, `status=succeeded`, `outcome=answer_complete`, and produced exactly one
  Claude conversation argv containing `--permission-mode plan`.
- `--provider codex 'qa codex route final 9779bd6'` exited 0 and persisted Codex success. The direct,
  fallback, and failure Codex conversation argv records all contained
  `exec --skip-git-repo-check --json --sandbox read-only -C <non-Git-workspace> ... -`.
- Disabled `--provider gemini 'qa fallback route final 9779bd6'` exited 0, persisted Codex success,
  retained the exact notice `Requested provider gemini is unavailable; using codex for this
  read-only turn.`, and created no Gemini log. The Codex wrapper recorded the selected process,
  proving fallback before provider start.
- `--provider codex 'scenario:crash qa terminal final 9779bd6'` exited 1. The fake conversation
  marker changed from 3 to 4. The durable attempt was Codex `failed` with `needs_attention`; its
  exact `response_redacted` was `codex process failed. Review the redacted evidence, then retry this
  conversation.` The exact 792-byte `error_redacted` and 701-byte evidence are recorded verbatim in
  `evidence/final-invariants.json`; the corresponding command state was `failed`.
- The daemon reported online PID 1183, executable from the fresh target, build `0.1.0`, and target
  `linux/x86_64`. `daemon stop` and follow-up status both exited 0 with `state=stopped`; `/proc/1183`
  and the isolated `runtime/daemon.sock` were absent, and exact-process inspection found no Colay or
  fake-provider process from this target.
- Primary evidence paths are
  `/home/kimohy/.cache/colay-task4-final-9779bd6-20260727-qa/evidence/{migrate.json,eligible-claude.json,codex-nongit.json,fallback-gemini.json,failure-codex.json,database.json,daemon-online.json,daemon-stop.json,daemon-stopped.json,final-invariants.json}`;
  full wrapper logs are under `logs-evidence/`, and the marker is
  `tmp-evidence/colay-fake-conversation-starts.json`.

## Real-provider QA record: 2026-07-27

- Environment: WSL 2 Ubuntu 24.04, isolated `COLAY_HOME`, non-Git Linux-native workspace.
- Colay: `0.1.1-nightly.20260726.46acc8d`.
- Codex: `0.145.0`; public CLI started, but no model response because local authentication requires
  sign-in again.
- Claude: `2.1.217`; provider stream initialized, but no model response because the account credit
  balance is too low.
- Gemini: `0.51.0`; provider process started, but the configured individual tier rejects this client
  as unsupported.
- Agy: not installed, so no real inference attempt was possible.
- Automated tests/CI remain fake-provider-only as required. These were bounded manual QA calls.

## Clean-install real-provider QA refresh: 2026-07-28

- Public npm `nightly` still resolved to `0.1.1-nightly.20260726.46acc8d`; it does not contain the
  reviewed WSL-014 through WSL-016 fixes. The isolated Linux native binary SHA-256 was
  `aed1775eb1e3fc15b0eb0a605e4f186f09f4bb0e69344ca3c10dab9f0d153743`, with ELF Build ID
  `a284e7cdb28951622c359b91e0ea9b7c7bef618b`.
- A clean npm install under WSL's default Node `18.19.1` completed with `EBADENGINE` instead of
  failing, then the installed `colay` command refused to start because Node 22 or newer is
  required. Adding an isolated Node `22.23.1` to PATH made the same installation run normally.
  This is consistent with WSL-001's documented runtime boundary, but the install-success then
  first-run-failure sequence remains noticeable user friction.
- A fresh isolated `COLAY_HOME` migrated from schema 0 to 15. `doctor` reported database integrity
  `ok`, zero foreign-key violations, and the daemon started online from the installed nightly.
  Four separate non-Git workspaces contained neither `.git` nor `.colay`.
- Compatibility probes reported Claude `2.1.217`, Gemini `0.51.0`, and Agy `1.1.5`/`1.1.7` as
  healthy; Codex was correctly disabled because the Windows npm installation lacked the Linux
  native optional package.
- Bounded manual `run --plan-only --provider` calls requested Claude, Gemini, Agy, and Codex. All
  four commands exited 0 and all four durable attempts used `provider_id=claude`,
  `status=succeeded`, and the same generic `needs_attention` outcome after the Claude process
  exited 1. This reconfirms that WSL-015 and WSL-016 are source-fixed but not deployed in the
  current public nightly.
- Direct adapter-equivalent manual calls established the external causes without exposing
  credentials: Claude authentication initialized but billing returned `Credit balance is too
  low`; Gemini rejected the current individual account/client as `UNSUPPORTED_CLIENT`; Agy exited
  0 without an answer because headless plan mode auto-denied a required command permission; and
  Codex could not start from the cross-OS npm installation.
- The reviewed source candidate (`5ef9ed7`, Linux binary SHA-256
  `e86e22ea689ebcefcd5dd71cfc6711e8b7e16be8532a1fee5c7811211e490bf0`) was checked against the
  same real providers in a second isolated state root. It selected Claude, Gemini, and Agy exactly
  as requested, returned exit 1 for each provider failure, and persisted three schema-v16
  `status=failed` attempts with provider-specific bounded recovery messages. Database integrity
  was `ok`, foreign-key violations were zero, and tasks, task attempts, worktrees, coordinator
  leases, and worker leases remained zero in both the public-nightly and candidate databases.
- Both isolated daemons stopped cleanly. PIDs 2595 and 4392, all `daemon.sock` files, and all
  provider processes were absent after cleanup. Evidence is under
  `/home/kimohy/.cache/colay-real-provider-qa-20260728/evidence`.
- These were bounded manual user QA calls, not automated tests or CI. No credential value was
  printed or copied.

## WSL-017: compatibility health conflates public CLI probes with account readiness

- Severity: medium
- Status: fixed in source candidate; nightly verification pending
- Observed nightly: `0.1.1-nightly.20260726.46acc8d`
- Verified source candidate: `97a813b`

### Evidence and impact

`compatibility` and `doctor` marked Gemini, Claude, and Agy `healthy` after only version/help
capability probes. The immediately following real read-only calls could not produce an answer:
Gemini rejected the account/client tier, Claude had no billing credit, and Agy could not satisfy
its headless permission contract. `inference_requests=0` correctly describes the safe probe, but a
normal user can reasonably interpret `healthy` as ready for a task.

### Expected correction

- Separate binary compatibility from account/runtime readiness in the status vocabulary.
- When a provider exposes a non-inference authentication or entitlement status command, include it
  in `doctor` without printing account identifiers or credentials.
- Otherwise report `compatible; account readiness unverified` rather than `healthy`, and keep an
  explicitly opted-in live check separate because it can consume quota or incur cost.

### Source candidate verification (2026-07-31)

- Built the candidate natively on Ubuntu 24.04 WSL2 with Rust 1.95.0 and an isolated
  `COLAY_HOME`. `compatibility` and `doctor` made zero inference requests.
- Claude 2.1.217 and Agy 1.1.7 remained binary-compatible/healthy while the new independent
  `account_readiness.status` was `unverified`. Every provider doctor check was `warn` with the
  explicit detail `account readiness unverified`; `doctor` itself completed successfully.
- `migrate apply` created and migrated only the per-user global database to schema 16. The test
  workspace never gained a `.colay` directory.
- A deliberately opted-in live call then proved why the distinction matters: Claude reported
  unavailable quota/billing and Agy reported an incompatible read-only conversation protocol.
  Neither safe compatibility command inferred readiness from these account/runtime outcomes.

## WSL-018: provider failure evidence overwhelms the actionable diagnostic

- Severity: medium
- Status: fixed in source candidate; nightly verification pending
- Observed source candidate: `5ef9ed7`
- Verified source candidate: `97a813b`

### Evidence and impact

The source candidate correctly classified and failed all three provider attempts, but the Gemini
error appended repeated `unknown event: gemini.stderr` entries plus a provider-internal JavaScript
stack. The Agy evidence relayed its suggestion to use `--dangerously-skip-permissions`, even though
Colay must not recommend bypassing its read-only permission boundary. The useful one-line recovery
message appears first, but a terminal user then receives a long and potentially misleading raw
diagnostic.

### Expected correction

- Keep the concise classified recovery message on normal stderr and in the default TUI view.
- Deduplicate unknown-event summaries and bound stack output by lines as well as bytes.
- Store detailed redacted evidence for an explicit diagnostic view, but suppress provider advice
  that asks users to disable permission controls; replace it with Colay-safe configuration guidance.

### Source candidate verification (2026-07-31)

- Real Claude and Agy plan-only calls each exited 1 with one concise actionable message. Normal
  stderr contained no provider stack, `Evidence:` section, repeated unknown-event lines, or unsafe
  permission-bypass flag.
- The global database persisted both attempts as `failed` with the same concise classified error.
  Detailed redacted evidence was bounded to 120 bytes/3 lines for Claude and 251 bytes/4 lines for
  Agy. Neither outcome contained `--dangerously-skip-permissions`; the Agy evidence contained the
  Colay-owned safe replacement `unsafe permission bypass`.
- After stopping the isolated daemon, SQLite reported `integrity_check=ok`, zero foreign-key
  violations, zero tasks/task attempts/worktrees/coordinator leases/worker leases, one registered
  workspace, and two conversation attempts. The workspace-local `.colay` path remained absent.
- This was bounded manual user QA, not an automated test or CI inference. No credential value was
  printed or copied. Supporting JSON and database evidence is under
  `/home/kimohy/.cache/colay-readiness-qa-20260731`.

## WSL-019: unsealed legacy invalid graph prevents daemon startup

- Severity: high
- Status: fixed
- Observed nightly: `0.1.1-nightly.20260801.3f4e2f7`
- Observed executable: `/home/kimohy/.nvm/versions/node/v22.23.1/bin/colay`
- Verified source candidate: `72d9bc6315b1c57d92b27231be4e94c5a5000b68`

### Evidence and root cause

These opening observations come from the controller's pre-implementation read-only reproduction
of the public nightly, not from the later isolated source candidate. User-provided command evidence
established the executable and version above and the generic pre-IPC symptom that every daemon
contender exited with status 1. The controller's public-nightly `doctor` exited 0 and reported
user-global root `/home/kimohy/.local/state/colay`, database `state.db`, schema 16, integrity true,
zero foreign-key violations, daemon stopped, `/home/kimohy` registered, and
`inference_requests = 0`. Public-nightly daemon start reproduced the same generic contender exit
and left no daemon process or `daemon_instances` row. A bounded foreground daemon run exited 1
before IPC and identified a missing required `node_count` field.

The controller's read-only source query of `/home/kimohy/.colay/orchestrator.db` reported
authoritative schema 8 and one legitimate unsealed graph attempt with `status = invalid`, absent
proposal JSON/hash, and validation top-level key `errors` containing an array of count 1. No prompt
content or historical validation message was printed or recorded.

The source database is authoritatively schema 8. Legacy inspection upgrades only its guarded
private snapshot through schema 13 before graph validation; earlier wording that called the source
itself schema 13 conflated those two stages. `validate_source_graphs` then unconditionally decoded
every `validation_json` value as `GraphValidationSummary`, even though that typed successful-plan
shape requires `node_count` and an unsealed invalid attempt deliberately retains arbitrary
structured validation evidence. The importer rejected the valid historical row before the daemon
could register its lease or publish IPC.

Source commit `89b9edb` now parses validation as JSON first and deserializes
`GraphValidationSummary` only when both proposal and hash exist. Unsealed rows preserve their
validation evidence and require absent row-level authority. Final-review safety commit `c48e4c9`
accepts that unsealed shape only for `planning`, `invalid`, `cancelled`, and `superseded`, rejects
unsealed `awaiting_approval` and `approved`, and null-safely binds every approval proposal/session/
requirement/validation/base field to the sealed revision. Incomplete proposal/hash pairs, malformed
JSON, seal/identity/authority mismatches, and approvals of unsealed revisions continue to fail
before target mutation. Commits `360d3c8` and `72d9bc6` add read-only legacy import readiness to
`doctor` and point bounded contender exits to that diagnostic; final-review safety commit
`029ca24` replaces repository-controlled importer errors with fixed, actionable, source-value-free
details capped at 256 characters.

### Source candidate verification: 2026-08-02

- Focused Windows gates at exact source HEAD passed: format and diff checks; legacy import `26/26`
  in 220.6 seconds; global doctor `16/16` in 53.4 seconds; global daemon `7/7` in 43.4 seconds;
  and daemon lifecycle `5/5` in 21.7 seconds. Required format passed in 4.1 seconds and
  all-target/all-feature Clippy with `-D warnings` passed in 10.7 seconds.
- The first full Windows workspace run reached the existing `WIN-003` environment flake: two
  tests received OS error 5 while launching trusted `icacls.exe`. Each failed test then passed
  alone three times. A fresh-target audit later reproduced the same spawn-denial class in an
  unrelated test, while a controller-side sequential read-only `icacls.exe` probe passed `20/20`
  and found no active Cargo or Colay process. The final exact normal-target
  `cargo test --workspace --all-features` run passed every workspace target, feature, integration
  test, and doc test in 735.6 seconds. No product change was made for `WIN-003`.
- WSL used Ubuntu 24.04 on WSL2 kernel `6.18.33.2-microsoft-standard-WSL2` with Linux Rust/Cargo
  1.95.0. An initial byte copy of the Windows worktree retained CRLF migration bytes and correctly
  failed checksum validation; that QA root is preserved as harness evidence. The corrected source
  was exported from Git object bytes at exact HEAD into
  `/home/kimohy/.cache/colay-wsl019.XajHfk`. Exported `migrations/0001_core.sql` contained zero
  CRLF sequences and had SHA-256
  `28d9a2ec035472bc31df087dc579b24e23b280ccfcd1557ea2649ebe67266305`.
- The fresh Linux-native build completed in 109 seconds. Candidate `colay` SHA-256 was
  `cde795a0a003375015a19d4709d776ca94b0792db77239da137765412c50d734`, ELF Build ID
  `81633da4f86a6aad97af821ffc579e38586e06b6`, version `0.1.0`, target `linux/x86_64`.
  The compiled `colay-e2e-fake-provider` SHA-256 was
  `a585190ce4048dc508de16841f11fd2f1d0a174301d0f816f07a16542e8a5921`.
- With private copied state, isolated `COLAY_HOME`, and
  `COLAY_TEST_FAKE_PROVIDERS_ONLY=1`, `doctor` exited 0 in 14.263 seconds, reported the schema-8
  source import-ready, and reported zero inference requests. `migrate apply` exited 0 in 6.859
  seconds, created the schema-16 global database, and recorded one legacy import. Daemon start
  reached online in 15.527 seconds from the candidate executable; status was online, stop exited 0,
  and final status was stopped. Final exact-process and Unix-socket counts were both zero.
- Read-only SQLite inspection reported `integrity_check = ok` and zero foreign-key violations for
  both the copied schema-8 source and schema-16 global database. Each retained exactly one
  `invalid` graph with absent proposal/hash and an `errors` array; the global database had one
  workspace and one legacy-import ledger row. The original and copied source database SHA-256
  remained identical before and after:
  `d6a7c0dbd90b0109fa500c80ef77963726a6659eb87e52520c05e0b57aed22bc`.
- The required fake-only chat integration passed `4/4`. At the prior WSL source-candidate HEAD, the
  brief's literal `legacy_import` filter matched zero `global_doctor` test names, so the historical
  command completed with 16 tests filtered out; the substantive supplemental binary passed
  `16/16`. The final-review fix wave renamed the diagnostics and added the redaction regression:
  the same documented filter now selects and passes `3/3`, and the full amended `global_doctor`
  binary passes `17/17`. These tests use separate isolated state rather than the copied user
  configuration. No real Codex, Claude, Gemini, or Agy inference ran.

### Deployed-nightly verification: 2026-08-02

PR #16 merged as `8f2654ac43d2f8260cd529790f62357f7059523a` after all six PR CI checks passed.
Release run `30749716792` published `0.1.1-nightly.20260802.8f2654a`. A fresh WSL npm prefix,
private schema-8 source copy, and isolated `COLAY_HOME` reproduced the exact legacy boundary.
`doctor`, schema-16 migration, daemon start/status, import, stop, and final status all exited 0.
The daemon reached `online` with the deployed nightly executable; the global import ledger recorded
51 rows, and one unsealed `invalid` graph was preserved. Both databases passed integrity and
foreign-key checks, and the original source hash was unchanged. This closes the startup defect.
The post-import doctor display discrepancy is tracked separately as `WSL-021`.

## Standing deployed-nightly provider QA matrix

Every deployed-nightly validation must run the same bounded, read-only conversation lifecycle for
Codex, Claude, Gemini, and Antigravity (`agy`), then preserve the redacted outcome and confirm no
task or worktree was created. The source regression
`all_fake_providers_preserve_the_canonical_read_only_conversation_contract` performs that exact
four-provider contract only through `orchestrator-test-support`'s compiled fake runtime; it never
uses a PATH-installed provider CLI.

| Provider | Required deployed-nightly result | Readiness classification |
| --- | --- | --- |
| Codex | Canonical `answer_complete` with `response_redacted` | Product regression if the strict contract fails |
| Claude | Same read-only lifecycle and canonical outcome when the account is ready | Quota/billing/account-unready is an external readiness state, not a product defect |
| Gemini | Same read-only lifecycle and canonical outcome when the account is ready | Authentication/account-unready is an external readiness state, not a product defect |
| Antigravity (`agy`) | Same read-only lifecycle and canonical outcome when the account is ready | Account/protocol-unready is an external readiness state, not a product defect |

`WSL-020` and `WSL-021` remain deployment-pending until a fresh WSL nightly attaches this evidence;
source tests and prior implementation commits alone do not close either issue.

## WSL-020: real Codex answer omits the strict conversation response field

- Severity: high
- Status: fixed-in-source, deployment-pending
- Observed nightly: `0.1.1-nightly.20260802.8f2654a`
- Observed provider: Codex `0.146.0`

### Evidence and root cause

In the isolated non-Git WSL workspace, plain `colay run hello` selected Codex and completed one
read-only inference. The provider returned a concise `answer_complete` JSON object, but used the
field `response`. Colay's provider-neutral `ConversationOutcome` contract requires
`response_redacted` and denies unknown fields, so the attempt correctly persisted as terminal
`failed` with a bounded compatibility diagnostic:

```text
conversation output is not one strict outcome JSON object:
unknown field `response`, expected `response_redacted`
```

The compatibility probe had correctly verified the read-only sandbox and reported the output
schema option as advertised. The conversation request nevertheless set no output schema, and its
prompt named the internal `ConversationOutcome` type without enumerating the exact JSON shapes or
required field names. Fake-provider tests hard-code valid `response_redacted` fixtures and therefore
cannot detect this prompt-to-provider contract omission.

### Source correction and required deployment verification

- Source commit `8305abfbde34117353451836712800c8c3d734fa` adds the bounded provider-boundary
  `response` alias decoder and fail-closed normalization regressions while preserving
  `response_redacted` as the only canonical domain and persisted field.
- Source commit `190a2a1045f7cfc029081282d98906214b318dfd` puts the exact provider-neutral
  outcome shapes and required field names in the sealed read-only conversation request without
  weakening the strict deserializer. Its `conversation_prompt_lists_canonical_shapes` regression
  fails if `response_redacted` is omitted from that contract.
- The standing fake-only regression
  `all_fake_providers_preserve_the_canonical_read_only_conversation_contract` exercises Codex,
  Claude, Gemini, and Antigravity through the same canonical response and read-only lifecycle.
- Re-run a bounded real Codex turn from a fresh nightly and require a successful
  `answer_complete` attempt with no task or worktree creation before changing this status to
  `fixed`.

## WSL-021: doctor reports a completed legacy import as pending

- Severity: medium
- Status: fixed-in-source, deployment-pending
- Observed nightly: `0.1.1-nightly.20260802.8f2654a`

### Evidence and impact

After successful daemon startup/import and clean shutdown, offline `doctor` reported
`legacy_import.pending = true` for source fingerprint
`d66ffeb6ca0b4f2e6a4646b4adf5c6cbbde771c1247c65c5cd7d1e265c4967ab`. Read-only inspection of
the schema-16 global database found one `legacy_imports` row with that exact fingerprint and
workspace, `result_json.imported = true`, 51 imported rows, and the preserved invalid graph.

The import engine is correct, but the offline doctor projection describes source readiness without
correlating it with the durable import ledger. Users can therefore believe a completed import is
still pending and may repeat maintenance steps unnecessarily. Live-daemon doctor instead reports
that import readiness is unavailable through IPC, so neither mode currently presents a clear
"already imported" state.

### Source correction and required deployment verification

- Source commits `1d78d0e`, `f5e2082`, `c8e58e8`, `0482f26`, and `1326b65` validate durable import
  completion against the freshly inspected sealed source plan. The live daemon independently
  derives its effective configuration and re-inspects the source before returning completion
  evidence; it does not trust a client-selected state directory or fingerprint. No persisted schema
  version changed.
- The focused state and IPC regressions `legacy_import_completion`, `live_doctor_`, and
  `doctor_lookup` cover completed, pending, changed-source, and corrupt-evidence cases while
  preserving read-only doctor behavior.
- Attach a fresh deployed-nightly WSL run that reports completed import state after daemon startup
  and clean shutdown before changing this status to `fixed`.

## WSL-012: 최소 버전 이상 Codex가 exact-only 판정으로 safe mode에 고정됨

- 심각도: high
- 상태: fixed
- 발견 nightly: `0.1.1-nightly.20260726.5b1a207`

### 관찰 및 원인

WSL에 nightly를 설치한 뒤 공개 probe만 수행한 `colay compatibility`에서 Codex
`0.145.0`의 exec, App Server, read-only/workspace-write sandbox 기능이 확인됐지만 최종
상태가 `untested`로 남았다. 그 결과 provider inference와 무관한 `colay migrate apply`도
safe mode에 의해 차단됐다. 사용자 전역 DB 변경의 별도 구현은 배포됐지만, 승인된
"최소 버전 충족 또는 필수 공개 기능 확인" 정책은 설계 문서에만 있고 `codex-compat`의
generic adapter 판정에는 연결되지 않은 것이 원인이었다.

### 수정 및 검증

- Codex `0.144.5`를 최소 지원 버전으로 두고, exact fixture가 없는 버전도 최소 버전 이상이면
  실제 probe에서 관찰된 기능 한계와 함께 `CompatibleWithWarnings`로 허용한다.
- 최소 버전 미만은 안전한 transport, read-only sandbox, workspace-write sandbox가 관찰된
  경우에만 writable generic adapter를 허용한다.
- 버전 판정이 누락 기능을 만들어내지 않도록 workspace-write sandbox가 명시적으로 없으면
  최소 버전 이상에서도 read-only로 제한한다.
- fake Codex `0.145.0` CLI 통합 테스트에서 `compatibility`가 degraded/eligible로 보고되고,
  inference request가 0이며, `migrate apply`가 safe mode에 막히지 않음을 확인했다.
- Windows에서 fmt와 전체 Clippy, npm 66개가 통과했다. 전체 Rust suite의 유일한 실패는
  기존 `WIN-003`의 간헐적 `icacls.exe` 접근 거부였고 같은 테스트를 단독 3회 재실행해
  모두 통과했다. WSL Ubuntu 24.04/Rust 1.95에서도 대상 통합 테스트와 전체 Clippy가
  통과했다.

### 배포 완료 검증

PR #8을 merge commit `209e6d25c7025784f8a0245da59bcbbf4d15dc66`으로 병합하고 nightly
`0.1.1-nightly.20260726.209e6d2`를 WSL에 설치했다. 실제 Codex `0.145.0` 공개 probe는
`degraded`와 writable `verified`, minimum version `0.144.5; met=true`를 보고했고
`inference_requests`는 0이었다. 격리된 `COLAY_HOME`에서 config 파일 없이 schema 0→15
migration이 성공했으며 기존 사용자 DB SHA-256
`d6a7c0dbd90b0109fa500c80ef77963726a6659eb87e52520c05e0b57aed22bc`는 유지됐다.

## WSL-013: 느린 startup/secondary workspace probe가 daemon restart 종료를 차단

- 심각도: high
- 상태: fixed
- 발견 nightly: `0.1.1-nightly.20260726.209e6d2`
- 추가 재현 nightly: `0.1.1-nightly.20260726.20b7654`

### 관찰 및 원인

격리된 사용자 전역 DB에서 첫 workspace로 daemon을 시작하고 두 번째 workspace에서
`status`를 호출한 직후 `daemon restart`를 실행하면 다음 오류가 두 번 재현됐다.

```text
error: user daemon did not release its singleton ownership within ten seconds
```

두 번째 workspace 등록 응답 후 activation 루프가 해당 workspace의 provider 공개 probe를
동기적으로 기다리고 있었다. WSL의 실제 probe는 약 50초가 걸릴 수 있는데, activation
branch 내부 await는 daemon cancellation을 관찰하지 않아 restart의 10초 종료 한도를
넘겼다. 오류 뒤 기존 daemon은 stopped가 됐지만 새 daemon은 시작되지 않아 restart 계약이
깨졌다.

### 수정 및 검증

- workspace runtime 준비 future와 daemon cancellation을 cancellation 우선 `select` 경계로
  감싸 stop/restart 시 진행 중인 공개 probe future를 즉시 drop한다.
- cancellation이 이미 발생한 경우 pending activation future가 drop되는 단위 테스트를
  추가했다.
- 두 번째 workspace에 15초 지연 fake Codex를 설정하고 activation 직후 restart하는 CLI
  subprocess 회귀 테스트를 추가했다. 수정 후 Windows에서 9.36초, WSL에서 8.36초에
  성공했다.
- 실제 provider inference는 호출하지 않는다.

### 1차 배포 검증 및 추가 재현

PR #9를 merge commit `b0864483f6bfa2da2a9f34b00786f11355edc4ce`로 병합했다. push/PR
CI의 Ubuntu, Windows, macOS 작업 6개와 merge commit CI, 세 플랫폼 release build/smoke,
attestation, npm publish가 모두 성공했다.

nightly `0.1.1-nightly.20260726.b086448`를 WSL Ubuntu 24.04에 정확한 버전으로 설치하고,
격리된 `COLAY_HOME`에서 schema 0→15 migration 후 두 directory workspace를 활성화했다.
두 번째 workspace의 공개 provider probe가 진행 중인 상태에서 측정 wrapper 오류로 약 1초
지연된 뒤 실행한 `daemon restart`는 약 3초 안에 exit 0으로 완료됐고 PID
`1501`→`1792`의 새 instance가 online이 됐다. 이어지는 status와 stop도 성공했다.

상태 DB는 사용자 전역 경로의 `home/state/state.db` 하나뿐이었고, `ws-one`과 `ws-two`는
각각 별도 `workspace_id` (`019fa004-aca4-7583-ac55-e251e08d00d2`,
`019fa004-d7ae-7be0-b248-21e7aee9d9f0`)로 분리됐다. workspace 내부 DB는 없었다. 실제
Codex `0.145.0` 공개 compatibility probe는 minimum `0.144.5; met=true`, writable
`verified`, `inference_requests: 0`을 보고했다. 기존 사용자 DB SHA-256도
`d6a7c0dbd90b0109fa500c80ef77963726a6659eb87e52520c05e0b57aed22bc`로 유지됐다.

그러나 문서 PR #10의 merge commit `20b76548470540942c61aed0695f94109c718e53` nightly를
새 격리 환경에 설치해 `start`→두 번째 workspace `status`→`restart`를 지연 없이 한
shell에서 실행하자 10초 singleton ownership 오류가 다시 재현됐다. 첫 수정은 secondary
workspace activation probe만 취소했다. 실제로는 daemon이 DB phase를 `online`으로 바꾼 뒤
첫 workspace의 provider probe를 계속 수행하는 startup 경로도 있었고, 이 probe는 daemon
cancellation 경계 밖이었다. `start`가 online을 반환한 직후 restart하면 이 초기 probe가
종료를 막았다.

두 번째 수정은 startup workspace planner probe도 cancellation 우선 경계로 감싸고,
cancellation 시 startup lease를 해제한 뒤 정상 종료한다. 초기 workspace에 15초 지연 fake
Codex를 설정하고 probe 시작 marker 직후 restart하는 회귀 테스트를 추가했다. 수정 전에는
실패했고 수정 후 Windows에서 통과했다.

PR #11을 merge commit `b2daed02a27a128b43984bab0eedeca6d60324e4`로 병합하고 nightly
`0.1.1-nightly.20260726.b2daed0`를 WSL에 정확한 버전으로 설치했다. 새 격리
`COLAY_HOME`에서 schema 0→15 migration, 첫 workspace start, 두 번째 workspace status,
지연 없는 즉시 restart, status, stop을 한 shell에서 실행해 11초 전체 안에 모두 exit 0을
확인했다. restart 뒤 instance는 online이었고 새 PID `1499`를 보고했다.

전역 DB는 `home/state/state.db` 하나였으며 workspace 내부 DB는 없었다. 두 workspace는
각각 `019fa059-aab7-7d70-8e58-e757c22121d0`,
`019fa059-ae91-70e3-8482-b0cc625b3ffc`로 분리됐다. 공개 compatibility probe는 Codex
`0.145.0`, minimum `0.144.5; met=true`, writable `verified`, `inference_requests: 0`을
보고했고 기존 사용자 DB SHA-256도
`d6a7c0dbd90b0109fa500c80ef77963726a6659eb87e52520c05e0b57aed22bc`로 유지됐다.

이 문서는 WSL Linux와 Windows에서 nightly Colay를 실제 사용하면서 발견한 오류와 개선
후보를 지속적으로 누적하는 메모다. 오류를 재현했다고 해서 수정 완료로 간주하지 않으며,
각 항목은 증거, 영향, 임시 우회, 제품 개선안, 검증 조건을 분리해서 기록한다. 기존
`WSL-*` ID는 이력 안정성을 위해 유지하되 Windows에서도 재현되면 공통 이슈로 표시한다.

## Tracking metadata

- 최초 작성: 2026-07-22 (Asia/Seoul)
- 마지막 갱신: 2026-08-02
- 대상 환경: WSL 2 Ubuntu 24.04 x86-64, Windows 11 Home 10.0.26100 x86-64
- 확인한 nightly: `0.1.1-nightly.20260722.f693062`, `0.1.1-nightly.20260723.7a977cf`,
  `0.1.1-nightly.20260726.7a45d97`, `0.1.1-nightly.20260726.209e6d2`,
  `0.1.1-nightly.20260726.b086448`, `0.1.1-nightly.20260726.20b7654`,
  `0.1.1-nightly.20260726.b2daed0`, `0.1.1-nightly.20260726.46acc8d`,
  `0.1.1-nightly.20260801.3f4e2f7`, `0.1.1-nightly.20260802.8f2654a`
- Windows PATH 설치본: Cargo 설치 `colay 0.1.0` (nightly와 불일치)
- 기본 원칙: 자동화 테스트와 CI는 fake provider만 사용한다. 실제 provider inference는 사용자가
  명시적으로 승인한 격리 수동 QA에서만 제한적으로 호출한다.
- 상태 값: `open`, `workaround-confirmed`, `fix-in-progress`, `fixed`, `closed`

## Issue index

| ID | 심각도 | 상태 | 요약 |
| --- | --- | --- | --- |
| `WSL-014` | high | fixed | non-Git plan-only Codex invocation omits `--skip-git-repo-check` |
| `WSL-015` | high | fixed | `--provider` preference is recorded but ignored for conversation execution |
| `WSL-016` | high | fixed | provider failures are persisted as succeeded and reduced to generic needs-attention |
| `WSL-019` | high | fixed | unsealed legacy invalid graph prevents daemon startup |
| `WSL-020` | high | fixed-in-source, deployment-pending | real Codex output is rejected because the exact conversation JSON field contract is not supplied |
| `WSL-021` | medium | fixed-in-source, deployment-pending | doctor reports an already completed legacy import as pending |
| `WSL-001` | medium | fixed | NVM/Node 버전 및 비대화형 PATH 불일치 |
| `WSL-002` | high | fixed | daemon startup phase, bounded probe wait, exact child cleanup 적용 |
| `WSL-003` | high | fixed | WSL/Windows idle daemon의 반복 `BEGIN IMMEDIATE`로 direct writer starvation |
| `WSL-004` | high | fixed | WSL/Windows non-Git 위치에서 task 영속화 후 raw Git 128 오류 |
| `WSL-005` | high | fixed | WSL/Windows unborn HEAD에서 raw `Needed a single revision` 오류 |
| `WSL-006` | medium | fixed | WSL Git이 `/mnt/c` Windows checkout 줄바꿈을 대량 변경으로 인식 |
| `WSL-007` | low | fixed | chat TUI reconnect 테스트의 고정 500ms 타이밍 플래이크 |
| `WSL-008` | high | fixed | provider 오류/실행 중단 후 장기 lease가 남아 `resume` 충돌 |
| `WSL-009` | high | fixed | config가 없는 기존 DB에서 `migrate apply`가 시작 전에 실패 |
| `WSL-010` | critical | fix-in-progress | repository-local DB 분산과 provider safe mode가 migration·plan 진입을 순환 차단 |
| `WSL-011` | high | open | migration 대기 DB에서 `doctor`가 미래 schema 컬럼을 먼저 조회해 raw SQL 오류 반환 |
| `WSL-012` | high | fixed | 최소 버전 이상 Codex가 exact-only 판정으로 safe mode에 고정됨 |
| `WSL-013` | high | fixed | startup/secondary workspace probe가 daemon restart 종료를 차단 |
| `WIN-001` | medium | fixed | Windows PATH가 npm nightly 대신 오래된 Cargo `0.1.0`을 선택 |
| `WIN-002` | medium | closed | Windows nightly PE의 Authenticode 부재를 enterprise 지원 제한으로 명시 |
| `WIN-003` | low | open | Windows 전체 테스트에서 `icacls.exe` 접근 거부 플래이크가 재발 |
| `WIN-004` | medium | fixed | Agy가 provider 관리 CLI의 허용 enum에서 누락됨 |

## WSL-010: repository-local 상태 분산과 safe-mode migration 순환

### 관찰

nightly `0.1.1-nightly.20260723.7a977cf`를 WSL 홈에서 실행했을 때 다음 순환이 발생했다.

```text
$ colay run --plan-only hello
error: state schema migration is required ([9, 10, 11]); run `colay migrate apply`

$ colay migrate apply
error: migration apply is disabled in safe mode; run `colay compatibility` and resolve:
Codex version is untested; writable work is disabled
```

또한 `colay run hello`는 홈 디렉터리에 별도 repository-local 상태를 전제로 하면서 plan
대화에 진입하기 전에 committed Git repository를 요구했다. 경로마다 `.colay` DB가 생성되므로
사용자가 작업 디렉터리를 바꿀 때마다 schema, daemon, lease, backup이 분리되고 migration이
로컬 config 존재 여부에 영향을 받는다.

### 영향

- provider가 새 버전이라는 이유로 상태 유지보수까지 safe mode에 묶여 사용자가 순환에서
  빠져나올 수 없다.
- non-Git 위치에서 plan 대화를 시작할 수 없고 raw Git 준비 상태가 제품 진입 조건이 된다.
- daemon과 CLI가 repository-local DB에 직접 접근해 `WSL-003`의 writer 경합이 구조적으로
  남는다.
- 동일 사용자의 task와 conversation을 다른 경로에서 일관되게 조회하거나 재개하기 어렵다.

### 승인된 개선 방향

- OS 사용자당 SQLite DB 하나를 두고 모든 workspace 상태를 `workspace_id`로 분리한다.
- Windows와 WSL은 SQLite 파일을 공유하지 않고 각 환경의 사용자 전역 위치를 사용한다.
- 정상 동작의 SQLite writer는 사용자당 daemon 하나로 제한하고 CLI/TUI는 IPC를 사용한다.
- Git이 없는 디렉터리에서도 interview·validation·plan 대화를 허용하고, 정확한 최종 승인
  뒤 writable task로 승격할 때만 committed Git repository를 요구한다.
- schema 생성·backup·forward migration은 provider compatibility 검사 전에 실행하고 safe
  mode가 이를 차단하지 못하게 한다.
- 현재 workspace의 기존 `.colay` 상태만 자동으로 멱등 import하며 원본은 삭제하지 않는다.
- 상세 결정은
  `docs/superpowers/specs/2026-07-24-user-global-workspace-state-design.md`에서 추적한다.

### 완료 조건

- WSL과 Windows의 non-Git 홈에서 `colay run hello`와 `run --plan-only hello`가 task/worktree
  없이 plan 대화를 시작한다.
- schema 8 전역 DB와 untested provider 조합에서 daemon이 backup 후 schema 11 이상으로
  migration하고 provider 진단 단계로 진행한다.
- 여러 workspace와 동시 CLI stress test에서 `SQLITE_BUSY`가 발생하지 않는다.
- 기존 repository-local 상태가 정확히 한 번 import되고 원본과 audit evidence가 보존된다.
- active lease의 `resume`은 충돌 오류 대신 기존 실행에 연결된다.
- fake provider만 사용한 Windows/WSL QA와 전체 Rust 품질 게이트가 통과한다.

### Task 8 Windows/WSL rollout 검증 (2026-07-26)

검증 환경은 Windows 11 Home `10.0.26100` x86-64와 WSL 2 Ubuntu 24.04
`6.18.33.2-microsoft-standard-WSL2` x86-64, Rust 1.95.0이다. WSL은 source만
`/mnt/c`에서 읽고 아래 Linux-native cache를 사용했다. 모든 CLI fixture는 OS-native
임시 `COLAY_HOME`, `COLAY_TEST_FAKE_PROVIDERS_ONLY=1`, compiled fake provider만
사용했다. 실제 Codex, Claude, Gemini, Agy inference는 호출하지 않았다.

```text
export RUSTUP_HOME=/home/kimohy/.cache/colay-task8-20260726/rustup
export CARGO_HOME=/home/kimohy/.cache/colay-task8-20260726/cargo
export CARGO_TARGET_DIR=/home/kimohy/.cache/colay-task8-20260726/target
```

#### 재현된 실패와 원인

- 초기 WSL cold fan-in의 readiness timeout, 후속 raw `ENOENT`/connection reset은 같은
  daemon endpoint 소실의 서로 다른 관찰이었다. test-only daemon stderr와 persisted
  `daemon_instances`를 함께 수집한 재현에서 online owner가 stop 요청 없이 released되고
  `optimistic update conflict for daemon instance ...`로 종료됨을 확인했다. 5초 lease보다
  WSL virtual ext4 I/O가 늦어지면 기존 SQL의 `lease_expires_at > heartbeat_at` 조건이
  아직 대체 owner가 없는 동일 owner의 늦은 heartbeat까지 거절한 것이 원인이었다.
  동일 owner는 unreleased row를 갱신할 수 있게 하되, takeover가 먼저 old row를 release하면
  old-owner heartbeat가 계속 거절되도록 수정했다.
- heartbeat 수정 후 16개 plan-only client 가운데 하나가 10초 IPC response deadline을
  넘기는 RED가 재현됐다. 각 client가 command status를 고정 25ms로 polling해 single IPC
  writer에 최대 640 request/s를 넣은 것이 원인이었다. 전체 command deadline 2분은 유지하고
  poll 간격만 25, 50, 100, 200, 400, 800, 1000ms로 지수 backoff/cap했다.
- 과거 WSL full legacy-import를 5분에 중단했을 때 main thread의 `futex_do_wait`만 보고
  deadlock으로 분류한 것은 잘못이었다. 다시 관찰한 worker들은 `jbd2_log_wait_commit`,
  `submit_bio_wait`, `folio_wait_bit_common`에서 진행 중이었고, serial 22/22가 660.12초
  (command 741.9초), default-parallel 22/22가 283.27초(command 331.8초)에 모두 끝났다.
  원인은 WSL virtual ext4 journal I/O 지연이며 import deadlock이나 제품 실패가 아니다.

#### 결정적 RED/GREEN 회귀

```text
cargo test -p colay --bin colay ipc_client::tests --all-features -- --nocapture
cargo test -p orchestrator-state --lib daemon_instances::tests::same_owner_renews_an_expired_lease_before_any_takeover -- --nocapture --exact
cargo test -p colay --bin colay app::tests::run_command_polling_backs_off_and_caps_at_one_second --all-features -- --nocapture --exact
```

첫 명령은 helper 추출 전 unresolved import RED 뒤 4/4 GREEN으로 startup spawn 1회,
Windows `ERROR_PIPE_BUSY` busy→busy→success, non-busy 즉시 실패, deadline 종료를 고정했다.
두 번째는 수정 전 `OptimisticConflict` RED, 수정 후 1/1 GREEN이었다. takeover test에도
old-owner heartbeat 거절 assertion을 추가했다. 세 번째는 helper 부재 compile RED 뒤
backoff sequence 1/1 GREEN이었다. 모두 exit code 0으로 재실행했다.

#### Windows-native 최종 명령

```text
cargo test -p colay --bin colay ipc_client::tests --all-features
cargo test -p orchestrator-state --lib daemon_instances::tests
cargo test -p orchestrator-state --test global_workspace_state -- --nocapture
cargo test -p colay --test global_daemon --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_plan_first --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_resume --features test-fixtures -- --nocapture --test-threads=1
cargo test -p colay --test global_doctor --features test-fixtures doctor_reports_global_workspace_and_operational_checks -- --nocapture --exact
cargo test -p orchestrator-state --test legacy_import -- --nocapture
cargo test -p colay --test global_concurrency --features test-fixtures -- --nocapture --test-threads=1
```

위 순차 행렬은 각 명령 exit code 0이었다. 전역 path 14/14, daemon/IPC 7/7,
non-Git plan 2/2, resume 3/3, doctor exact 1/1, full import 22/22, 32-client stress와
Unicode/case 2/2가 통과했다. Named pipe DACL은 현재 SID ACE 하나만 포함하고 broad
principal ACE를 포함하지 않았다.

#### WSL-native 최종 명령

```text
cargo test -p orchestrator-state --test global_workspace_state -- --nocapture
cargo test -p colay --test global_concurrency --test global_plan_first --features test-fixtures -- --nocapture --test-threads=1
for run in 1 2 3; do cargo test -p colay --test global_concurrency --features test-fixtures concurrent_clients_never_observe_sqlite_busy_or_duplicate_rows -- --nocapture --exact || exit 1; done
cargo test -p orchestrator-state --test legacy_import -- --nocapture
```

각 명령은 exit code 0이었다. path/XDG 18/18, clean combined concurrency 2/2
(105.42초)와 plan-first 2/2(23.76초), exact 32-client stress 3회(67.89, 54.58,
53.33초), final full import 22/22(293.87초)가 통과했다. Unix socket은 mode `0600`이며
`COLAY_HOME` 소유자 UID와 일치했다. Windows와 WSL DB root는 서로 다른 OS-native
임시 경로였고 WSL `/mnt/<drive>` state root 거부도 통과했다.

모든 성공 경로 concurrency test는 `daemon stop` 성공 뒤 최대 10초 동안 unreleased
lease 0건, IPC endpoint 부재, OS PID 종료를 확인한다. `Drop`은 실패 경로 fallback일
뿐 성공 증거가 아니다. 이 원인 분석과 clean full matrix로 `WSL-010`을 fixed로 유지한다.

최종 candidate에서 `cargo fmt --all -- --check`는 exit code 0(6.6초),
`cargo clippy --workspace --all-targets --all-features -- -D warnings`는 exit code
0(28.2초), `cargo test --workspace --all-features`는 exit code 0(676.4초)이었다.
마지막 전체 suite에서도 concurrency 2/2(45.49초), daemon 7/7(27.85초), doctor
13/13(15.26초), plan-first 2/2(24.23초), resume 3/3(14.48초)와 full import
22/22가 통과했다.

### 배포 nightly 재검증 (2026-07-26)

`main` merge commit `7a45d976d89274fd575babeabbedf9438c183452`에서 배포된
`0.1.1-nightly.20260726.7a45d97`을 WSL Ubuntu 24.04의 NVM Node 22.23.1에
정확한 버전으로 설치했다. npm root와 중첩 Linux native package 버전은 일치했고,
native binary는 x86-64 static PIE였으며 `colay --version`도 일치했다. 실제 provider
inference는 호출하지 않았다.

배포본에는 이 브랜치의 전역 상태 변경이 아직 포함되지 않았다. 격리 HOME 아래 두 Git
workspace에서 `colay init`을 실행하자 각각 `.colay/config.toml`,
`.colay/events.jsonl`, `.colay/orchestrator.db`가 생성됐고 사용자당 공용 DB는 생성되지
않았다. non-Git `run --plan-only hello`도 provider를 호출하지 않고 성공했지만 같은
경로에 별도 `.colay/orchestrator.db`를 만들었으며 출력 계약은 계속
`static assessment, not conversation mode`였다.

실사용 schema 8 DB에서 `migrate plan`은 9, 10, 11의 비파괴 순차 계획을 만들었지만
`migrate apply`는 Codex 0.145.0을 `untested`로 분류한 safe mode에 의해 다시 거부됐다.
호출 전후 DB SHA-256은 같아 사용자 상태는 변경되지 않았다. `compatibility`는 exec,
App Server, read-only/workspace-write sandbox와 reasoning effort를 공개 probe로 확인했지만
최종 상태는 `generic_untested`, writable unsupported였다.

반면 신규 schema 11 격리 workspace에서는 `init`, `doctor`, `migrate status`, `status`,
daemon start/status/restart/stop이 모두 성공했다. restart는 PID를 교체했고 CLI/daemon의
native 경로, nightly 버전, `linux/x86_64` identity가 일치했다. non-Git과 unborn HEAD의
writable `run`은 상태 파일을 만들기 전에 구체적인 Git 준비 오류로 차단됐다.

따라서 source candidate의 Windows/WSL 검증은 유지하되, 사용자에게 배포된 nightly에서
완료 조건이 확인될 때까지 `WSL-010` 상태를 `fix-in-progress`로 되돌린다. 다음 완료
조건은 이 브랜치를 `main`에 통합하고 새 nightly에서 사용자당 DB 1개, schema 8 자동
유지보수, capability-qualified Codex 0.145.0, conversation-first non-Git 진입을 재검증하는
것이다.

## WSL-011: migration 대기 DB의 `doctor` 선행 schema 조회

### 관찰

nightly `0.1.1-nightly.20260726.7a45d97`에서 schema 8 사용자 DB를 대상으로
`colay --json doctor`를 실행하면 migration-required 진단 대신 다음 SQL 오류로 종료한다.

```text
error: SQLite operation failed: no such column: phase in SELECT ...
FROM daemon_instances WHERE released_at IS NULL ...
```

같은 DB에서 `migrate plan`은 pending 9, 10, 11을 정상 보고하고 `daemon status`와
`status`는 `state schema migration is required`로 올바르게 차단된다. `phase`는 schema 9에서
추가되는 컬럼이므로 `doctor`만 migration guard보다 daemon runtime 조회를 먼저 수행한다.

### 영향 및 완료 조건

- 사용자가 migration 필요성을 진단하려는 명령에서 내부 SQL과 schema 세부가 노출된다.
- `doctor`가 migration을 적용하지 않더라도 현재/목표 schema와 pending step을 안전하게
  보고하고, 현재 schema에 없는 컬럼을 조회하지 않아야 한다.
- schema 8 fixture에서 `doctor`가 raw SQLite 오류 없이 구조화된 migration-required 또는
  제한된 진단을 반환하는 회귀 테스트가 필요하다.

## WSL-001: NVM/Node 및 PATH 불일치

### 관찰

- 패키지는 Node.js 22 이상을 요구한다.
- 최초 설치는 NVM의 Node.js `20.19.6` 아래에 존재했다.
- interactive Bash에서는 `colay`가 NVM 경로에서 확인됐지만, 일반적인
  `wsl.exe ...` 비대화형 실행에서는 NVM이 로드되지 않아 `colay`를 찾지 못했다.
- 이후 Node.js `22.23.1` 아래에도 동일 nightly가 설치됐다.
- 한동안 Node 20으로 시작한 daemon과 Node 22로 시작한 TUI가 동시에 같은 DB를 사용했다.

### 영향

- 실행 방식에 따라 서로 다른 Node/Colay 설치가 선택된다.
- 업그레이드 후에도 기존 daemon은 이전 NVM 경로의 binary로 계속 실행될 수 있다.

### 현재 우회

```bash
nvm install 22
nvm alias default 22
nvm use 22
npm install --global @kimohy/colay@nightly
```

업그레이드 후에는 기존 TUI를 종료하고 daemon을 명시적으로 stop/restart한다.

### 제품 개선 후보

- launcher가 지원되지 않는 Node 버전을 명확한 오류로 거부한다.
- `doctor`가 launcher, native binary, daemon 각각의 실제 경로와 버전을 함께 보고한다.
- daemon 상태에 시작 executable 경로와 Colay build version을 포함한다.
- WSL 비대화형 실행과 NVM 사용법을 설치 문서에 명시한다.

### 수정 구현 및 재검증

- npm launcher가 native binary를 resolve/spawn하기 전에 실제 Node major version을 검사한다.
  Node 22 미만이면 `nvm install 22 && nvm use 22`를 포함한 명확한 오류로 종료한다.
- `doctor`에 `runtime` check를 추가해 실제 native executable 경로, Colay build version,
  target OS/architecture, invocation path를 보고한다. 이 check는 state를 만들지 않는다.
- WSL의 system Node `18.19.1`로 새 launcher를 실행하면 native를 시작하지 않고 지원 버전
  오류를 반환하며, NVM Node `22.23.1`에서는 launcher 8개 테스트가 모두 통과했다.
- 비대화형 shell이 NVM을 source하지 않아 명령 자체를 찾지 못하는 경우는 shell 환경
  설정이므로 PATH에 Node 22 NVM bin을 넣거나 실행 전에 `nvm use 22`를 수행해야 한다.
- 설치된 nightly root/native package는 모두
  `0.1.1-nightly.20260721.8c7f638`로 일치했고 Linux native는 static PIE x86-64였다.

## WSL-002: daemon start timeout과 orphan child

### 재현된 증상

```text
error: daemon did not publish a healthy heartbeat within five seconds
```

- 격리된 임시 repository에서 `daemon start`가 두 번 timeout 됐다.
- 같은 상태에서 `daemon restart`는 online이 됐다.
- timeout을 반환한 이전 child는 종료되지 않았고, 이후 별도로 daemon lease를 획득했다.
- 이미 stop이 성공한 뒤에도 느리게 초기화되던 이전 child가 lease를 획득해 daemon이 다시
  online이 되는 race가 관찰됐다.

### 근본 원인 방향

- `ensure_started`의 고정 timeout은 5초다.
- child는 provider probe를 마치기 전까지 heartbeat/lease를 게시하지 않는다.
- timeout 경로가 spawn한 child를 종료하거나 회수하지 않는다.
- child stdout/stderr가 null로 폐기돼 시작 실패 원인도 남지 않는다.

### 제품 개선 후보

- 최소 bootstrap lease/heartbeat를 provider probe보다 먼저 게시한다.
- startup phase를 `booting`, `probing`, `online`, `failed`로 구분한다.
- timeout 시 정확히 자신이 spawn한 child를 종료하고 종료 확인까지 수행한다.
- stderr 또는 redacted startup diagnostics를 repository state에 보존한다.
- 느린 fake provider probe를 사용하는 회귀 테스트를 추가한다.

### 수정 구현

- schema migration 9에서 daemon instance에 `booting`, `probing`, `online`, `failed` phase와
  redacted `startup_error`를 추가했다. schema 8의 기존 행은 `online`으로 보존된다.
- child는 provider probe 전에 bootstrap lease를 획득하고 별도 startup heartbeat로 lease를
  갱신한다. 서비스 구성이 끝난 뒤에만 `online`으로 전환하며 정상 daemon loop는 같은
  instance 소유권을 재획득하지 않고 이어받는다.
- 부모는 `booting`과 `probing`을 진행 중 상태로 처리하고, 활성 provider 수에 따른 bounded
  probe 예산을 사용한다. 6초 지연 fake Codex probe가 과거 5초 거짓 timeout 없이 Windows에서
  online이 되는 회귀 테스트를 추가했다.
- timeout과 조기 종료 경로는 부모가 보유한 정확한 child PID의 프로세스 트리를 종료하고
  child 종료를 확인한다. 같은 PID의 lease만 `failed`로 기록·해제하며 다른 owner는 건드리지
  않는 테스트를 추가했다.
- child setup 오류는 configured redactor를 거친 뒤 repository DB에 보존된다. detached child의
  장기 stderr pipe는 Windows parent 종료를 막을 수 있어 사용하지 않고 raw provider stderr도
  영속화하지 않는다.
- 수정 커밋 `f88d974`, `26a001d`, `de70216`, `96d8460`에서 Windows lifecycle 3개,
  전체 Rust 418개, npm 65개, fmt와 전체 Clippy `-D warnings`가 통과했다. 실제 provider
  inference는 호출하지 않았고 `WIN-003`의 `icacls.exe` 접근 거부도 재발하지 않았다.

### 완료 조건

- 느린 probe에서도 `start`가 거짓 실패를 반환하지 않는다.
- timeout을 강제해도 child와 lease가 나중에 다시 나타나지 않는다.
- 반복 start/restart/stop 후 관련 프로세스가 남지 않는다.

## WSL-003: SQLite writer starvation (WSL/Windows 공통)

### 재현된 증상

```text
error: SQLite operation failed: database is locked: database is locked: Error code 5
```

### 증거

- daemon과 TUI가 같은 WAL/SHM을 열고 있는 상태에서 발생했다.
- DB `PRAGMA integrity_check`는 `ok`였다. DB 손상은 아니었다.
- 실패한 `colay run hello`는 task 생성, 분석, routing, event reconciliation을 완료하고
  `planned` 상태까지 기록한 뒤 coordinator lease 획득 전에 실패했다.
- 해당 task에는 coordinator lease와 provider attempt가 없었다.
- 활성 daemon 상태에서 무변경 `BEGIN IMMEDIATE` 획득을 1,000회 시도했을 때
  671회가 즉시 `SQLITE_BUSY`였다. 다른 시점의 200회 표본에서는 2회로, 경쟁률은
  daemon 활동에 따라 크게 변했다.
- daemon 기본 command poll은 100ms다.
- idle poll도 session command claim, orchestration command claim, ready-task claim에서
  pending row 존재 여부를 확인하기 전에 `TransactionBehavior::Immediate`를 시작한다.
- Windows 11의 격리 repository에서도 fake-provider daemon이 online인 동안 SQLite
  `timeout=0`으로 `BEGIN IMMEDIATE`/rollback을 500회, 10ms 간격으로 시도했을 때
  81회(16.2%)가 `SQLITE_BUSY`였다. daemon stop 후 동일 표본은 500회 모두 성공했다.
- Windows 표본 전후의 `PRAGMA integrity_check`도 `ok`였으므로 플랫폼별 DB 손상이 아니라
  활성 writer 경쟁으로 보는 것이 타당하다.

### 현재 우회

- direct `colay run`을 사용할 때 TUI를 먼저 종료하고 repository daemon을 stop한다.
- 실패 후 새 task를 무작정 만들지 말고, 이미 `planned`로 남은 task와 lease/attempt 유무를
  먼저 확인한다.
- `orchestrator.db-wal` 또는 `orchestrator.db-shm`을 직접 삭제하지 않는다.

### 제품 개선 후보

- read-only precheck로 pending 후보가 있을 때만 immediate transaction에 진입한다.
- transaction 안에서 후보를 재검증해 TOCTOU 안전성은 유지한다.
- `SQLITE_BUSY`에 bounded retry, jitter, deadline, 구체적인 owner diagnostics를 추가한다.
- direct run이 daemon과 별도 writer로 경쟁하지 않고 durable command를 통해 daemon에
  제출되는 단일-writer 구조를 검토한다.
- task 영속화와 coordinator 확보 사이 실패를 명시적인 recoverable 상태로 기록한다.

### 수정 구현

- `codex/fix-sqlite-writer-starvation`에서 command queue와 scheduler의 idle claim 경로에
  read-only `SELECT EXISTS` 사전검사를 추가했다.
- 후보가 없으면 `BEGIN IMMEDIATE` 없이 `None`을 반환한다. 후보가 보이면 기존 immediate
  transaction에 진입해 같은 조건을 다시 조회하므로 concurrent claim의 단일 승자와
  TOCTOU 안전성은 유지된다.
- 별도 SQLite 연결이 `BEGIN IMMEDIATE`를 보유한 상태에서도 빈 general/session/
  orchestration command queue와 빈 scheduler poll이 `SQLITE_BUSY` 대신 `None`을 반환하는
  Windows 회귀 테스트 2개를 추가했다.
- 수정 커밋 `bf49188`, 전체 Rust 테스트 409개, npm 테스트 65개, fmt와 전체 Clippy
  `-D warnings` 통과로 수정 범위를 검증했다. 실제 provider inference는 호출하지 않았다.

### 완료 조건

- idle daemon/TUI와 direct run을 병행하는 stress test에서 `SQLITE_BUSY`가 발생하지 않는다.
- 실패 주입 후 task가 중복 생성되지 않고 동일 task를 안전하게 재개할 수 있다.
- DB integrity, append-only event chain, exact lease ownership이 유지된다.

## WSL-004: Git 저장소가 아닌 위치의 late failure (WSL/Windows 공통)

### 재현된 증상

```text
fatal: not a git repository (or any of the parent directories): .git
```

### 증거

- Colay/TUI가 `/home/kimohy`에서 시작됐다.
- `/home/kimohy`는 Git repository가 아니지만 `/home/kimohy/.colay` state가 생성됐다.
- `colay run`은 task를 `planned`까지 영속화한 뒤 worktree 생성 시 raw Git 128로 실패했다.
- Windows 11의 격리된 non-Git 디렉터리에서도 native nightly와 fake provider로 같은 raw
  오류를 재현했다. 실패 후 DB integrity는 정상이었지만 `planned` task 1건이 남았다.

### 제품 개선 후보

- writable `run`, `resume`, TUI approval/execution 전에 Git repository와 worktree 지원 여부를
  preflight한다.
- preflight는 DB/task/event/worktree mutation보다 먼저 실행한다.
- 사용자 오류는 `colay run must be executed inside a Git repository`처럼 제품 문맥으로
  변환하고 실행한 Git argv와 안전한 cwd를 진단 데이터로 남긴다.

### 수정 구현

- `codex/fix-git-readiness-preflight`에서 read-only Git readiness 검사를 추가했다.
- direct `colay run`은 `.colay` state와 task를 만들기 전에 repository root와
  `HEAD^{commit}`을 검사한다.
- non-Git 상태는 `direct task execution requires a Git repository`로 분류하며 raw Git 128을
  사용자 오류로 노출하지 않는다.
- Windows 호환 CLI 회귀 테스트가 실패 후 `.colay`가 존재하지 않음을 검증한다.
- 수정 커밋 `787ffdf`, `64583f7`과 Windows 전체 Rust 테스트 407개 통과로 완료 조건을
  확인했다.

### 사용자 정정: conversation-first plan mode

- 사용자가 의미하는 plan-only는 현재 CLI의 `run --plan-only`와 다르다. orchestrator가
  read-only provider를 선택해 사용자의 질의를 이해하고 답변하며, 후속 질문으로 요구를
  구체화하는 **task 이전의 대화 단계**다.
- 이 단계에서는 task row, coordinator/worker lease, provider attempt, worktree를 만들지
  않는다. 처음부터 모든 입력을 coding task로 영속화하는 현재 `run` 중심 흐름은
  바람직하지 않다.
- orchestrator가 대화 결과를 `answer_complete`, `more_information_needed`,
  `worktree_task_candidate` 같은 명시적인 outcome으로 판단한다.
- 단순 질의는 답변으로 끝나고 session 대화 기록만 남는다. 구현 또는 repository 변경이
  필요하다고 판단한 경우에만 실행 가능한 task graph를 제안하고, 전환 게이트를 통과한
  뒤 task와 격리 worktree를 생성한다.
- 사용자는 전환 게이트를 **인터뷰를 통한 요구 구체화 → 실행 가능성 및 결과 검증 →
  최종 승인** 순서로 확정했다. orchestrator의 필요성 판단만으로 task를 자동 시작하지
  않으며, 최종 승인 전에는 task materialization과 writable 실행이 모두 금지된다.
- 인터뷰 중 요구나 검증 결과가 바뀌면 계획을 새 revision으로 갱신하고 이전 승인 후보를
  무효화해야 한다. 최종 승인은 검증된 최신 revision에만 결합된다.
- Git repository/HEAD 검사는 대화 시작 조건이 아니라 task 승격 조건이다. Git이 없거나
  unborn HEAD라면 대화와 답변은 계속 가능해야 하며, task를 만들지 않은 채 준비 방법을
  안내하고 plan mode에 머문다.
- 이미 materialize된 task의 `resume`은 이 초기 대화 흐름과 별개이며, lease 및 Git
  안전성 검사를 그대로 적용한다.

### 구현 결과

- session-level 사용자 메시지는 task가 아니라 durable
  `request_conversation_turn`으로 이어진다. official CLI adapter는 read-only sandbox에서
  `answer_complete`, `more_information_needed`, `worktree_task_candidate`,
  `needs_attention` 중 하나의 엄격한 JSON outcome만 반환한다.
- 답변과 인터뷰는 conversation attempt와 timeline, 필요 시 immutable requirement revision만
  기록한다. task, task attempt, worktree, coordinator/worker lease는 만들지 않는다.
- 완전한 candidate만 read-only graph planning을 자동 queue한다. Git repository와 valid
  `HEAD`는 이 승격 단계에서 검사하며, 실패하면 session을 유지하고 `Initialize Git and
  create HEAD` 안내를 남긴 채 approvable hash를 만들지 않는다.
- 승인 카드는 requirement revision, validation hash, base commit, validation checks와
  proposal hash를 표시한다. 최신 사용자 메시지 또는 Git `HEAD`가 바뀌면 승인을
  숨기거나 거부한다. 정확한 typed 승인 이후에만 task를 atomic materialize한다.
- provider 실패는 redacted `needs_attention` 응답으로 종료하고 session과 사용자 메시지를
  보존한다. crash replay는 deterministic ID와 idempotency key로 중복 응답·계획을 막는다.
- 기존 `run --plan-only`는 provider를 호출하지 않는 static persisted assessment라는
  compatibility 의미를 유지해 conversation-first 흐름과 구분했다.

### 정정된 완료 조건 검증

- `fixed`: 단순 질의와 인터뷰에서 task/worktree/worker/coordinator lease가 모두 0건임을
  Windows fake-provider 통합 테스트로 검증했다.
- `fixed`: 구현 요청도 session 대화로 시작하고 완전한 candidate와 검증 전에는 task가
  없다.
- `fixed`: Git repository와 valid HEAD를 task graph 승인 후보 승격 직전에 검사하고,
  정확한 최종 승인 이후에만 task를 materialize한다. worktree는 scheduler가 이후 별도로
  생성한다.
- `fixed`: non-Git 및 unborn HEAD에서 session을 유지하고 task 없이 준비 안내를 반환한다.
- `fixed`: `run --plan-only`를 static compatibility command로 문서화하고 자동
  conversation-first TUI session과 분리했다.
- 승인된 접근법의 정식 설계는
  `docs/superpowers/specs/2026-07-22-conversation-first-plan-mode-design.md`에서 추적한다.

## WSL-005: unborn HEAD의 late failure (WSL/Windows 공통)

### 재현된 증상

```text
fatal: Needed a single revision
```

### 증거

- `/home/kimohy/workspace`는 `git init`은 됐지만 `No commits yet on main` 상태였다.
- 상위 workspace에는 `camfit`, `camping`, `quiz` 등 별도 Git repository가 중첩돼 있었다.
- 상위 workspace의 `.colay` DB에는 실패 후 `planned` task 1건이 남았다.
- `git rev-parse --verify HEAD`는 동일한 `Needed a single revision` 오류를 재현했다.
- 하위 `camping` repository는 clean 상태이고 유효한 HEAD가 있었다.
- Windows 11의 `git init`만 수행한 격리 repository에서도 native nightly와 fake provider로
  동일한 `fatal: Needed a single revision`을 재현했다. DB integrity는 정상이었지만
  `planned` task 1건이 남았다.

### 추가 재현: `~/workspace/test`

- 2026-07-22에 사용자가 `~/workspace/test`에서 `colay run hello`를 다시 실행했다.
- 해당 경로는 Git repository이지만 `## No commits yet on main`인 unborn `HEAD` 상태였다.
- state 이외의 project 파일은 없고 `.colay/`만 untracked로 존재했다.
- `git rev-parse --show-toplevel`은 `/home/kimohy/workspace/test`를 반환했지만,
  `git rev-parse --verify HEAD`는 `fatal: Needed a single revision`을 재현했다.
- DB integrity는 정상이고 `planned` task 1건이 남았다.
- active coordinator lease와 provider attempt는 모두 0건이므로 provider 실행 전 worktree
  base revision 확인 단계에서 실패한 것으로 확인됐다.

### 안전 주의

상위 workspace에서 문제를 해결하려고 `git add . && git commit`하면 `.env`,
`node_modules`, 중첩 repository 등을 잘못 포함할 수 있다. 실제 프로젝트 repository로
이동해야 한다.

### 제품 개선 후보

- writable 실행 전에 `git rev-parse --show-toplevel`과 `git rev-parse --verify HEAD^{commit}`을
  분리해서 검사한다.
- unborn HEAD를 `repository has no base commit; create an initial commit first`로 설명한다.
- 중첩 repository를 포함하는 상위 폴더에서 실행할 때 repository 선택 경고를 제공한다.
- 이 검사 역시 task 영속화보다 먼저 수행한다.

### 수정 구현

- Git root probe가 성공한 뒤 `git rev-parse --verify HEAD^{commit}`을 별도 실행한다.
- unborn `HEAD`는 `Git repository has no base commit; create an initial commit before task
  execution`으로 분류한다.
- Windows 호환 CLI 회귀 테스트가 실패 후 `.colay`와 `planned` task가 생성되지 않음을
  검증한다.
- 수정 커밋 `787ffdf`, `64583f7`과 Windows 전체 Rust 테스트 407개 통과로 완료 조건을
  확인했다.

## WSL-008: provider 오류 후 남은 장기 lease

### 재현된 증상

```text
error: lease conflict for task 019f86e9-e70b-7340-a119-20d230d0f8ff: another coordinator lease is active
```

### 증거

- 2026-07-22 08:09 KST의 이전 `resume`은 유효한 초기 commit
  `daf06377f772d3a68aa28b331508d6ee892e2b77`을 기준으로 격리 worktree를 만들었다.
- task는 `planned`에서 `running`으로 전환됐고 Claude writable worker attempt 및 lease가
  생성됐다.
- worker event에는 `Credit balance is too low` 메시지와 `claude_result` 오류가 기록됐지만,
  attempt의 `ended_at`과 `outcome`은 계속 `NULL`이고 task도 `running` 상태로 남았다.
- 조사 시점에는 Colay, Claude, Codex, Gemini 관련 실행 프로세스가 없었으며 worktree에는
  변경 사항도 없었다.
- target coordinator lease는 2026-07-22 08:09:25 KST에 획득됐고
  12:49:25 KST까지, worker lease는 08:40:34 KST까지 유효하게 저장됐다.
- 기본 설정에서는 coordinator TTL이
  `(default_timeout_minutes * (max_retries + 8)) + 600초`로 계산된다. 기본값 30분과
  retry 1회를 적용하면 한 번 획득한 lease가 4시간 40분 유지된다.
- 08:11 KST에 생성된 다른 `hello` task
  `019f86f2-ea08-7bb0-b7b6-0f24822b5eac`도 동일한 Claude credit 오류 후 `running`,
  open attempt, unreleased coordinator/worker lease 상태로 남아 동일 패턴이 반복됐다.
- DB `PRAGMA integrity_check`는 `ok`였으므로 database 손상이 원인은 아니다.

### 근본 원인 방향

- `resume`의 lease 획득 로직은 `released_at IS NULL`인 coordinator를 충돌로 처리하고,
  현재 시각이 저장된 `expires_at`에 도달했을 때만 stale row를 원자적으로 만료시킨다.
- coordinator lease에는 짧은 heartbeat나 owner process liveness 정보가 없고, 정상적인
  함수 반환 경로에서만 release된다. provider 오류 뒤 CLI가 중단되거나 비정상 종료되면
  긴 TTL 전체가 복구 지연 시간이 된다.
- provider가 fatal credit 오류를 event로 보낸 뒤 attempt/task/lease finalization까지
  도달하지 못한 흐름도 함께 조사해야 한다. 현재 증거만으로 CLI가 스스로 비정상
  종료됐는지 사용자가 대기 중 중단했는지는 구분할 수 없지만, 어느 경우든 다음
  `resume`이 수 시간 막히는 복구 설계 문제가 확인됐다.

### 현재의 안전한 복구

- 실행 중인 owner/provider 프로세스가 정말 없는지 먼저 확인한다.
- DB, `orchestrator.db-wal`, `orchestrator.db-shm`, worktree를 삭제하거나 lease row를
  수동 SQL로 변경하지 않는다. append-only audit와 소유권 경계를 훼손할 수 있다.
- 현재 공개된 안전한 takeover 경로는 coordinator 만료 후 같은 task를 다시
  `resume`하는 것이다. target task의 확인된 만료 시각은
  **2026-07-22 12:49:25 KST**다.
- 재개 전에는 Claude의 `Credit balance is too low` 원인을 해소하거나 다른 eligible
  provider로 routing되도록 정상적인 설정/상태를 준비해야 같은 실패가 반복되지 않는다.
- 별도 running task `019f86f2-ea08-7bb0-b7b6-0f24822b5eac`의 coordinator 만료 시각은
  **2026-07-22 12:51:44 KST**이므로 두 task를 혼동하지 않는다.

### 제품 개선 후보

- coordinator를 짧은 TTL과 주기적 renewal로 바꾸고, heartbeat가 끊긴 owner를 짧은
  grace period 뒤 안전하게 takeover할 수 있게 한다.
- Ctrl-C, signal, panic, provider fatal 오류 경로에서 attempt 결과, task 상태,
  worker lease, coordinator lease를 순서대로 finalization하는 공통 cleanup guard를 둔다.
- provider의 terminal 오류 event가 도착하면 stream 종료를 무기한 기다리지 않고 bounded
  wait/cancel 후 attempt를 실패 또는 blocked로 확정한다.
- `resume`의 lease 충돌 메시지에 owner 상태, 획득/갱신/만료 시각, child worker,
  안전한 다음 조치를 표시한다.
- credit/quota 계열의 terminal 오류를 provider health/routing evidence에 반영해 다음
  attempt가 같은 provider로 즉시 반복되지 않게 한다. 확인되지 않은 quota 수치는 계속
  unknown으로 유지한다.
- 명시적인 감사 이벤트와 승인 조건을 가진 `recover stale-lease` 관리 경로를 검토한다.
  owner liveness가 불명확하면 fail-closed하고 worktree와 attempt evidence를 보존한다.

### 수정 구현

- provider `WorkerEvent::Error`는 redacted audit event를 먼저 기록한 뒤 즉시 cancel을
  요청하고 기존 process-tree termination 확인 경로로 진입한다. 확인된 종료는 attempt의
  `ended_at`과 `outcome`을 확정하며, 확인되지 않은 종료만 기존대로 lease를 보존한다.
- direct coordinator lease는 30초, child worker lease는 20초 TTL로 줄이고 활성 owner가
  5초마다 갱신한다. owner가 사라지면 기존 원자적 expiry/takeover가 최대 30초 경계에서
  authority를 회수하며, 살아 있는 owner는 계속 갱신되어 takeover되지 않는다.
- 충돌 오류에 coordinator owner, `renewed_at`, `expires_at`, active worker 수, 안전한 재시도
  시각을 포함한다.
- fake Claude terminal credit 오류, active coordinator/worker renewal, bounded TTL, 충돌
  diagnostics, 기존 atomic expiry/takeover 회귀가 Windows에서 통과했다.
- 수정 커밋 `5f09ecd`와 전체 Rust 테스트 411개, npm 테스트 65개, fmt 및 전체 Clippy
  `-D warnings` 통과로 검증했다. 실제 provider inference는 호출하지 않았다.

### 완료 조건

- fake provider가 terminal credit 오류를 반환하는 회귀 테스트에서 bounded 시간 내
  attempt가 종료되고 task 상태와 두 lease가 일관되게 finalization된다.
- worker 실행 중 parent CLI를 강제 종료하는 테스트에서 짧은 grace period 후 안전하게
  takeover할 수 있으며, 동시에 살아 있는 owner의 lease는 빼앗지 않는다.
- 충돌 출력만으로 사용자가 예상 만료 시각과 안전한 복구 방법을 확인할 수 있다.
- DB integrity, append-only event chain, worktree 격리, exact lease ownership이 유지된다.

## WSL-006: `/mnt/c` checkout과 줄바꿈 불일치

### 증거

- Windows Git에서는 대상 repository가 clean이었다.
- Windows Git의 system `core.autocrlf`는 `true`였다.
- WSL Git에서는 `core.autocrlf`가 unset이었다.
- 동일한 `/mnt/c` checkout을 WSL Git으로 조회하면 거의 모든 파일이 수정된 것으로
  표시됐다.

### 현재 우회

- Linux Colay는 WSL ext4 내부의 clone에서 사용한다.
- Windows checkout은 Windows Colay/Windows Git과 함께 사용한다.
- 한 checkout을 Windows Git과 WSL Git이 번갈아 관리하지 않는다.

### 제품 개선 후보

- WSL에서 repository가 `/mnt/*` 아래에 있으면 성능과 줄바꿈 위험을 진단한다.
- Git status가 전체 파일의 줄바꿈 변경으로 보이는 패턴을 감지해 writable 실행을
  fail-closed하거나 명시적 승인을 요구한다.

### 수정 구현 및 재검증

- `doctor`가 Linux에서 `/mnt/<drive>/...` checkout을 감지하면 Windows/WSL Git 혼용과
  줄바꿈·권한 위험을 경고한다. 경로 판별은 platform-neutral 단위 테스트로 고정했다.
- 운영 문서는 Linux Colay에는 WSL ext4 내부 native clone을, Windows checkout에는
  Windows Colay/Windows Git을 사용하고 writable 승인 전 `git status --short`를 검토하도록
  명시한다. 전체 줄바꿈 변경의 자동 수정이나 사용자 Git 설정 변경은 수행하지 않는다.

## WIN-001: Windows PATH가 오래된 Cargo 설치본을 선택

### 재현된 증상

- Windows PowerShell에서 `Get-Command colay`와 `where.exe colay`는
  `C:\Users\kimoh\.cargo\bin\colay.exe`를 선택했다.
- 이 binary는 `colay 0.1.0`이며, 검증 대상 nightly
  `0.1.1-nightly.20260721.8c7f638`과 다르다.
- Windows 전역 npm에는 `@kimohy/colay`가 설치돼 있지 않았다. 반면 격리된
  `npm exec --package=@kimohy/colay@nightly`와 임시 npm 설치는 같은 nightly를 정상
  선택했다.

### 영향

- 사용자가 nightly를 검증한다고 생각해도 Windows native 명령은 과거 Cargo build를
  실행할 수 있다.
- WSL과 Windows에서 같은 `colay` 명령이 서로 다른 기능, schema 기대치, 오류 처리를
  제공해 재현 결과가 혼재할 수 있다.

### 현재 우회

- 실행 전에 `Get-Command colay -All`, `where.exe colay`, `colay --version`을 함께 확인한다.
- 검증 시에는 버전이 고정된 `npm exec --yes --package=@kimohy/colay@nightly -- colay ...`
  또는 확인된 npm shim/native 경로를 사용한다.
- 오래된 Cargo 설치 제거 또는 PATH 순서 변경은 사용자 환경을 바꾸므로 자동 수행하지
  않는다.

### 제품 개선 후보

- `doctor`가 launcher 경로/버전, native 경로/버전, daemon 경로/버전을 한 화면에 표시한다.
- npm launcher가 자신의 package version과 native binary version 불일치를 거부한다.
- Windows 설치 문서에 Cargo/npm 명령 충돌 확인 절차를 포함한다.

### 수정 구현 및 재검증

- 새 `doctor.runtime` check가 현재 실행 중인 native binary 경로와 build version을 함께
  반환하므로, PATH가 Cargo `0.1.0` 또는 npm nightly 중 무엇을 골랐는지 JSON에서 바로
  확인할 수 있다.
- Windows source build에서 `runtime.status=pass`, `version=0.1.0`, 실제 격리 worktree의
  `target/debug/colay.exe`, `windows/x86_64`가 보고됐고 `.colay` state는 생성되지 않았다.
- PATH 우선순위 변경이나 오래된 Cargo binary 제거는 사용자 환경 변경이므로 자동화하지
  않으며, `Get-Command colay -All`과 `where.exe colay` 확인 절차를 유지한다.
- schema v11은 active daemon의 실제 executable 경로, build version, target을 영속화하며
  `doctor.daemon_runtime`이 현재 CLI와 비교한다. 불일치 시 intended binary로 daemon을
  stop/restart하라는 경고를 반환하므로 PATH 충돌과 stale daemon을 한 진단에서 구분한다.

## WIN-002: Windows nightly PE의 Authenticode 부재

### 증거

- npm의 `@kimohy/colay-win32-x64` native binary는 정상 AMD64 PE(`0x8664`)였고
  `--version`도 nightly와 일치했다.
- SHA-256은
  `1BAD6DDC441320165AFBD0B8E214BEF11B02CA806B6A7F97E882C5C8F23EB5BC`였다.
- `npm audit signatures --include-attestations`는 registry 서명과 GitHub Actions 기반
  SLSA provenance를 정상 검증했다.
- 그러나 Windows `Get-AuthenticodeSignature` 결과는 `NotSigned`였다. npm 공급망
  provenance가 유효하다는 사실과 Windows OS 수준의 code-signing 신뢰는 별개다.

### 영향

- 이번 QA 환경에서는 실행 차단이 발생하지 않았으므로 현재 오류의 직접 원인은 아니다.
- SmartScreen 평판, AppLocker/WDAC 또는 기업용 allowlisting 정책에서는 서명되지 않은
  nightly 실행 파일이 경고 또는 차단될 수 있다.

### 제품 개선 후보

- Windows release binary에 신뢰 가능한 Authenticode 서명을 추가하고 서명 검증 절차를
  배포 문서에 포함한다.
- 서명 도입 전에는 npm integrity/provenance와 공개 checksum 검증 방법을 명시한다.
- code signing이 초기 release 범위 밖이라는 기존 결정을 유지한다면 Windows enterprise
  지원 제한으로 명확히 문서화한다.

### 종료 근거

- 현재 release/security 문서는 Windows PE가 Authenticode `NotSigned`임을 명시하고,
  GitHub attestation·npm provenance·SHA-256이 OS publisher trust를 대체하지 않는다고
  구분한다.
- WDAC/AppLocker/SmartScreen publisher trust가 필수인 환경은 검증된 digest allowlist 또는
  조직 내부 build/sign을 사용해야 하며, upstream Authenticode가 필수인 배포는 현재 지원
  범위 밖이다. 서명 인증서 권한 없이 허위로 signed artifact를 주장하지 않고 명시적 지원
  제한을 제공했으므로 queue의 허용된 종료 조건에 따라 `closed` 처리한다.

## WSL-007: chat TUI reconnect 500ms 플래이크

### 증거

- 첫 `cargo test --workspace --all-features`에서
  `chat_tui_help_and_durable_reconnect_keep_daemon_alive`가
  `daemon did not create session within 500ms`로 실패했다.
- 해당 테스트만 3회 재실행했을 때 모두 통과했다.
- 전체 suite 재실행에서는 Rust 테스트 387개가 모두 통과했다.
- Windows 11 전체 suite에서는 같은 테스트를 포함한 Rust 테스트 402개가 한 번에 모두
  통과했다. 따라서 Windows에서 추가 재현되지는 않았으며 기존 WSL 타이밍 플래이크
  분류를 유지한다.

### 제품 개선 후보

- 고정 500ms 제한을 상태 기반 wait와 환경에 맞는 상한으로 교체한다.
- timeout 시 daemon heartbeat, command state, claimed/completed timestamps를 출력한다.
- CI 부하 상태에서 반복 실행하는 플래이크 검증을 추가한다.

### 수정 구현

- session/message projection의 고정 500ms 제한을 10ms 간격, 최대 5초의 상태 기반 bounded
  wait로 교체했다. 성공 시 즉시 반환하므로 정상 경로를 불필요하게 지연하지 않는다.
- 수정 커밋 `fe23303` 이후 해당 Windows 테스트를 단독 3회 연속 통과시켰고 전체
  workspace suite도 통과했다.

## WIN-003: Windows `icacls.exe` 접근 거부 테스트 플래이크

### 증거

- Git readiness 수정 후 첫 전체 suite에서 기존
  `rollback_relative_codex_target_matches_persisted_writable_worker_worktree` 테스트가
  `C:\Windows\System32\icacls.exe: Access is denied (os error 5)`로 한 번 실패했다.
- 동일 테스트만 연속 3회 실행했을 때 모두 통과했다.

### 재개 근거

- conversation-first 최종 전체 target/feature 실행까지 재발하지 않아 한때 `closed`였으나,
  `f693062` 기준 새 worktree의 수정 전 전체 suite에서
  `provider_capacity_and_overlapping_resource_claims_block_admission`이 동일한
  `C:\Windows\System32\icacls.exe: Access is denied (os error 5)`로 다시 실패했다.
- 실패한 테스트를 즉시 단독 3회 재실행하면 모두 통과했다. 현재 Git 변경과의 인과관계는
  확인되지 않았지만 두 번째 관찰이므로 `open`으로 재개한다.

### 다음 조사 조건

- 동일 오류가 다시 발생하면 executable resolution evidence, 현재 identity, ACL, antivirus
  또는 endpoint policy, 동시 실행 중인 `icacls` 프로세스를 실패 시점에 수집한다.
- 원인이 확인되기 전에는 무조건적인 retry나 권한 완화를 추가하지 않는다.

## WIN-004: Agy provider 관리 CLI 누락

### 재현 및 영향

- Windows source QA에서 `colay providers disable agy`가 가능한 값으로 Gemini, Codex,
  Claude만 출력하며 Agy를 거부했다. Agy는 scheduler/config/profile에서 독립 provider로
  지원되므로 관리 CLI만 비대칭인 결함이었다.
- 운영자는 config TOML을 직접 수정하지 않고 Agy를 enable/disable하거나 profile target으로
  선택할 수 없었다.

### 수정 및 검증

- `ProviderName`과 문자열 parser에 Agy를 추가하고 `ProviderId::Agy`로 변환하도록 했다.
- `providers disable agy`와 `profiles reset agy standard` clap 회귀 테스트가 통과했다.
- Windows 실제 source binary에서 `provider_updated { provider: agy, enabled: false }`가
  반환되고 effective provider report에도 `enabled=false`가 반영됨을 확인했다.
- 변경 후 workspace 전체 target/feature Clippy `-D warnings`와 전체 Rust suite가 통과했다.

## WSL-009: config 없는 기존 DB의 migration 진입 실패 (WSL/Windows 공통)

### 재현 및 영향

```text
$ colay migrate apply
error: I/O operation failed for <repository>/.colay/config.toml: No such file or directory (os error 2)
```

- `0.1.1-nightly.20260722.f693062`에서 schema 8 DB에 schema 9, 10, 11 migration이
  필요하다는 안내 직후 재현됐다.
- repository `.colay/config.toml`이 없는 것은 유효한 상태다. 자동 설정 layer는 선택
  사항이며 기본 설정만으로도 runtime과 repository-local state를 사용할 수 있다.
- migration이 막혀 daemon을 새 nightly로 재시작할 수 없다.

### 근본 원인

- 일반 runtime loader는 자동 탐색 config가 없어도 유효 기본 설정을 만든다.
- 그러나 `migrate_inner`는 runtime load 성공 후에도 기본 편집 경로인
  `.colay/config.toml`을 `MigratableConfigDocument::load`로 무조건 읽었다.
- 따라서 DB migration과 무관한 선택적 config 파일 부재가 migration 진입을 차단했다.
  명시적 `--config` 또는 `COLAY_CONFIG` 누락을 거부하는 기존 fail-closed 계약과는 별개의
  자동 탐색 경로 결함이다.

### 수정 및 검증

- schema 8 DB와 config 부재를 실제 CLI subprocess로 구성하는 회귀 테스트를 추가했다.
  수정 전에는 사용자와 같은 `os error 2`로 실패하는 RED를 확인했다.
- 자동 config 경로가 없을 때는 이미 검증된 effective default document로 config preview를
  만들고 config write를 생략하도록 최소 수정했다. 명시적 config 누락 오류는 유지한다.
- 대상 테스트는 schema 11 도달과 `.colay/config.toml` 비생성을 확인하며 GREEN이다.
- Windows source에서 대상 회귀 테스트와 기본 시작 통합 테스트 8개, workspace 전체
  target/feature Clippy `-D warnings`, 전체 Rust suite, npm 66개가 통과했다.
- WSL 2 Ubuntu 24.04의 Rust 1.95 Linux 컨테이너에서 source를 read-only mount하고 공식
  fake provider만 사용해 같은 schema 8→11 회귀 테스트를 통과했다. 실제 provider
  inference는 호출하지 않았다.
- repository config는 생성되지 않았고 migration manifest, DB backup, append-only event
  경로의 기존 계약을 유지한다. 상태를 `fixed`로 전환한다.

### 현재 우회

- 수정 nightly가 나오기 전에는 repository에서 `colay init`으로 현재 config를 생성한 뒤
  `colay migrate apply`를 실행할 수 있다. 기존 `.colay` DB/WAL/SHM 파일은 직접 삭제하지
  않는다.

## Confirmed healthy controls

- 설치된 Linux native binary는 x86-64 static PIE였고 `--version`이 정상 동작했다.
- npm nightly dist-tag와 설치된 build version이 일치했다.
- 격리 repository에서 `init`, migration schema v8, DB integrity, event log integrity가
  정상 동작했다.
- 기존 사용자 DB의 integrity와 append-only event hash chain은 정상으로 확인됐다.
- pseudo-terminal에서 TUI를 열고 `q`로 종료하는 흐름은 정상 동작했다.
- Windows npm root package와 `win32-x64` optional package의 nightly version이 일치했고,
  native binary는 정상 AMD64 PE였다.
- Windows 격리 repository에서 `init`, `doctor`, `providers`, `compatibility`, `status`,
  `run --plan-only`, daemon start/status/stop이 fake provider로 정상 동작했다.
- Windows `doctor`의 6개 check가 모두 pass였고 schema v8, DB integrity, foreign key
  integrity가 정상이었다.
- Windows에서 `cargo fmt --all -- --check`, 전체 clippy `-D warnings`, npm 테스트 65개가
  통과했다. 최초 nightly QA에서는 Rust 402개, Git readiness 수정 후에는 신규 회귀를
  포함한 Rust 407개, SQLite 수정 후 409개, provider lease 수정 후 411개가 통과했다.
- QA 과정에서 실제 Codex, Claude, Gemini inference는 호출하지 않았다.

## Prioritized improvement queue

1. `완료`: 모든 session 입력을 task로 시작하지 않는 conversation-first plan mode를
   기본 TUI 진입점으로 두고, Git preflight는 승인 후보 승격 직전에만 수행한다.
2. `완료`: provider terminal 오류를 finalization하고 장기 고정 lease를 짧은 renewable
   lease로 교체했다 (`WSL-008`).
3. `완료`: idle daemon의 불필요한 immediate transaction을 제거하고 writer starvation을
   회귀 테스트로 고정했다 (`WSL-003`).
4. `완료`: daemon startup timeout 시 child 정리와 phase diagnostics를 보장한다.
5. `완료`: 실패 후 `planned` task는 `colay resume <task-id>`로 동일 task/worktree를
   재사용하며, fake CLI 회귀 테스트로 중복이 없음을 고정한다.
6. `완료`: WSL/NVM과 Windows Cargo/npm 충돌을 포함한 CLI/daemon 실제 binary 경로와
   build identity를 `doctor`에 노출한다.
7. `완료`: Windows PE의 Authenticode 부재와 enterprise 지원 제한을 release/security
   문서에 명시한다.
8. `완료`: `/mnt/c` mixed-Git 환경 경고와 WSL native clone 문서를 추가한다.
9. `완료`: 500ms reconnect 테스트를 condition-based wait로 바꿨다 (`WSL-007`).
10. `완료`: config 파일이 없는 기존 DB도 기본 설정으로 migration할 수 있게 하고,
    명시적 config 누락은 계속 fail-closed로 유지한다 (`WSL-009`).
11. `fixed-in-source, deployment-pending`: 실제 provider에게 strict `ConversationOutcome` JSON
    shape를 전달하고 검증된 output-schema 기능을 연결한다 (`WSL-020`).
12. `fixed-in-source, deployment-pending`: doctor가 source fingerprint와 완료 import ledger를
    대조해 pending과 already-imported를 구분한다 (`WSL-021`).

## Update log

### 2026-08-02

- PR #16의 Ubuntu/macOS/Windows CI 6개 통과, merge commit `8f2654ac`, Release run
  `30749716792`, npm nightly `0.1.1-nightly.20260802.8f2654a`를 확인했다.
- WSL Ubuntu 24.04의 새 npm prefix와 격리된 `COLAY_HOME`에서 schema-8 private source를
  검사하고 schema 16 migration, legacy import, daemon online/status/stop을 완료했다. 원본
  legacy DB hash는 변경되지 않았고 양쪽 DB integrity와 foreign key 검사는 통과했다.
- 이 배포 검증으로 `WSL-019`를 `fixed`로 전환했다.
- 실제 Codex `0.146.0`이 생성한 정상 답변 JSON이 prompt에 누락된 strict field 계약 때문에
  거부되는 `WSL-020`을 추가했다. Claude quota와 Gemini authentication 실패는 계정 상태로
  분리했다.
- 완료 import ledger와 동일 fingerprint가 있는데도 offline doctor가 pending으로 표시하는
  `WSL-021`을 추가했다. QA daemon은 stopped로 정리했고 timestamped 증거 디렉터리는
  보존했다.

### 2026-07-23

- nightly `0.1.1-nightly.20260722.f693062`의 schema 8 사용자 DB에서 config 파일 부재로
  `migrate apply`가 시작 전에 실패하는 `WSL-009`를 추가했다. schema 8 DB를 사용하는 CLI
  회귀 테스트로 RED를 확인하고, effective default config로 DB migration만 수행하는 최소
  수정 후 Windows와 WSL Linux에서 GREEN을 확인했다. workspace 전체 Clippy, Rust suite,
  npm 66개도 통과해 `fixed`로 전환했다.
- 수정 전 전체 Windows suite에서 `WIN-003`의 `icacls.exe` 접근 거부가 두 번째로
  관찰돼 상태를 `open`으로 재개했다. 같은 테스트의 즉시 단독 3회는 모두 통과했다.
- `/plan`이 요구사항 revision 없이 approval 후보를 만들던 우회 경로를 차단하고, 새 사용자
  메시지가 planning/awaiting-approval graph를 같은 transaction에서 supersede하도록 했다.
- session의 실제 `validating` 상태, Agy provider compatibility, exact graph approval authority,
  daemon executable/build identity를 schema v11 projection으로 추가했다. migration v1→v11과
  backup pending-plan 회귀 테스트가 통과했다.
- requirement를 scope/acceptance/구조화 verification command/risk/open-question으로 확장하고,
  shell interpreter·metacharacter가 포함된 검증 명령을 approval 전에 fail-closed하도록 했다.
- 자동 conversation→complete requirement→validation→exact approval→두 task materialization→
  scheduler isolated worktree→fake worker completion E2E가 Windows source build에서 통과했다.
  승인 전 task/worktree/worker lease는 0건이며 실제 provider inference는 호출하지 않았다.
- provider timeout과 conversation future cancellation을 fake runtime으로 검증하는 회귀를
  추가했고, daemon restart가 interrupted conversation/command를 terminal failed로 함께
  정리하도록 했다.
- `planned` task의 명시적 `colay resume <task-id>` 경로, CLI/daemon identity 진단,
  `/mnt/<drive>` mixed-Git 경고, WSL native clone 안내, unsigned Windows PE enterprise 지원
  제한을 코드·테스트·운영 문서에 반영했다. 전체 Windows/WSL 게이트 결과는 아래 최종 QA
  실행 후 이 항목에 이어 기록한다.
- Windows source QA에서 Agy가 provider 관리 CLI enum에서 누락된 `WIN-004`를 추가로
  발견했다. Agy selector와 profile target을 추가하고 실제 `providers disable agy`, 전체
  Clippy, 전체 Rust suite로 수정 완료를 확인했다.
- 최종 Windows source 검증에서 `cargo fmt --all -- --check`, workspace 전체 target/feature
  Clippy `-D warnings`, `cargo test --workspace --all-features`, Node 24의 npm 66개 테스트가
  통과했다. 격리 Git repository의 `init`은 schema v11을 적용했고 `doctor`는 database,
  current runtime, active daemon runtime, fake Codex provider를 모두 pass로 보고했다.
- WSL 2 Ubuntu 24.04의 Rust 1.95 Linux container에서 source를 read-only mount하고
  conversation answer/interview/candidate/timeout/cancel 4개, `/mnt/c` mixed-Git 경고,
  conversation→exact approval→scheduler worktree E2E를 통과했다. 최초 slim image 실행의
  `os error 2`는 제품이 아니라 image에 Git package가 없던 환경 오류였고, Git을 임시
  설치한 동일 container에서 E2E가 통과했다. Linux build volume은 검증 후 제거했다.
- 최종 Windows/WSL QA에서도 실제 Codex, Claude, Gemini, Agy 모델 inference는 호출하지
  않았다. 수동 provider 진단은 공개 `--version`/`--help` capability probe에만 한정했다.

### 2026-07-22

- 최초 WSL nightly 설치 및 전체 QA 결과를 정리했다.
- daemon startup orphan race와 SQLite writer starvation을 기록했다.
- `not a git repository`와 unborn `HEAD` 오류가 task 영속화 이후에 발생하는 preflight
  결함임을 기록했다.
- `~/workspace/test`의 빈 Git repository에서 unborn `HEAD` 오류가 동일하게 재현됐고,
  `WSL-005`의 두 번째 발생 사례로 추가했다.
- 처음에는 현재 `run --plan-only`로 강등하는 방향을 기록했으나 사용자 피드백에 따라
  폐기했다. 원하는 plan mode는 provider 기반 질의응답과 요구 명확화를 수행하는
  task 이전 session이며, orchestrator가 worktree 작업 필요성을 판단한 뒤에만 Git
  preflight와 task materialization을 수행하는 conversation-first 흐름으로 정정했다.
- task 승격은 orchestrator 단독 판단으로 자동 실행하지 않고, 인터뷰·구체화·검증을 거친
  최신 계획에 사용자가 최종 승인한 경우에만 허용한다는 결정을 추가했다.
- 기존 TUI session/graph/approval 구조를 확장하는 접근법과 상태 모델을 사용자가 승인해
  별도 conversation-first plan mode 설계 문서로 구체화했다.
- `resume`이 Claude의 terminal credit 오류 뒤 open attempt와 장기 coordinator/worker
  lease를 남겨 다음 `resume`이 충돌하는 현상을 `WSL-008`로 기록했다. 동일 패턴이 두
  task에서 반복됐으며 target lease의 안전한 만료 시각도 함께 남겼다.
- Windows checkout과 WSL Git 줄바꿈 설정 차이를 기록했다.
- Windows 11 native QA에서 npm nightly package/native PE/provenance, safe CLI, daemon,
  SQLite 경쟁, Git edge case, 전체 fake-provider test suite를 검증했다.
- `WSL-003`, `WSL-004`, `WSL-005`가 Windows에서도 재현되어 WSL 전용이 아닌 공통
  이슈로 재분류했다.
- Windows PATH가 nightly 대신 Cargo `0.1.0`을 선택하는 `WIN-001`과, npm provenance는
  유효하지만 native PE에 Authenticode가 없는 `WIN-002`를 추가했다.
- Windows daemon start의 일부 PowerShell output-capture 지연은 제품이 아니라 QA harness
  제약으로 판별해 이슈로 등록하지 않았다. 실제 lifecycle과 repository 테스트는
  정상 통과했다.
- Git readiness 수정 작업을 시작했다. typed repository/base-commit 검사를 worktree
  엔진과 direct `run`의 state mutation 이전에 적용했으며, non-Git/unborn 회귀 테스트를
  Windows에서 추가했다.
- Git readiness 수정의 fmt, 전체 clippy, npm 65개, Rust 407개 검증이 통과해
  `WSL-004`와 `WSL-005`를 `fixed`로 전환했다.
- 전체 suite 첫 시도에서 기존 rollback 테스트의 `icacls.exe` 접근 거부가 한 번
  발생했지만 단독 3회와 전체 재실행에서는 통과했다. 이를 `WIN-003`으로 추가했다.
- idle command/scheduler poll에 read-only candidate precheck를 추가했다. Windows에서
  별도 writer가 활성인 회귀 테스트 2개와 fmt, 전체 Clippy, npm 65개, Rust 409개가
  통과해 `WSL-003`을 `fixed`로 전환했다. 전체 재검증에서 `WIN-003`은 재발하지 않았다.
- provider terminal 오류를 즉시 cancel/confirmed-wait 경로로 연결하고 coordinator 30초,
  worker 20초, renewal 5초의 bounded authority로 변경했다. 상세 lease 충돌 진단과 fake
  terminal credit 회귀를 추가하고 Rust 411개 전체 suite를 통과해 `WSL-008`을 `fixed`로
  전환했다.
- 전체 suite에서 `WSL-007`의 500ms reconnect 플래이크가 다시 재현돼 상태 기반 최대 5초
  대기로 교체했다. 단독 3회와 전체 suite 통과 후 `fixed`로 전환했다.
- daemon startup phase와 bootstrap heartbeat를 schema 9에 추가하고, provider별 bounded wait,
  exact process-tree 종료/reap, PID 일치 lease 실패·해제, redacted durable 진단을 적용했다.
  Windows slow fake probe와 전체 Rust 418개, npm 65개, fmt/Clippy 검증이 통과해 `WSL-002`를
  `fixed`로 전환했다. 이 전체 실행에서도 `WIN-003`은 재발하지 않았다.
- conversation-first domain/engine/provider/state/daemon/TUI 경계를 schema 10으로 구현했다.
  Windows에서 자동 답변, 인터뷰, provider 실패 redaction, non-Git 차단, 정확 승인 1회
  materialization, 승인 전 writable table 0건, Git HEAD drift 거부를 fake-provider와 임시
  Git repository로 검증했다. 승인 카드는 requirement/validation/base-commit authority를
  표시하며 새 사용자 메시지는 stale card를 숨긴다.
- WSL 재검증에서 비대화형 shell은 system Node 18을 선택하고 NVM `colay`를 찾지 못하는
  반면, 명시적 Node 22 PATH에서는 설치된 nightly root/native version과 Linux ELF가
  정상임을 확인했다. launcher에 Node 22 fail-fast를 추가하고 Windows/Linux에서 테스트했으며,
  `doctor.runtime`에 현재 native path/build/target 진단을 추가해 `WSL-001`을 `fixed`로
  전환했다. release schema 기대값도 v10으로 갱신해 npm 66개 테스트가 통과했다.
- 최종 Windows 검증에서 `cargo fmt --all -- --check`, workspace 전체 target/feature Clippy
  `-D warnings`, `cargo test --workspace --all-features`, npm 66개 테스트가 모두 통과했다.
  전체 Rust 실행에는 migration v1→v10, conversation/approval, daemon lifecycle, 실제 임시
  Git worktree/rollback 회귀가 포함됐고 `WIN-003`의 `icacls.exe` 오류는 재발하지 않았다.
- WSL 2에서는 Rust 1.95 Linux container에 source를 read-only mount하고
  `ordinary_answer_is_automatic_and_creates_no_writable_state`를 재컴파일·실행해 통과했다.
  실제 Codex/Claude/Gemini inference는 Windows와 WSL 검증 모두 호출하지 않았다.
- 향후 대화에서 새 오류가 확인되면 새 ID를 추가하거나 기존 항목의 상태, 증거,
  완료 조건, update log를 갱신한다.
