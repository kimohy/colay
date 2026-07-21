# Agy Provider Design

Date: 2026-07-21
Status: Approved

## Goal

Add Google's Antigravity CLI (`agy`) as a first-class Colay provider while
keeping the existing Gemini CLI provider fully available. Colay must record,
route, configure, and audit Agy independently from Gemini even though both may
serve Gemini models.

The implementation must preserve Colay's local-only security boundary. It must
use the installed official CLI and its existing authenticated state, invoke
processes without shell interpolation, keep writable workers in isolated Git
worktrees, retain explicit permission policy, and never inspect credentials or
scrape a usage page.

## Observed CLI Contract

The locally installed `agy` version used to validate this design is 1.1.4. Its
public help advertises these relevant options:

- `--print` (`-p`) for a single non-interactive prompt;
- `--mode` with `plan` and `accept-edits` execution modes;
- `--sandbox` for terminal restrictions;
- `--model` for model selection;
- `--conversation` for resuming a conversation; and
- `--version` and `--help` for non-inference capability probes.

It does not advertise a JSON or JSONL output option. Colay therefore must not
reuse the Gemini stream-JSON adapter or claim that Agy provides a verified
structured wire protocol. The official project describes Agy as the
Antigravity terminal CLI, and its changelog documents headless `--print` mode.

References:

- <https://github.com/google-antigravity/antigravity-cli>
- <https://github.com/google-antigravity/antigravity-cli/blob/main/CHANGELOG.md>

## Provider Identity and Configuration

Add `ProviderId::Agy` with the stable serialized value `agy`. Existing provider
values and schema versions remain unchanged. Persisted Agy records use the same
versioned, append-only envelopes as other providers.

Add an optional `agy` entry to `ProviderConfigs`. Compiled defaults configure
all four providers and set Agy to:

- enabled: `true`;
- executable: `agy`;
- quota period: calendar day;
- quota limit: unknown;
- priority: `80`; and
- usage source: manual override, configured probe, or local ledger only.

The resulting default routing priority is Codex 100, Claude 90, Agy 80, and
Gemini 70. Health and capability filtering still excludes an unavailable or
incompatible executable before routing, so enabling Agy does not make a missing
installation eligible.

Provider parallel-limit validation, enable/disable commands, TUI controls,
planner selection, worker construction, handover targets, profile editing, and
provider reports must all recognize `agy` as a separate provider.

## Model Profiles

Add a complete built-in Agy profile set using stable slugs exposed by the local
official CLI:

| Profile | Effort | Model |
| --- | --- | --- |
| Economy | low | `gemini-3.5-flash-low` |
| Standard | medium | `gemini-3.5-flash-medium` |
| Premium | high | `gemini-3.1-pro-high` |

The adapter passes the selected slug with separated `--model` arguments. It
does not pass an effort flag in the initial implementation because version
1.1.4 does not advertise one and the selected slugs already identify effort
variants. Future support for an independently advertised `--effort` contract
can be added without changing the provider identity or stored profiles.

## Adapter and Invocation

Create an `AgyAdapter` and `AgyAdapterConfig` in `orchestrator-providers`.
Provider-specific flags, help parsing, and output normalization remain in that
crate. `orchestrator-domain` stays vendor-neutral and I/O-free apart from the
new provider identity.

The adapter probes only `agy --version` and `agy --help`. It reports:

- non-interactive support when `--print` is advertised;
- read-only support when `--mode` and `plan` are advertised;
- writable support when `--mode` and `accept-edits` are advertised;
- session resume support when `--conversation` is advertised; and
- structured output as degraded, backed by Colay's bounded plain-text bridge,
  rather than as an Agy-provided structured protocol.

Prepared invocations use separated executable and argument values:

```text
agy --print --mode plan --sandbox [--model <slug>] [--conversation <id>]
agy --print --mode accept-edits --sandbox [--model <slug>] [--conversation <id>]
```

The first form is used for read-only planning and review. The second is used
only for writable workers already placed in isolated worktrees. Colay never
adds `--dangerously-skip-permissions`. Task-envelope JSON is written to stdin,
and no task content is interpolated into a shell command or argument string.

## Plain-Text Lifecycle Bridge

Add a dedicated `StructuredOutput::AgyText` transport marker. The name is a
Colay runtime classification; it does not assert that Agy emits structured
output.

For this transport only, the process runtime forwards bounded, redacted stdout
frames to the adapter and emits a synthetic provider-runtime exit protocol
frame after the direct child exits. The Agy adapter normalizes:

- stdout into `WorkerEvent::Message`;
- stderr into a non-lifecycle `WorkerEvent::Unknown` diagnostic;
- a successful exit protocol frame into `WorkerEvent::Completed`; and
- a non-zero exit protocol frame into `WorkerEvent::Error`.

The normal worker and planner loops still require a completed lifecycle event,
a zero exit code, and no lifecycle error before recording success. The bridge
therefore does not weaken completion rules for Gemini, Codex, Claude, or future
structured providers. Output limits, redaction, timeout, cancellation, process
tree termination, and executable-resolution evidence continue to come from the
shared hardened process layer.

Agy does not expose command, file-change, token-usage, or session-start events
through the observed public plain-text contract. Colay must not invent them.
Git snapshots and the existing independent verification gate remain the source
of truth for writable results.

## Usage and Quota Isolation

Agy and Gemini usage are never combined. In the absence of a configured JSON
probe, Agy returns an unknown usage snapshot in its own quota scope. Missing
values remain unknown, and raw quota units are not compared with another
provider. Administrators may use the existing manual override or explicit
configured-probe mechanisms when they have an approved source.

Quota-error text may be classified only through the existing conservative
provider error classifier. No usage-page scraping, unofficial API, credential
inspection, or inferred quota balance is introduced.

## Error Handling and Compatibility

An Agy installation is healthy only when its help output proves the required
mode for the requested sandbox and the bounded plain-text bridge is available.
Missing flags, a missing executable, malformed runtime protocol frames,
truncated lifecycle evidence, a non-zero exit, a timeout, cancellation, or
uncertain process-tree termination all fail closed.

Unknown stderr and stdout content is preserved only after redaction and within
configured bounds. It cannot directly create command evidence, verification
evidence, usage balances, or approval decisions.

Adding the `agy` serialized provider value is additive. Existing `gemini`,
`codex`, and `claude` configuration and persisted data continue to parse with
their current meanings. No schema version is changed solely for the additive
provider value.

## Testing

Development follows red-green-refactor. Tests are added before production code
and must demonstrate the missing Agy behavior before implementation.

Coverage includes:

- `ProviderId::Agy` parsing, display, serde, and persisted-record round trips;
- default and layered Agy configuration, provider limits, enable/disable, and
  priority ordering;
- all three Agy model profiles and CLI/TUI profile editing;
- exact separated Agy arguments for plan, accept-edits, model selection, and
  conversation resume;
- capability probing against fake `--version` and `--help` output;
- plain-text message normalization and synthetic success/failure lifecycle
  frames;
- timeout, cancellation, truncation, malformed protocol, and unknown-usage
  behavior;
- planner, worker, handover, status, provider report, and routing integration;
  and
- executable resolution on supported operating systems.

Every test and CI path uses `orchestrator-test-support` fake binaries. Tests
must never start real Codex, Claude, Gemini, or Agy inference. Documentation and
CI environment wording are updated to make that restriction explicit.

## Documentation and Verification

Update the README provider description and profile table, `config.example.toml`,
architecture documentation, testing policy, and relevant operations/release
wording to include Agy while preserving the distinction from Gemini.

Before completion, run the repository-required verification commands:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```
