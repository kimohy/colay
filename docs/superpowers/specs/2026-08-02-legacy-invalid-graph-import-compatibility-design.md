# Legacy Invalid-Graph Import Compatibility Design

Date: 2026-08-02
Status: Approved for implementation

## Context

Manual WSL QA of public nightly `0.1.1-nightly.20260801.3f4e2f7` reproduced a daemon startup failure from the user's normal NVM installation:

```text
$ which colay
/home/kimohy/.nvm/versions/node/v22.23.1/bin/colay
$ colay run hello
error: user daemon contenders exited before IPC readiness; last exit: exit status: 1
```

Running the same binary as a bounded foreground daemon exposed the hidden bootstrap error:

```text
error: JSON serialization failed: missing field 'node_count' at line 1 column 72
```

The user-global database was healthy at schema 16, and `colay doctor` passed. The failure occurred while registering `/home/kimohy` and inspecting its repository-local schema-13 legacy store at `/home/kimohy/.colay/orchestrator.db`. That store contains one legitimate terminal graph revision with these non-sensitive structural properties:

- `status = invalid`;
- `proposal_json IS NULL`;
- `proposal_hash IS NULL`; and
- `validation_json` is an object with an `errors` array.

The current graph model intentionally persists invalid attempts with arbitrary structured validation evidence such as `{"errors":[...]}`. However, `validate_source_graphs` unconditionally deserializes every `validation_json` value as `GraphValidationSummary`, whose successful-validation shape requires `node_count`. The importer therefore rejects a valid historical record before the daemon registers a lease or publishes IPC. The top-level startup client suppresses daemon stderr and reports only the contender exit status, while `doctor` checks the global database but not the pending repository-local import.

This is tracked as `WSL-019`. The defect is cross-platform source behavior even though it was discovered through WSL nightly QA.

## Goals

- Import legitimate historical graph attempts without deleting, rewriting, or synthesizing their validation evidence.
- Continue validating successful graph proposals, seals, authority, identity, and approval relationships fail-closed.
- Reject incomplete proposal/hash pairs and malformed JSON.
- Keep the state schema at version 16; this is an importer validation correction, not a persisted schema change.
- Make `doctor` report whether the current repository-local legacy store is import-ready before daemon startup.
- Make generic pre-IPC daemon exits direct the user to the diagnostic that can expose an import blocker.
- Add `WSL-019` evidence, root cause, correction, and Windows/WSL verification to the QA tracker.

## Non-Goals

- Do not mutate or delete `/home/kimohy/.colay/orchestrator.db` during development or verification.
- Do not convert invalid validation evidence into a synthetic `GraphValidationSummary`.
- Do not skip invalid, planning, cancelled, or superseded graph rows merely to complete an import.
- Do not weaken proposal hashes, approval gates, authority checks, event-chain validation, source snapshot guards, or append-only audit behavior.
- Do not add a state migration or increment `STATE_SCHEMA_VERSION`.
- Do not invoke real Codex, Claude, Gemini, or Agy inference from tests or CI.
- Do not persist unbounded daemon stderr or expose credentials through startup diagnostics.

## Selected Architecture

### Shape-aware legacy graph validation

Legacy validation follows the persisted proposal/hash shape instead of assuming that every graph attempt completed successfully.

For each graph revision:

1. Parse `validation_json` as `serde_json::Value` so malformed JSON always fails.
2. When `proposal_json` and `proposal_hash` are both present:
   - deserialize the validation value as `GraphValidationSummary`;
   - deserialize the proposal as `TaskGraphProposal`;
   - verify proposal revision, session, and goal identities;
   - verify the task-graph schema version is supported;
   - recompute and compare the proposal hash; and
   - compare embedded validation authority with the row-level requirement, validation hash, and base commit.
3. When both proposal and hash are absent:
   - preserve the validation value exactly as historical evidence;
   - require row-level authority columns to be absent; and
   - reject any graph approval that targets the unsealed revision.
4. When exactly one of proposal or hash is present, reject the source as an incomplete sealed document.

This accommodates the domain's existing `NewGraphAttempt::invalid` representation without introducing status-specific fabricated data. The optional-pair invariant is more durable than enumerating statuses because terminal and superseded revisions may legitimately have different payload shapes depending on when transition occurred.

`rewrite_graphs` already selects only rows with a non-null proposal. It continues to rewrite typed proposals and validation summaries and recompute their seals. Unsealed rows remain byte-for-byte unchanged by the graph rewrite path and are copied with the rest of the sealed source snapshot.

### Approval integrity

The existing approval hash comparison is retained for sealed revisions. Validation also explicitly rejects approvals whose target revision has no proposal/hash pair. This avoids relying on SQL `NULL` comparison behavior and prevents an orphaned approval from passing validation.

No approval is synthesized, removed, or rewritten for an unsealed historical attempt.

### Doctor import-readiness check

When `doctor` owns global-state maintenance and resolves the current repository, it constructs the same `RepositoryStatePaths` used by daemon registration and runs the read-only `LegacyImporter::inspect` path when a legacy database is present. Inspection continues to operate on guarded private snapshots and may use the existing locked global scratch area, but it does not mutate the source, global workspace rows, import ledger, audit log, or published artifacts.

Doctor emits a dedicated `legacy_import` check:

- pass when no repository-local legacy store exists;
- pass with structural plan data when inspection succeeds;
- fail with the bounded importer error when inspection finds an incompatible or invalid source; and
- report that the check is unavailable when maintenance ownership cannot be obtained and the live daemon path cannot inspect the repository source.

The check does not expose prompt content, validation error text, credentials, or raw JSON. It reports only import readiness, source schema, and non-sensitive plan metadata already suitable for diagnostics.

### Startup recovery guidance

If every spawned daemon contender exits before IPC readiness, the client retains the existing exit-status evidence and appends an actionable instruction to run `colay doctor`. It does not capture or persist raw daemon stderr. This keeps the startup path bounded and avoids creating a new sensitive log surface.

After the importer correction, the reproduced WSL store should start normally. The guidance remains useful for other pre-IPC bootstrap failures, and the new doctor check makes this specific class diagnosable without the hidden `daemon serve` command.

## Error and Safety Semantics

- Invalid historical attempts are evidence, not corruption, when their proposal and hash are both absent and their validation is valid JSON.
- A sealed proposal still requires a typed successful validation summary and an exact recomputed seal.
- Optional authority columns must agree exactly with typed validation authority for sealed rows and must all be absent for unsealed rows.
- Any proposal/hash mismatch, malformed JSON, unsupported graph schema, identity mismatch, forged authority, or approval of an unsealed revision fails before target state mutation.
- Inspection and failed import leave the source database bytes, global rows, import ledger, events, artifacts, and published legacy snapshot unchanged.
- Diagnostic output remains bounded and redacted; no provider request is made.

## Testing Strategy

### State-layer TDD regressions

The first failing regression creates a schema-13 repository-local database through existing fixtures and persists an invalid graph attempt with `proposal_json = NULL`, `proposal_hash = NULL`, and `validation_json = {"errors":["cycle"]}`. It asserts:

- `LegacyImporter::inspect` succeeds;
- `LegacyImporter::apply` imports the workspace;
- the imported graph retains `status = invalid`;
- proposal and hash remain absent;
- validation JSON remains semantically identical;
- no graph approval is created; and
- source database bytes remain unchanged.

Additional regressions assert:

- a valid sealed graph still imports and its rewritten hash remains verifiable;
- a proposal without a hash, or a hash without a proposal, fails closed;
- row-level authority on an unsealed graph fails closed;
- an approval targeting an unsealed graph fails closed; and
- malformed validation JSON fails closed.

### CLI diagnostic regressions

Fake/local fixtures verify that:

- `doctor` passes `legacy_import` for no source and for an import-ready invalid graph source;
- `doctor` fails `legacy_import` for a structurally invalid source without starting a provider or mutating state;
- the pre-IPC contender failure contains the `colay doctor` recovery instruction; and
- normal daemon startup behavior and child cleanup remain unchanged.

### User-flow verification

Windows source verification uses copied fixture state only. WSL verification first uses a private copy of `/home/kimohy/.colay` and an isolated `COLAY_HOME`, then runs the compiled Linux binary through `doctor`, migration/import, daemon start/status/stop, and a fake-provider conversation. Only after those pass is the public user path tested with non-destructive commands. The original repository-local database is hashed before and after and must remain unchanged.

All automated provider behavior uses `orchestrator-test-support` fake binaries. Required repository gates are:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

`git diff --check` and focused state/CLI tests run before the full gates.

## QA Tracker Update

`docs/qa/wsl-nightly-error-tracker.md` gains `WSL-019` with:

- observed public nightly and NVM path;
- generic startup symptom and foreground root-cause diagnostic;
- global schema/integrity and zero-daemon-row evidence;
- structural legacy graph evidence without prompt/error content;
- source-fixed verification results; and
- deployed-nightly WSL verification status, initially pending until a new public nightly is available.

The issue moves from `open` to `source-fixed` only after focused and full Windows gates plus WSL compiled-binary QA pass. It moves to `fixed` only after the public nightly is installed cleanly and the user's preserved legacy store starts successfully.
