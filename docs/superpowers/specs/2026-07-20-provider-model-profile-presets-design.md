# Provider Model Profile Presets Design

## Goal

Make Colay usable without requiring an end user to configure provider model names. Colay supplies a complete built-in model-profile preset for Codex, Claude, and Gemini, selects a logical profile automatically from task quality, and lets an administrator inspect, override, or reset the effective mapping through both the CLI and TUI.

## Scope

This change covers:

- built-in `economy`, `standard`, and `premium` model profiles for all three approved providers;
- automatic use of the selected profile's model and reasoning effort;
- administrator-facing CLI and TUI profile inspection and editing;
- audit evidence for the model settings used by a worker attempt; and
- tests and documentation for the preset and override behavior.

This change does not add custom providers, online model-catalog discovery, user-supplied per-run model flags, provider identity changes, quota workarounds, or inference-based health checks.

## Built-in Preset Matrix

The compiled configuration contains this complete matrix:

| Provider | Economy | Standard | Premium |
| --- | --- | --- | --- |
| Codex | `gpt-5.6-luna`, low | `gpt-5.6-terra`, medium | `gpt-5.6-sol`, high |
| Claude | `claude-haiku-4-5`, low | `claude-sonnet-5`, medium | `claude-fable-5`, high |
| Gemini | `gemini-3.1-flash-lite`, low | `gemini-3.5-flash`, medium | `gemini-3.1-pro-preview`, high |

The model identifiers are provider-supported versioned aliases or model IDs current on 2026-07-20. Administrators can override any entry through the existing layered configuration system when availability, entitlement, or organization policy differs.

Profile meanings remain vendor-neutral:

- `economy`: fast and cost-efficient for simple, well-scoped work;
- `standard`: the normal profile for everyday development work; and
- `premium`: the highest-quality profile for complex or high-value work.

## Configuration Architecture

`RootConfig::default()` supplies the complete matrix through a focused model-profile default constructor. The existing configuration precedence remains unchanged:

```text
compiled defaults
< $COLAY_HOME/config.toml
< <repository>/.colay/config.toml
< $COLAY_CONFIG
< --config
```

Existing deep table merging means an administrator may override a single provider/profile entry without repeating the remaining presets. For example, overriding only `orchestrator.model_profiles.claude.premium.model` preserves the other eight built-in entries and the preset effort value.

The existing `model_profiles` schema already represents this data, so the configuration schema stays at version 4 and no database migration is required. Direct TOML parsing remains backward compatible, including the existing empty-model behavior that delegates model choice to a provider. The new management interfaces do not create empty model values; administrators use reset to remove an override and restore the effective lower-precedence value.

## Automatic Selection and Execution

The routing engine keeps its current deterministic mapping from assessed task quality to logical model profile:

```text
economy quality  -> economy profile
standard quality -> standard profile
premium quality  -> premium profile
```

Users do not select a model or profile per run. After routing selects a provider and profile, `profile_settings()` resolves the effective model and effort from the merged configuration. The resolved values populate `WorkerRequest` and flow to the official provider CLI through separated process arguments.

Codex continues to pass reasoning effort only when its compatibility adapter verifies support. Claude's compiled provider default enables effort use, but the adapter must also observe `--effort` in `claude --help` before it passes the flag; older Claude CLI versions therefore omit the option safely. Gemini receives the configured model through `--model`; its adapter does not introduce an unsupported effort flag.

Handover and review workers resolve the destination provider's entry for the already-selected logical profile in the same way as primary implementation workers.

## CLI Interface

Add a top-level `profiles` command group:

```text
colay profiles
colay profiles --json
colay profiles set <provider> <profile> --model <model-id> [--effort low|medium|high]
colay profiles reset <provider> <profile>
```

`colay profiles` prints all nine effective mappings with provider, profile, model, effort, a short profile description, and whether the effective value matches the built-in preset or is customized. JSON output uses stable snake-case fields.

`profiles set` accepts only the approved providers and logical profiles. It requires a non-blank model and accepts only `low`, `medium`, or `high` effort. When `--effort` is omitted, the effective effort for that profile is preserved.

`profiles reset` removes the selected profile's model and effort from the writable override document, cleans up only empty tables created by the profile override, reloads the layered configuration, and reports the resulting effective mapping. It never deletes lower-precedence configuration.

Both mutations use the same explicit edit target, private-file handling, symlink rejection, atomic replacement, and effective-configuration reload used by existing provider controls. Draft validation happens before persistence, write or replacement failures preserve the previous file, and reload failures return an actionable error.

## TUI Interface

Add `f:profiles` to the TUI help line because `p` is already used for pause. The profile view contains all effective mappings:

```text
┌ Model Profiles ───────────────────────────────────────┐
│ Provider  Profile   Model                    Effort   │
│ codex     economy   gpt-5.6-luna             low      │
│ codex     standard  gpt-5.6-terra            medium   │
│ claude    premium   claude-fable-5           high     │
│ gemini    standard  gemini-3.5-flash          medium   │
│                                                      │
│ economy: fast and cost-efficient simple work         │
│ standard: everyday development work                  │
│ premium: complex work requiring the highest quality  │
│                                                      │
│ Up/Down select  Enter edit  Delete reset  Esc close  │
└──────────────────────────────────────────────────────┘
```

The editor allows model text entry and an effort choice from the three supported values. Save validates the draft and returns a typed TUI control action. Delete opens an explicit reset confirmation before returning a reset action. The CLI application layer performs persistence and reload, so the TUI crate remains I/O-free apart from terminal interaction and does not learn configuration paths.

The TUI snapshot carries presentation-safe effective profile rows. It never contains credentials or provider authentication state.

## Validation and Error Handling

The CLI and TUI reject unknown providers, unknown profile names, blank models, and effort values outside `low`, `medium`, and `high` before writing. Configuration reload remains the final validation gate.

Colay does not invoke inference or scrape usage/model pages to validate model entitlement. If an account cannot use a configured model or a provider retires an ID, the official CLI error follows the existing failed-attempt, retry, and handover path. Administrators can use `profiles set` to apply an organization-approved replacement or `profiles reset` to return to the compiled mapping.

The `WorkerStarted` append-only audit event adds the effective `model` and `reasoning_effort` beside the existing provider, sandbox, and profile fields. This records the exact requested settings for the attempt without changing persisted domain schemas or treating provider-specific model IDs as portable handover state.

## Testing

Tests use only fake provider binaries and non-inference probes.

- State configuration tests assert all nine compiled model/effort mappings.
- Layering tests assert that a one-entry override preserves every other preset.
- CLI parser tests cover profile listing, set, reset, provider/profile enums, and effort validation.
- CLI persistence tests cover atomic set/reset behavior, effective reload, and lower-precedence fallback.
- TUI unit tests cover profile navigation, draft validation, save actions, reset confirmation, and escape behavior.
- Provider adapter tests assert the exact separated `--model` arguments for Codex, Claude, and Gemini.
- Claude adapter tests assert that `--effort` is included only when enabled and advertised by the fake help output.
- CLI execution tests assert that `WorkerStarted` records model, profile, and effective effort.
- Existing routing tests continue to prove automatic quality-to-profile selection.

Required repository verification remains:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
```

## Documentation

Update `README.md` and `config.example.toml` with the built-in matrix, automatic selection behavior, CLI commands, TUI key, override example, reset semantics, and the fact that model availability depends on the installed official CLI and authenticated account.
