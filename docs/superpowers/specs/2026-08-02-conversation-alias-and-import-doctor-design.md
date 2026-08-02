# Conversation Alias and Legacy Import Doctor Design

Date: 2026-08-02  
Status: approved design, implementation pending

## Context

Nightly `0.1.1-nightly.20260802.8f2654a` fixed WSL-019: an unsealed historical
`invalid` graph no longer prevents the user daemon from reaching IPC readiness.
Clean WSL installation and private legacy-state QA verified schema-16 migration,
one completed import ledger row, 51 imported rows, the preserved invalid graph,
database integrity, and an unchanged source database hash.

That deployed-nightly QA found two follow-up defects:

1. WSL-020: real Codex `0.146.0` produced a valid read-only answer with the
   top-level field `response`, while the canonical provider-neutral
   `ConversationOutcome` requires `response_redacted`. The strict collector
   rejected the result.
2. WSL-021: after the import completed, offline `doctor` inspected the still
   present source and reported `pending: true` without correlating its fingerprint
   with the matching durable import ledger row.

Automated tests and CI must continue to use only `orchestrator-test-support` fake
provider binaries. Real provider inference is limited to bounded, explicitly
approved manual WSL QA. Future provider QA must include Antigravity through its
Colay provider identifier `agy`.

## Goals

- Accept the observed `response` spelling only at the provider conversation
  response boundary.
- Preserve `response_redacted` as the only canonical domain and persisted field.
- Make the conversation prompt state the exact canonical output shapes instead of
  relying on the internal Rust type name.
- Distinguish pending and already-imported legacy sources in offline and live
  doctor output without breaking existing `pending` consumers.
- Validate import ledger and published evidence before reporting an import as
  completed.
- Add Codex, Claude, Gemini, and Antigravity fake-provider coverage and include
  Antigravity in future bounded WSL provider QA.

## Non-goals

- Do not relax unknown-field rejection for `ConversationOutcome`.
- Do not add `response` as a domain-wide Serde alias.
- Do not persist, audit, or replay the noncanonical `response` spelling.
- Do not retry malformed provider output with another inference request.
- Do not add a provider-specific output-schema transport in this change.
- Do not change the state schema, migration manifest, public provider wire types,
  approval model, task materialization, or worktree rules.
- Do not make real provider inference part of automated tests or CI.

## Decision 1: Provider-boundary compatibility decoding

`orchestrator-domain::ConversationOutcome` remains unchanged and retains
`deny_unknown_fields`. `orchestrator-engine` gains a private provider-boundary
wire representation matching the four conversation variants. Only its
`response_redacted` fields declare `response` as an input alias.

The private wire value is deserialized directly from provider bytes and converted
into the canonical domain enum before existing validation. Direct Serde decoding
preserves duplicate-field detection: a payload containing both `response` and
`response_redacted` is rejected as ambiguous, even when the values are equal.
Unknown fields, non-string response values, missing required fields, malformed
JSON, incomplete requirement snapshots, and invalid outcome/requirement
combinations continue to fail closed.

The converted domain value contains only `response_redacted`. Durable conversation
attempts, audit events, IPC results, and any later serialization therefore remain
canonical. The alias input itself is not retained as evidence after successful
normalization.

## Decision 2: Explicit canonical conversation contract

The serialized conversation prompt will describe every canonical outcome shape:

- `answer_complete`: `outcome`, `response_redacted`
- `more_information_needed`: `outcome`, `response_redacted`, `requirements`
- `worktree_task_candidate`: `outcome`, `response_redacted`, `requirements`
- `needs_attention`: `outcome`, `response_redacted`, `evidence_redacted`

The existing requirements contract remains authoritative for `objective`,
`in_scope`, `out_of_scope`, `constraints`, `acceptance_criteria`,
`verification_plan`, `risks`, and `open_questions`. The prompt explicitly says
that unknown fields are forbidden and that providers must emit
`response_redacted`, not the compatibility alias `response`.

This fixes the source of the observed Codex deviation while keeping the alias as a
bounded compatibility guard. Provider-specific output-schema transport is left
out of scope because it would require a wider request and adapter contract and is
not needed to make the provider-neutral prompt self-describing.

## Decision 3: Ledger-aware legacy import readiness

The state layer owns all durable import interpretation. It exposes a read-only
completion query that accepts the registered workspace, inspected source plan,
and caller-selected global paths. The query:

1. finds a ledger row by source fingerprint;
2. requires the row to belong to the expected workspace;
3. validates indexed columns against `result_json`;
4. validates replayed audit and ID-mapping evidence;
5. validates the published import snapshot below the expected workspace data
   root; and
6. returns a typed pending/imported result without changing the ledger or the
   source.

The existing idempotent apply path may continue to report that no new import was
performed, but doctor completion status must not derive from that operation-local
boolean. Doctor uses the stored, validated completion state instead.

## Doctor data contract

The existing `pending` boolean remains for backward compatibility. Doctor adds an
`imported` boolean with these meanings:

| Source and ledger state | `pending` | `imported` | Check status |
| --- | ---: | ---: | --- |
| no legacy source | `false` | `false` | pass |
| valid source, no matching ledger | `true` | `false` | pass |
| valid source and matching validated ledger | `false` | `true` | pass |
| source fingerprint changed after an older import | `true` | `false` | pass |
| invalid source, corrupt ledger, or corrupt published evidence | not asserted | not asserted | fail |

Offline doctor performs source inspection under maintenance ownership, opens the
global database read-only, resolves the current repository workspace when
available, and asks the state layer for the completion result. A missing or
pre-current global database cannot contain a usable completion ledger, so a valid
source remains pending.

For live doctor, source inspection remains outside database writer ownership. The
typed doctor IPC request carries the inspected source fingerprint when one is
available. The daemon resolves the registered workspace and asks the same state
API to validate a matching completion. The IPC response contains only the typed
readiness projection needed by the client; repository-controlled error text and
published paths are not exposed. New optional request/response fields are
backward-compatible within schema v1 and default to unavailable for an older peer.
If safe inspection or IPC correlation is unavailable, doctor retains a warning
and does not infer `imported: true`.

## Failure and redaction rules

- Provider alias normalization never turns a malformed lifecycle into success.
- One conversation turn performs at most one provider inference. There is no
  repair prompt or cross-provider runtime fallback after process start.
- Failed provider output retains the existing bounded, redacted terminal outcome.
- Dual response fields are rejected rather than choosing one.
- Ledger or published-evidence mismatches fail closed with fixed, bounded,
  source-value-free doctor details.
- Doctor inspection makes zero inference requests and performs no source, ledger,
  audit, workspace, or artifact mutation.
- Existing explicit source database paths in normal doctor structured data remain
  unchanged; no additional sensitive paths or prompt content are introduced.

## Automated verification

### Conversation compatibility

- Accept `response` for each of the four provider-boundary outcome variants.
- Convert and serialize accepted values with only `response_redacted`.
- Reject both response spellings in one object, non-string aliases, missing
  response fields, malformed JSON, and unrelated unknown fields.
- Keep requirement completeness and safe separated-command validation unchanged.
- Assert the serialized conversation prompt names every canonical outcome and
  required field, includes `response_redacted`, and does not advertise `response`
  as provider output.

### Provider matrix

- Run successful read-only conversation fixtures through Codex, Claude, Gemini,
  and Agy adapters.
- Assert selected provider identity, canonical outcome, bounded evidence, and zero
  writable task/worktree state.
- Preserve the repository rule that all test executables are fake binaries.

### Doctor matrix

- No source: `pending: false, imported: false`.
- Valid source before import: `pending: true, imported: false`.
- Matching completed import: `pending: false, imported: true`.
- Changed source fingerprint: pending again, not imported.
- Mismatched workspace, malformed ledger result, damaged published evidence, and
  repository-controlled source errors fail closed and remain bounded/redacted.
- Exercise both maintenance-owner and live-daemon IPC paths.
- Hash source bytes and count global workspace/import rows before and after doctor
  calls to prove read-only behavior.

Required repository gates are:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Deployed-nightly WSL QA

After CI, merge, and a successful nightly release, allocate a new timestamped npm
prefix, isolated Linux-native `COLAY_HOME`, non-Git workspace, and private copy of
the legacy source. Preserve the user's normal installation and original source.

The release QA must verify:

- npm nightly version, native package version, ELF architecture, and merge commit
  provenance agree;
- pre-import doctor reports pending with zero inference requests;
- schema-16 migration, import, daemon online/status/stop, DB integrity, and zero
  foreign-key violations succeed;
- offline and live doctor report the matching import as
  `pending: false, imported: true`;
- original and private-copy source hashes remain unchanged;
- a bounded real Codex conversation accepts the observed `response` compatibility
  shape and persists a canonical successful outcome;
- Claude, Gemini, Codex, and Antigravity run safe public compatibility probes;
- Antigravity performs one bounded real inference when its account is ready;
- an unavailable Antigravity account is recorded separately from product failure,
  while fake Agy automated coverage remains mandatory; and
- no credential values are queried or recorded, no writable task/worktree is
  created, and the final isolated daemon state is stopped.

The QA tracker closes WSL-020 and WSL-021 only after this deployed-nightly evidence
is recorded. Source-only tests change their status to source-fixed but do not close
them.
