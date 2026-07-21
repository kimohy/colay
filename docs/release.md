# Release compatibility record

This repository versions Colay and its persisted/public contracts independently.

| Component | Current value | Authority |
|---|---:|---|
| Colay | `0.1.0` | workspace `Cargo.toml` |
| Tested Codex | `0.144.5`, `0.144.6` | `compatibility/codex-version.toml` |
| Recommended Codex | `0.144.6` | `compatibility/codex-version.toml` |
| SQLite state schema | `7` | `STATE_SCHEMA_VERSION` and migrations |
| Config schema | `4` | `CONFIG_SCHEMA_VERSION` |
| Checkpoint/handover schema | `1` | domain writers |

## Release notes obligations

Every Colay release must state the supported/recommended Codex versions, state/config migration requirement, known compatibility limitations, and rollback manifest procedure. A production build must use an exact release artifact/revision, never upstream `main`.

Current known limitations:

- The CLI intentionally keeps `codex exec --json` as the default transport; selecting App Server first is an adapter policy rather than a user-facing CLI switch.
- Authoritative checkpoint diffs remain sensitive local source artifacts even though persistence/handover preflight blocks known secret patterns and oversized unscanned files.
- The daemon executes dependency-ready approved tasks concurrently within exact global/provider and write-scope limits. Read-only reviewer fan-out is not implemented.
- Task results remain isolated and retained after verification. Integration, merge, push, publication, cleanup, and `/retry` remain unavailable until Phase 5.

See [`rollback.md`](rollback.md) for the release manifest, explicit approval, recovery journal, and restart procedure.

## Distribution channels

Colay publishes four public npm packages under the `@kimohy` scope:
`@kimohy/colay`, `@kimohy/colay-win32-x64`,
`@kimohy/colay-darwin-arm64`, and `@kimohy/colay-linux-x64`. All four packages
in one release use the same immutable version. The root package is published
last, after the three native packages, so its exact optional dependencies are
already available.

| Source event | Required version | npm dist-tag | GitHub delivery |
|---|---|---|---|
| push to `main` | generated `X.Y.Z-nightly.YYYYMMDD.{7-char-sha}` | `nightly` | workflow artifact retained 14 days |
| tag `vX.Y.Z-beta.N` | `X.Y.Z-beta.N` | `beta` | permanent prerelease |
| tag `vX.Y.Z` | `X.Y.Z` | `latest` | permanent release |

For a stable workspace version, the nightly increments its patch before adding
the nightly prerelease segment. For a prerelease workspace version, the
nightly retains its `X.Y.Z` core and replaces the prerelease segment. The
workflow rejects any other tag. Before making a beta or stable tag, prepare the
same version in `Cargo.toml` and every checked-in npm template: the Cargo,
npm, and tag versions must be exactly equal. A nightly is materialized only in
workflow staging and never changes checked-in version files.

The short SHA normally uses its first seven lowercase hexadecimal characters.
When those seven characters are all decimal digits, the workflow prefixes the
identifier with `g` (for example, `g0123456`) so every commit-derived component
is non-numeric and never creates a leading-zero numeric SemVer identifier.

## One-time npm bootstrap and Trusted Publishing

The `@kimohy` scope owner must first make all four packages public. npm
requires a package to exist before a trusted publisher can be configured, so
the first attested, validated bundle needs a **one-time interactive bootstrap**.
Use Node.js 22.14.0 or newer and the exact npm client used by the workflow:

```text
npm install --global npm@11.18.0
npm login
# Complete the interactive 2FA challenge.
npm publish dist/tarballs/kimohy-colay-darwin-arm64-<version>.tgz --access public --tag colay-candidate
npm publish dist/tarballs/kimohy-colay-linux-x64-<version>.tgz --access public --tag colay-candidate
npm publish dist/tarballs/kimohy-colay-win32-x64-<version>.tgz --access public --tag colay-candidate
npm publish dist/tarballs/kimohy-colay-<version>.tgz --access public --tag <nightly|beta|latest>
```

Use the exact `filename` entries in `dist/tarballs/npm-pack.json` rather than
constructing scoped package paths. Verify the four tarballs and their release
files before this bootstrap; never publish a reconstructed bundle. After that
first publication, configure npm Trusted Publishing for the sole release
workflow. Each package supports one trusted publisher, so these commands
intentionally omit `--environment`: the workflow name is the stable identity
while GitHub environments provide the per-channel gate.

```text
npm trust github @kimohy/colay --file release.yml --repo kimohy/colay --allow-publish
npm trust github @kimohy/colay-win32-x64 --file release.yml --repo kimohy/colay --allow-publish
npm trust github @kimohy/colay-darwin-arm64 --file release.yml --repo kimohy/colay --allow-publish
npm trust github @kimohy/colay-linux-x64 --file release.yml --repo kimohy/colay --allow-publish
```

Those publisher records restrict `npm publish` to
`.github/workflows/release.yml`. Configure GitHub environments `npm-nightly`,
`npm-beta`, and `npm-stable`; require human approval for `npm-stable`. The
workflow uses OIDC and does not store an npm token, provider credential,
signing key, or personal access token in the repository.

## Verify a published bundle

Download the beta or stable release assets into `dist/release`, then verify the
published bytes and their GitHub attestation before installing anything:

```text
(cd dist/release && sha256sum --check SHA256SUMS)
gh attestation verify <archive> --repo kimohy/colay
```

Use a disposable npm project to verify the package provenance associated with
the exact published version:

```text
verify_dir="$(mktemp -d)"
cd "$verify_dir"
npm init --yes
npm install @kimohy/colay@<version>
npm audit signatures --json --include-attestations
```

npm Trusted Publishing automatically creates provenance for workflow-published
packages. The audit command validates those npm attestations; it does not
replace checking the release archive and GitHub attestation above.

## Operator release procedure

1. Prepare a beta or stable workspace/npm version, run the required checks,
   and create only `vX.Y.Z-beta.N` or `vX.Y.Z`. Push to `main` is the only
   nightly source event.
2. Let `.github/workflows/release.yml` classify the event, build each native
   target, stage one immutable bundle, and run its platform-local tarball
   smoke test. It never runs Codex, Claude, or Gemini inference and provider
   credentials are cleared; release smoke invokes only `colay --version`.
3. Inspect `SHA256SUMS`, `release-manifest.json`, GitHub artifact attestations,
   and npm provenance before accepting the release. The manifest carries the
   source commit, package version, targets, archive digests, schema versions,
   Codex compatibility authority, and Apache-2.0 license identity.
4. The npm job first verifies or publishes the three native tarballs under the
   non-user-facing `colay-candidate` tag, verifies their immutable integrity,
   then publishes the root tarball with `nightly`, `beta`, or `latest`. npm OIDC
   permits publishing but not arbitrary `npm dist-tag add` operations.
5. For beta and stable, reconcile an immutable GitHub prerelease/release from
   the same bundle. Nightly creates no permanent GitHub Release and its workflow
   artifact expires after 14 days.

## Retries, recovery, and release notes

npm package versions and beta/stable GitHub releases are immutable. On retry,
the workflow checks an existing package's registry integrity and skips it only
when it matches the validated tarball; a mismatch fails closed. Existing
GitHub release metadata and assets are verified, never overwritten, and only
missing digest-matching assets may be attached. A failure before the root
publish leaves the selected public channel pointing at the prior complete
release. If an already-published root version has the wrong channel tag, the
workflow stops with the explicit interactive-maintainer `npm dist-tag add`
recovery command instead of adding a long-lived registry token.

Every beta and stable release note must retain the compatibility record above:
supported and recommended Codex versions, state/config migration requirements,
known compatibility limitations, and the rollback-manifest procedure. Use the
validated `release-manifest.json` with [`rollback.md`](rollback.md); do not
infer a rollback plan from an upstream branch.
