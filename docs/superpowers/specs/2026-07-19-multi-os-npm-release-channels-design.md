# Multi-OS npm Release Channels Design

## Goal

Make Colay easy to install on the initial supported platforms with one primary
command:

```text
npm install --global @kimohy/colay
```

The initial platform set is Windows x64, macOS Apple Silicon, and Linux x64.
The release system provides nightly, beta, and stable channels from GitHub
Actions without invoking real Codex, Claude, or Gemini inference.

## Scope and non-goals

This change adds native release builds, npm packages, GitHub release assets,
release-channel automation, supply-chain metadata, launcher tests, and user
documentation. It does not add Windows ARM64, macOS Intel, Linux ARM64, package
manager integrations other than npm, auto-update behavior, code signing, a
container image, external telemetry, or any provider credential handling.

The public npm package is `@kimohy/colay`; the installed executable command
remains `colay`. Colay is distributed under the Apache License 2.0. The
repository root contains the unmodified Apache License 2.0 text in `LICENSE` so
GitHub can detect and display the repository license. `Cargo.toml` declares the
SPDX identifier `Apache-2.0` in `[workspace.package]`, every Rust crate inherits
that value with `license.workspace = true`, and all four npm manifests declare
`"license": "Apache-2.0"`. A `NOTICE` file is added only if Colay later gains
attributions that require one; this release work does not create an empty
placeholder notice.

## Considered delivery approaches

1. Publish a small root package with platform-specific packages selected through
   exact-version `optionalDependencies`. This avoids lifecycle download scripts,
   keeps each installation small, and lets npm perform native OS/CPU selection.
2. Publish one package whose `postinstall` script downloads a GitHub release.
   This reduces the package count but fails in environments that block lifecycle
   scripts or GitHub downloads and adds proxy, checksum, and partial-download
   behavior to installation.
3. Put all three native binaries in one npm package. This is mechanically simple
   but makes every user download binaries for two unrelated platforms.

Approach 1 is selected. GitHub release archives remain a direct-download path
for users who do not have Node.js.

## Package topology

The repository gains a private npm workspace root and four publishable packages:

```text
npm/
|-- colay/
|   |-- package.json
|   |-- LICENSE
|   `-- bin/colay.js
|-- colay-win32-x64/
|   |-- package.json
|   |-- LICENSE
|   `-- bin/colay.exe
|-- colay-darwin-arm64/
|   |-- package.json
|   |-- LICENSE
|   `-- bin/colay
`-- colay-linux-x64/
    |-- package.json
    |-- LICENSE
    `-- bin/colay
```

The root `@kimohy/colay` package declares all three native packages as
optional dependencies pinned to exactly the same version. Each native package
uses npm `os` and `cpu` metadata so npm installs only the matching artifact:

| npm package | npm OS/CPU | Rust target | Release runner |
|---|---|---|---|
| `@kimohy/colay-win32-x64` | `win32` / `x64` | `x86_64-pc-windows-msvc` | `windows-2022` |
| `@kimohy/colay-darwin-arm64` | `darwin` / `arm64` | `aarch64-apple-darwin` | `macos-14` |
| `@kimohy/colay-linux-x64` | `linux` / `x64` | `x86_64-unknown-linux-musl` | `ubuntu-22.04` |

The Linux binary uses musl so the initial Linux x64 artifact is independent of
a distribution's glibc version. The implementation must prove that the current
bundled SQLite and process dependencies build and run on this target before the
target is considered supported.

The CommonJS launcher maps `process.platform` and `process.arch` to one native
package, resolves only that package's known binary path, and starts it with the
original argument array and inherited standard streams. It uses Node process
APIs with separated executable and argument values; it never constructs a shell
command. It mirrors normal exit codes and forwards supported termination signals.
It supports maintained Node.js releases starting at Node.js 22 and declares
`"engines": { "node": ">=22" }`. There are no `preinstall`, `install`, or
`postinstall` scripts.

If the current platform is unsupported, the launcher reports the detected pair
and the three supported pairs. If the optional dependency is missing, including
after an `--omit=optional` installation, it reports the expected package name,
the exact reinstall command, and the GitHub Releases fallback. It does not
download or repair files automatically.

## Channel and version contract

All package versions in one release are identical and immutable. npm dist-tags
provide the mutable channel pointers:

| Source event | Version | npm dist-tag | GitHub retention |
|---|---|---|---|
| successful push to `main` | generated nightly version | `nightly` | Actions artifact for 14 days |
| tag `vX.Y.Z-beta.N` | `X.Y.Z-beta.N` | `beta` | permanent GitHub prerelease |
| tag `vX.Y.Z` | `X.Y.Z` | `latest` | permanent GitHub release |

For a stable workspace version `X.Y.Z`, the next main build is
`X.Y.(Z+1)-nightly.YYYYMMDD.{short-sha}`. For a prerelease workspace version,
the nightly build keeps the same `X.Y.Z` core and replaces its prerelease part
with `nightly.YYYYMMDD.{short-sha}`. The date is UTC and the commit identifier
makes every main build version unique. For example, workspace version `0.1.0`
produces a version shaped like `0.1.1-nightly.20260719.a1b2c3d`. The short
identifier is the first seven lowercase hexadecimal characters of the Git
commit SHA.

The generated nightly version is embedded in the Rust binary so `colay
--version`, all four npm package versions, the archive name, and release
metadata agree. A beta or stable tag is accepted only when its version exactly
matches the Rust workspace version and the checked-in npm package template
version. A tag outside the two documented patterns is not a release event.

User-facing channel commands are:

```text
npm install --global @kimohy/colay
npm install --global @kimohy/colay@beta
npm install --global @kimohy/colay@nightly
```

Stable and beta versions are prepared explicitly in the repository before the
corresponding tag is created. Nightly version materialization happens only in a
temporary workflow staging directory; a main build never edits or commits
version files.

## Workflow architecture and data flow

A single release workflow handles `push` events on `main` and matching version
tags. Its jobs have these boundaries:

1. **Classify and verify** parses the event, derives the channel/version, rejects
   mismatches, and runs the repository-required format, Clippy, and full test
   commands with fake-provider-only environment controls.
2. **Build native artifacts** runs one job per target on its native runner, builds
   only the production `colay` binary, executes `colay --version`, stages one
   native npm package, and creates one direct-download archive.
3. **Validate packages** downloads all build artifacts, confirms their manifest
   versions and checksums, builds the root npm package with exact optional
   dependencies, runs launcher/package smoke tests, and produces `SHA256SUMS`
   plus `release-manifest.json`.
4. **Attest artifacts** creates GitHub artifact attestations for the native
   archives and checksum file. This job receives only the permissions required
   to create attestations.
5. **Publish npm** publishes the three native packages first through npm Trusted
   Publishing, verifies that all three exact versions are visible in the npm
   registry, then publishes the root package last with the selected npm dist-tag.
   Native packages use the internal `colay-candidate` tag because users install
   only the root package and its dependencies are exact versions. This job
   receives `id-token: write` and no long-lived npm token.
6. **Publish GitHub** uploads beta artifacts to a prerelease and stable artifacts
   to a normal release. A nightly uploads the same validated bundle as a workflow
   artifact with 14-day retention and does not create a permanent GitHub release.

Publishing the root package last prevents users from resolving a release whose
native packages are not yet available. GitHub release publication and npm
publication consume the same immutable validated artifacts rather than
rebuilding them.

Direct-download archives use deterministic names:

```text
colay-v{version}-x86_64-pc-windows-msvc.zip
colay-v{version}-aarch64-apple-darwin.tar.gz
colay-v{version}-x86_64-unknown-linux-musl.tar.gz
```

Every direct-download archive includes the matching binary and a byte-for-byte
copy of the repository root `LICENSE`. Every npm package includes the same
license text next to its manifest.

`release-manifest.json` records the channel, version, source commit, Rust target,
archive name, SHA-256 digest, state/config schema versions, and supported and
recommended Codex versions, plus the `Apache-2.0` license identifier. It contains
no credentials or local paths.

The workflow uses GitHub-hosted actions pinned to immutable commit SHAs. Build
jobs have `contents: read`. npm OIDC, GitHub attestation, and GitHub release
permissions are isolated in their respective jobs and are not available to pull
request jobs or native compilation jobs. Provider API-key variables are cleared
and `COLAY_TEST_FAKE_PROVIDERS_ONLY=1` remains active.

## Publication prerequisites

Repository automation cannot create or authorize the npm identity. Before the
first OIDC publication, the maintainer must own the `@kimohy` npm scope. npm
requires a package to exist before it can receive a trusted-publisher
configuration, so the first validated four-package bundle is a one-time
interactive bootstrap: a maintainer downloads the attested workflow tarballs,
publishes the three native packages with `colay-candidate` and the root package
with its selected channel tag using npm login plus 2FA, then configures each
package to allow `npm publish` only from this repository's `release.yml`.

Each npm package supports one trusted-publisher configuration. The configuration
therefore binds to the workflow filename without an npm environment claim, while
GitHub environments named `npm-nightly`, `npm-beta`, and `npm-stable` provide
independent policy gates inside that workflow; the stable environment should
require human approval.

No npm access token, provider credential, signing key, or GitHub personal access
token is committed to the repository. If the npm scope or trusted-publisher
configuration is absent, the workflow fails at publication while retaining the
validated workflow artifacts for inspection.

## Failure and retry behavior

Classification, version checks, compilation, smoke tests, checksums, and
attestations complete before any public package is published. A failure in those
stages creates no public release.

npm does not permit overwriting a published name/version pair. Rerunning a
partially completed publication therefore checks each exact package version:
matching existing packages are verified and skipped, missing packages are
published, and any mismatched provenance or digest fails closed. The root
package remains last and its `npm publish --tag {channel}` operation atomically
publishes the version and moves the selected channel tag. The workflow never
uses `npm dist-tag add`, which is outside npm OIDC publishing authorization. If
an existing root version has the wrong channel tag, automation fails with an
interactive-maintainer recovery command rather than introducing a registry
token. Consequently, a failure before root publication leaves `nightly`,
`beta`, or `latest` pointing at the last complete release.

Stable GitHub releases and version tags are immutable in this workflow: an
existing release is verified, never overwritten. Beta releases are also not
overwritten. A retry can attach only missing assets whose digests match the
validated bundle. Nightly workflow artifacts are immutable per run and expire
after 14 days.

## Verification

Tests are layered so packaging behavior is exercised without real provider
inference:

- Pure launcher tests cover platform mapping, unsupported targets, missing
  optional dependencies, argument preservation, normal exit codes, spawn
  failures, and supported signal propagation using a local fake executable.
- Version-script tests cover stable-to-nightly patch increments,
  prerelease-to-nightly conversion, UTC formatting, short commit identifiers,
  beta/stable tag classification, malformed tags, and Cargo/npm/tag mismatch
  rejection.
- `npm pack --json --dry-run` output is parsed and compared with an exact
  per-package allowlist. Each native package may contain only its manifest,
  npm-required documentation, and one expected binary; the root package contains
  no native binary. Every package must contain `LICENSE`, whose SHA-256 digest
  must match the repository root license file.
- Cargo metadata validation proves that the workspace and every member crate
  resolve to the `Apache-2.0` SPDX identifier, and npm manifest validation proves
  the same identifier is present in all four packages.
- Each native runner installs the locally packed root and platform package in an
  isolated prefix and runs `colay --version`, proving npm's command shim and the
  Rust binary agree on the generated version.
- Each archive is extracted on its native runner, executed with `--version`, and
  re-hashed before upload.
- Workflow static tests validate channel routing, permissions, immutable action
  pins, artifact names, and the publish order without contacting npm or creating
  a GitHub release.
- The required repository checks remain:

  ```text
  cargo fmt --all -- --check
  cargo clippy --workspace --all-targets --all-features -- -D warnings
  cargo test --workspace --all-features
  ```

All CI provider integration continues to use `orchestrator-test-support` fake
binaries. Release smoke tests invoke only Colay's own `--version`; they never
invoke Codex, Claude, or Gemini.

## Documentation and operator experience

The README displays an Apache-2.0 license badge linked to the root `LICENSE`,
leads with stable npm installation, and shows beta, nightly, and direct-download
alternatives. The release guide documents channel semantics, tag preparation,
required GitHub environments, initial npm Trusted Publishing setup, retry
behavior, checksum and attestation verification, Apache-2.0 licensing, and the
fact that nightly artifacts expire after 14 days.

Every beta and stable GitHub release satisfies the existing release-note
obligations: supported and recommended Codex versions, state/config migration
requirements, known compatibility limitations, and rollback manifest procedure.
The release workflow does not auto-merge, push source changes, or delete a
worktree.
