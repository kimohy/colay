# Codex Stable Compatibility Release Asset Design

**Status:** Approved for implementation

**Date:** 2026-07-26

## Context

The scheduled `Codex stable compatibility` workflow clones the latest `rust-v*` release tag and
runs `cargo build --locked`. Upstream release tags rewrite the workspace package version while the
checked-in lock file retains development package versions. Cargo therefore requires a lock-file
update, and every scheduled run stops before collecting the public compatibility contract.

Removing `--locked` would make the monitor resolve a large, time-dependent dependency graph. That
would test a locally reconstructed binary rather than the artifact users actually install.

## Decision

The workflow will inspect the official Linux x86_64 release package instead of compiling the
release source tree.

For the resolved exact tag it will download both:

- `codex-package-x86_64-unknown-linux-musl.tar.gz`
- `codex-package_SHA256SUMS`

It will select the package's exact checksum entry, reject missing or duplicate entries, verify the
archive with `sha256sum --check`, extract into a fresh directory, and require the executable at
`bin/codex`. The package for `rust-v0.145.0` was checked during design: it contains `bin/codex`, and
its published SHA-256 matches the downloaded archive.

The workflow will continue to clone the exact tag only to record the upstream commit and supply
source context for the compatibility report. It will not build that checkout. Public
`--version`, help, and schema commands will run against the verified release binary without
credentials or inference.

## Boundaries and Failure Handling

- Release tag validation remains `rust-v<semver>` and the workflow never follows an unvalidated
  asset name.
- HTTP download failure, missing or duplicate checksum entries, checksum mismatch, unsafe archive
  paths, missing `bin/codex`, or a non-executable binary fails the inspection before probing.
- Archive extraction must reject absolute paths and `..` traversal before invoking `tar`.
- The report records the tag, source commit, asset filename, and verified SHA-256 so later reviews
  can identify exactly what was inspected.
- No provider credentials, inference, usage scraping, unofficial endpoint, or external telemetry
  is introduced.
- Draft report publication permissions and behavior remain unchanged.

## Implementation Shape

A small repository script will own deterministic asset selection, checksum verification, archive
path validation, extraction, and provenance output. The workflow will call that script rather than
embedding security-sensitive parsing in YAML. The script accepts explicit local input paths so its
contract can be tested entirely with fixture archives and checksum files.

The workflow remains responsible for resolving the tag, downloading the two official assets from
the corresponding GitHub release, and passing explicit paths to the script. The capture step reads
the verified binary path and asset provenance produced by the script.

## Verification

Tests will first demonstrate the current absence of the asset verification interface. They will
then cover:

- a valid fixture package and checksum;
- missing and duplicate checksum entries;
- checksum mismatch;
- absolute and parent-directory archive traversal;
- missing `bin/codex`;
- provenance containing the exact asset name and digest;
- static workflow assertions that source `cargo build --locked` is absent and the verified binary
  output feeds the existing non-inference capture.

Repository verification will include the new focused tests, workflow YAML parsing, required Rust
format, Clippy, and workspace tests. A `workflow_dispatch` run for an exact stable tag is the final
remote proof after the branch is pushed or merged; local implementation will not invoke real
provider inference.

## Alternatives Rejected

- **Remove `--locked`:** resolves thousands of dependency changes and does not inspect the shipped
  artifact.
- **Regenerate the upstream lock file:** has the same dependency-drift problem and mutates a
  third-party release checkout.
- **Keep source building and pin a pre-release commit:** no longer represents the exact stable
  artifact selected by the workflow.
