# Provider Minimum-Version and Capability Compatibility Design

**Status:** Approved design awaiting written-spec review

**Date:** 2026-07-24
**Applies to:** Codex, Claude, Gemini, and Agy official CLI adapters

## Context

Colay currently gives Codex writable authority only to exact fixture-backed versions. A newer
Codex release can expose every required public interface and still be classified as `untested`,
which disables writable work. Claude, Gemini, and Agy already inspect public version/help output,
but they do not share one explicit minimum-version and capability decision model. Maintaining an
exhaustive allowlist for every provider release is not practical.

The product must allow a provider when either its version meets a reviewed minimum or its required
public capabilities are observed. Explicit evidence that a required capability is missing must
override an otherwise acceptable version. Read-only and writable authority must be decided
separately. Diagnostic probes must never start inference or inspect credentials.

Provider compatibility is user-wide rather than repository-specific. Its cache will therefore use
the planned user-wide SQLite database, in which repository and conversation state are partitioned
by `workspace_id`. The global-state design and migration are a prerequisite and will be specified
separately before this policy is implemented.

## Goals

- Apply one compatibility decision model to Codex, Claude, Gemini, and Agy.
- Permit an operation when the provider meets its minimum version or proves the capabilities that
  operation requires.
- Let explicit negative capability evidence override the version fallback.
- Preserve read-only conversation access when writable capability is unavailable.
- Make default `colay doctor` perform a fresh, non-inference compatibility check for all configured
  providers.
- Cache normal-startup assessments by executable fingerprint in user-wide state.
- Fail only the current execution when a runtime error suggests compatibility drift, and direct the
  operator to `colay doctor` without silently changing provider configuration or routing.
- Preserve schema safe mode, approval gates, worktree isolation, redaction, and provider-neutral
  domain boundaries.

## Non-goals

- No automatic provider installation, upgrade, downgrade, or replacement.
- No automatic failover to another provider after a compatibility-suspected error.
- No credential inspection, usage-page scraping, unofficial endpoint, prompt submission, or
  external telemetry.
- No user setting that lowers a compiled minimum version.
- No claim that a version-based fallback is fixture-equivalent to an exact tested version.
- No global-state schema or local-to-global migration design in this document.

## Compatibility Evidence Model

The current capability support representation must distinguish absence of evidence from evidence
of absence. The policy receives, for every capability, one of these semantic states:

- `unknown`: the probe could not determine whether the capability exists.
- `advertised`: a public help or schema surface names the capability.
- `verified`: the safe probe establishes the capability contract strongly enough for its intended
  use without starting inference.
- `degraded`: the capability is usable through a documented reduced path.
- `missing`: a successful, authoritative probe explicitly demonstrates that a required capability
  is absent or incompatible.

Transport-specific parsing stays in the provider or compatibility crate. `codex-compat` continues
to own Codex wire/schema interpretation. Claude, Gemini, and Agy keep their help/version parsing in
their provider adapters. The vendor-neutral policy receives only normalized versions, capability
states, evidence descriptions, and provider identity. `orchestrator-domain` remains I/O-free and
does not import provider wire types.

A probe failure is `unknown`, not `missing`. A capability may become `missing` only when the
provider-specific parser recognizes an authoritative successful response and can identify the
absent or incompatible contract. This prevents a transient process or filesystem error from
creating false negative evidence.

## Version Policy

Each provider has a release-owned compatibility manifest containing:

- the normalized minimum version, when a fixture-backed parser and reviewed floor exist;
- exact tested versions and their fixture identities;
- the read-only and writable required-capability sets;
- the probe contract revision used to interpret evidence.

An exact tested version remains `tested_exact`. A version at or above the floor may use the version
fallback even if one or more required capabilities are `unknown`. A provider without a committed,
fixture-backed floor cannot use the version branch; it can still qualify through observed
capabilities. This rule allows a safe staged rollout for provider version formats that are not yet
normalized without inventing a minimum.

The initial Codex floor is `0.144.5`, the oldest exact fixture-backed version currently committed.
Claude, Gemini, and Agy floors become active only when their implementation PR adds a public
version fixture, a deterministic parser, and a reviewed floor to the manifest. Until then, their
existing public capability probes remain the eligibility path. Minimum floors are product release
data, not administrator overrides.

## Decision Algorithm

Compatibility is evaluated separately for read-only and writable operation classes.

For an operation class with required capability set `R`:

1. If any capability in `R` is `missing`, deny that operation class.
2. Otherwise, if the normalized version is at or above the provider floor, allow it by version.
3. Otherwise, if every capability in `R` is `advertised`, `verified`, or an explicitly accepted
   `degraded` path, allow it by capability.
4. Otherwise, deny it because neither branch supplied sufficient evidence.

The writable capability set includes every read-only prerequisite plus the provider's writable
sandbox or permission-mode contract. Consequently, a provider can remain eligible for
conversation while being excluded from task routing.

The aggregate statuses are:

- `tested_exact`: exact fixture-backed version and required capabilities are not explicitly missing.
- `compatible_by_version`: allowed through the minimum-version branch.
- `compatible_by_capability`: allowed through the observed-capability branch.
- `read_only`: read-only is allowed and writable is denied.
- `incompatible`: authoritative evidence marks a required capability missing.
- `unavailable`: the executable or sufficient evidence cannot be obtained for either operation
  class.

Every assessment records its decision source. A newer unknown version is no longer denied merely
because it lacks an exact fixture. Schema, configuration, handover, and persisted-domain
incompatibility remain independent safe-mode blockers.

## Runtime Architecture

The design introduces three boundaries:

1. **Provider probe adapters** execute only allowlisted public diagnostic commands and normalize
   provider-specific output.
2. **Compatibility policy engine** combines manifest version data and normalized capability
   evidence into read-only and writable decisions. It is deterministic and has no process or
   filesystem I/O.
3. **Compatibility assessment store** loads and persists assessments by executable fingerprint in
   user-wide state. The CLI and daemon consume it through an interface rather than depending on a
   physical table layout.

The global-state design must provide the assessment-store interface before provider compatibility
implementation starts. Provider assessments are global and carry no `workspace_id`; task,
session, daemon, artifact, and audit records remain workspace-partitioned. A cache refresh records
redacted assessment evidence and preserves existing append-only audit semantics. Physical table,
retention, and global migration details belong to the global-state spec.

## Executable Fingerprint and Cache

The cache key contains:

- provider ID;
- canonical executable path;
- file size;
- filesystem modification time;
- normalized version reported by the executable.

Normal startup resolves path metadata and performs only the bounded `--version` check needed to
confirm the complete fingerprint. It reuses the newest complete assessment when that fingerprint
matches, skipping the deeper help/schema capability probes. There is no time-based expiry. A path,
size, modification-time, or reported-version change forces a fresh full safe probe. An incomplete,
corrupt, or schema-incompatible cache entry is ignored and cannot grant authority.

Default `colay doctor` always bypasses the cache and performs fresh probes. It attempts to persist
the new assessments, but a cache-write failure does not hide the observed result: doctor reports
both the live assessment and the persistence error. Probe execution uses bounded per-command
timeouts and limited provider-level concurrency.

## CLI and Doctor Experience

Default `colay doctor` checks every configured provider. A missing or disabled configuration is
reported explicitly rather than silently skipped. For each provider the output includes:

- configured and resolved executable;
- observed and normalized version;
- minimum version and whether it is met;
- read-only and writable required capabilities;
- observed, unknown, degraded, and explicitly missing capabilities;
- final read-only and writable decisions;
- aggregate status and decision source;
- executable fingerprint, probe-contract revision, and check time;
- cache persistence warning, when present.

The existing `colay compatibility` command remains available and uses the same assessment engine.
It may support an optional provider filter, but its default result is the same all-provider view so
there is one source of compatibility truth.

Conversation routing includes only providers whose read-only decision is allowed. Task planning
and writable execution include only providers whose writable decision is allowed. No route is
changed after a command begins.

## Runtime Compatibility Errors

Provider adapters classify errors into compatibility-suspected and operational categories.
Unknown lifecycle events, rejected public flags, protocol/schema mismatches, and structured-output
contract failures are compatibility-suspected. Authentication, quota, billing, network,
cancellation, timeout, and general process-crash errors retain their existing categories and are
not relabeled as compatibility problems.

A compatibility-suspected error fails the current attempt with redacted evidence and an actionable
message to run `colay doctor`. It does not persistently downgrade the provider, invalidate a
matching assessment, edit configuration, or automatically select another provider. Doctor's fresh
probe is the explicit revalidation path.

## Security and Trust Boundaries

The diagnostic allowlist is restricted to public version/help commands and stable schema
generation that cannot start a model turn. Commands use Rust `Command` with separated executable
and arguments. No shell interpolation is introduced.

Raw probe output remains untrusted. Parsers are bounded, redaction runs before durable evidence is
stored, and cache contents do not override explicit current negative evidence or persisted-schema
guards. The design introduces no identity rotation, quota bypass, credential extraction, credit
purchase, unofficial endpoint, or default external telemetry.

## Migration and Rollout

Implementation order is:

1. Specify and implement the user-wide SQLite database with workspace partitioning and the global
   compatibility-assessment store interface.
2. Add provider manifests, normalized version parsers, explicit `unknown` versus `missing`
   capability evidence, and the deterministic policy engine.
3. Route CLI, daemon, planner, and worker startup through the shared assessment.
4. Make default doctor and compatibility use the all-provider fresh-probe path.
5. Add runtime compatibility-error diagnostics and update operations, release, compatibility, and
   threat-model documentation.

Existing exact Codex fixtures remain authoritative regression inputs. The old behavior that maps
every unknown Codex version to read-only is removed only after the shared policy and cache are in
place. No persisted provider assessment is silently interpreted under a different probe-contract
revision.

## Verification Strategy

All tests and CI use `orchestrator-test-support` fake binaries. They never invoke real Codex,
Claude, Gemini, or Agy inference.

For every provider, table-driven policy tests cover:

- exact tested version;
- version at or above the floor with unknown capabilities, allowed with version provenance;
- version below the floor with all required capabilities observed, allowed with capability
  provenance;
- version at or above the floor with an explicitly missing writable capability, writable denied;
- read-only requirements present and writable requirements absent, read-only only;
- unparseable version and incomplete capability evidence, denied;
- probe failure represented as unknown rather than explicitly missing.

Integration tests cover:

- fingerprint match reusing a cached assessment after only a version check, without rerunning
  help/schema capability probes;
- executable path, size, modification time, or reported version invalidating the cache;
- default doctor bypassing cache and probing every configured fake provider;
- doctor showing live results when cache persistence fails;
- planner and conversation routing honoring separate read-only/writable decisions;
- compatibility-suspected errors failing only the active attempt and pointing to doctor;
- auth, quota, network, timeout, cancellation, and crash errors retaining their categories;
- schema safe mode and rollback approval protections remaining unchanged.

Required gates are `cargo fmt --all -- --check`, workspace Clippy across all targets/features with
`-D warnings`, `cargo test --workspace --all-features`, and the npm release test suite. Targeted
fake-provider integration runs execute on Windows and WSL Linux.

## Acceptance Criteria

- Codex `0.145.0` with no explicit missing required capability is eligible by the `0.144.5`
  minimum-version branch instead of being writable-disabled solely as untested.
- Any provider at or above its active floor is denied writable authority when a successful
  authoritative probe explicitly marks a writable requirement missing.
- A provider below its floor or without an active floor is usable when the relevant capability set
  is observed.
- Read-only conversation remains available independently of writable task authority.
- Default doctor freshly assesses all configured providers and reports evidence and decision
  provenance.
- Normal startup reuses only an exact fingerprint match; doctor always refreshes.
- Compatibility-suspected execution errors fail the current attempt and recommend doctor without
  persistent automatic mutation or failover.
- No test or CI path invokes real provider inference.

## Open Questions

None. The user approved the hybrid policy, negative-evidence precedence, current-attempt-only error
handling, all-provider scope, default deep doctor behavior, fingerprint-based cache invalidation,
and partial read-only degradation.
