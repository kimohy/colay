# Provider boundary and post-decode redaction implementation plan

> Execute in the isolated `codex/qa-scenario-hardening` worktree. Use only
> `orchestrator-test-support` provider binaries in tests and CI. The user's explicit instruction to
> proceed without further questions is the approval for the selected design.

**Goal:** Close the vacuous WSL-035/036 gates by reproducing and fixing post-decode secret leakage,
then make byte, event, and canary conversation scenarios reproducible through the packaged CLI for
all four providers.

**Architecture:** Keep existing transport redaction and limits, add one daemon-local structural
redaction and revalidation boundary after engine deserialization, and add fixed typed fake-provider
fixtures that drive the real process/parser/collector/daemon/SQLite path.

---

### Task 1: Prove and fix post-decode outcome redaction

**Files:**
- Modify: `crates/orchestrator-daemon/tests/conversation_flow.rs`
- Modify: `crates/orchestrator-daemon/src/conversation.rs`

1. Add a `ConversationOrchestrator` fixture that returns valid outcome JSON containing literal
   Unicode escape sequences. The wire bytes must not contain `secret-token`, but the parsed
   response, every requirement list, and verification executable/arguments must decode to strings
   containing it.
2. Add a focused test that processes a real daemon command with `SecretRedactor`, then asserts the
   decoded canary is absent from the conversation attempt, assistant message, current requirement
   revision, client command, and raw SQLite bytes. Assert `[REDACTED]` is present and writable task,
   worktree, and lease rows remain zero.
3. Run the single test before production edits and capture the intended failure caused by the
   persisted decoded secret.
4. Add daemon-local owned transformations for `ConversationOutcome`, `RequirementSnapshot`, and
   `VerificationCommand`. Redact every string independently without joining separated command
   arguments.
5. Apply the transformation immediately after engine collection and before fallback notices or
   persistence. Revalidate the transformed outcome; convert invalid transformed structures to the
   existing `ConversationFailure::Validation` path with bounded redacted evidence.
6. Run the focused test GREEN, then the complete daemon conversation-flow test target. Add a
   validation-failure test if a redacted executable/argument can exercise that fail-closed branch.

### Task 2: Add selectable fake conversation canary and boundary scenarios

**Files:**
- Modify: `crates/orchestrator-test-support/src/runtime.rs`
- Modify: `crates/orchestrator-test-support/tests/provider_e2e.rs`

1. Add RED tests naming three missing behaviors: `scenario:decoded-secret` preserves Unicode
   escapes through the provider wire layer, `scenario:byte-overflow` crosses the process frame
   boundary, and `scenario:event-overflow` emits more than 4,096 normalized events below the byte
   limit.
2. Run the focused fake-provider tests and record that the current dispatcher selects ordinary
   success for all three prompts.
3. Add typed `FakeRuntimeScenario` variants and one prompt-to-scenario selector shared by marker
   recording and fixture emission. Preserve all current selectors and crash/timeout behavior.
4. Change the fake conversation marker compatibly to retain `invocation_count` and also record the
   selected provider and scenario as harmless positive controls.
5. For `DecodedSecret`, build the inner outcome as raw JSON text so outer provider serialization
   preserves literal `\\uXXXX` sequences until the engine's second decode.
6. For `ByteOverflow`, emit provider-valid framing whose single conversation payload crosses the
   1 MiB boundary. For `EventOverflow`, emit at least 4,097 provider-valid assistant events whose
   total bytes stay below 1 MiB. Use each provider's existing Codex, Claude, Gemini, or Agy wire
   shape.
7. Run focused runtime and process-backed provider tests GREEN. Confirm existing ordinary,
   ambiguity, warning-between-deltas, crash, timeout, and Agy boundary fixtures remain unchanged.

### Task 3: Exercise all providers through CLI, daemon, and durable state

**Files:**
- Modify: `crates/orchestrator-cli/tests/global_plan_first.rs`
- Modify as evidence requires: `crates/orchestrator-cli/src/conversation_orchestrator.rs`
- Modify as evidence requires: `crates/orchestrator-providers/src/process_runtime.rs`
- Modify: `docs/qa/wsl-nightly-error-tracker.md`

1. Extend `PlanFixture` to configure Codex, Claude, Gemini, and Agy with the exact fake binary and
   to run a plan-only turn with an explicitly requested provider. Preserve the existing default
   Codex behavior.
2. Add a recursive byte scanner for the isolated stdout, stderr, state database family, JSONL, and
   artifact tree. It must derive expectations from literal canary and marker values, not from the
   production redactor.
3. Add a four-provider decoded-secret matrix. For every row assert exit zero, selected marker
   provider/scenario, exactly one invocation, `[REDACTED]` positive control, no decoded canary on
   any scanned surface, SQLite integrity/foreign keys, and zero writable rows.
4. Add four-provider byte-overflow and event-overflow matrices. For every row assert a bounded
   nonzero result, exact one-start/no-retry marker, terminal `needs_attention`, evidence no larger
   than 16 KiB, no success, no over-bound sentinel, and zero task/worktree/lease rows.
5. Run the new tests RED against any still-missing classification. If internal safety cancellation
   is reported as user cancellation, trace lifecycle precedence to its source and add the minimum
   production correction plus regression test; do not special-case CLI wording.
6. Run the full `global_plan_first` test target and related conversation/provider targets GREEN.
7. Update WSL-035 and WSL-036 tracker sections with the confirmed defect, source fix, exact local
   scenario matrix, and remaining deployed-nightly gate. Record WSL-040 as external WSL/systemd
   cold-start evidence without claiming a Colay fix.

### Task 4: Review and complete branch verification

1. Run `cargo fmt --all -- --check`.
2. Run `cargo clippy --workspace --all-targets --all-features -- -D warnings`.
3. Run `cargo test --workspace --all-features` from a clean exact commit. Cold-build duration is not
   a hang; observe child CPU/I/O before diagnosing inactivity.
4. Run `git diff --check`, inspect the exact file set, and scan source/test output for the synthetic
   canary outside the intentional fake fixture literals.
5. Obtain independent code and test reviews. Resolve every Critical or Important finding and rerun
   affected gates.

### Task 5: Publish, merge, deploy, and validate WSL nightly

1. Push the branch, open a PR, inspect all GitHub Actions logs, and merge only when required checks
   are green and review has no blocker.
2. Wait for the release workflow tied to the exact merge commit and verify Linux, Windows, macOS,
   smoke, attestation, and npm publication outcomes.
3. In WSL, remove the isolated test installation, clean-install the exact new nightly, and record
   package version plus native binary hash. Do not alter the user's unrelated global installation.
4. With fresh per-scenario workspaces and one isolated user-global database, run the four-provider
   fake decoded-secret, byte-overflow, and event-overflow matrix. Scan CLI output, SQLite family,
   JSONL, artifacts, tasks, worktrees, leases, and invocation markers.
5. Run bounded read-only real-provider turns for Codex, Claude, Gemini, and Agy under the same
   policy. Treat recognized authentication/quota/billing failures as external only after doctor,
   compatibility, zero-retry, and state-hygiene evidence passes.
6. If deployed evidence closes WSL-035/036, publish the tracker update through a follow-up docs PR
   with exact version, merge SHA, artifact hashes, and retained evidence paths. Otherwise reproduce
   the residual defect on source, return to Task 1 or 3 with a new RED test, and continue without a
   blind retry.
