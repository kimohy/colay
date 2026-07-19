# Multi-OS npm Release Channels Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship Colay through nightly, beta, and stable npm channels with verified native binaries for Windows x64, macOS Apple Silicon, and Linux x64, plus beta/stable GitHub Releases.

**Architecture:** A dependency-free CommonJS launcher selects one exact-version optional native npm package. Node release tooling derives immutable versions, stages and validates packages, and performs retry-safe publication; a least-privilege GitHub Actions workflow builds each Rust target natively and passes the same validated artifacts to npm, attestations, and GitHub Releases.

**Tech Stack:** Rust 1.95/Cargo, Node.js 22 CommonJS and ESM, built-in `node:test`, npm workspaces and Trusted Publishing, GitHub Actions, GitHub CLI, SHA-256, Apache License 2.0.

## Global Constraints

- The public package is `@kimohy/colay`; the installed command is `colay`.
- Support exactly Windows x64 (`x86_64-pc-windows-msvc`), macOS ARM64 (`aarch64-apple-darwin`), and Linux x64 musl (`x86_64-unknown-linux-musl`).
- Support Node.js 22 or newer and use no npm runtime dependencies other than exact-version optional native packages.
- `main` publishes `nightly`; `vX.Y.Z-beta.N` publishes `beta`; `vX.Y.Z` publishes `latest`.
- Nightly versions use `X.Y.Z-nightly.YYYYMMDD.{7-char-sha}` and never modify checked-in versions.
- All Rust crates, npm packages, archives, README metadata, and release metadata use Apache-2.0.
- Do not add lifecycle download scripts, shell-constructed provider commands, external telemetry, provider credentials, code signing, auto-update behavior, or unsupported platform packages.
- All provider tests use `orchestrator-test-support` fake binaries; release smoke tests may invoke only `colay --version`.
- Preserve schema versions, append-only audit semantics, redaction, approval gates, vendor-neutral domain boundaries, and isolated writable worktrees.
- Pin GitHub actions to immutable SHAs; isolate `contents: write`, `attestations: write`, and `id-token: write` to the jobs that require them.
- Do not auto-merge, push source changes, delete worktrees, overwrite npm versions, or overwrite beta/stable GitHub releases.
- Required final verification is `cargo fmt --all -- --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and `cargo test --workspace --all-features` plus `npm test`.

## File map

- `LICENSE`: canonical Apache License 2.0 text detected by GitHub and copied into packages/archives.
- `Cargo.toml` and `crates/*/Cargo.toml`: workspace Apache-2.0 metadata inherited by every crate.
- `package.json`: private dependency-free npm workspace and release-tool test entrypoint.
- `npm/colay`: public launcher package, platform resolution, and launcher tests.
- `npm/colay-{win32-x64,darwin-arm64,linux-x64}`: declarative native package templates; release staging supplies the binaries.
- `scripts/release/metadata.mjs`: event classification and deterministic version derivation.
- `scripts/release/stage.mjs`: package staging, file allowlists, checksums, and release manifest generation.
- `scripts/release/publish.mjs`: integrity-aware native-first npm publication and final dist-tag movement.
- `scripts/release/test/*.test.mjs`: license, metadata, staging, publication, and workflow contracts.
- `crates/orchestrator-cli/src/args.rs`: compile-time nightly version override for Clap.
- `.github/workflows/release.yml`: native builds, validation, smoke tests, attestation, npm publication, and GitHub publication.
- `README.md`, `docs/release.md`, and `docs/testing.md`: user installation, operator setup, channel policy, and local verification.

---

### Task 1: Apache-2.0 metadata and npm package templates

**Files:**
- Create: `LICENSE`
- Create: `package.json`
- Create: `npm/colay/package.json`
- Create: `npm/colay/LICENSE`
- Create: `npm/colay-win32-x64/package.json`
- Create: `npm/colay-win32-x64/LICENSE`
- Create: `npm/colay-darwin-arm64/package.json`
- Create: `npm/colay-darwin-arm64/LICENSE`
- Create: `npm/colay-linux-x64/package.json`
- Create: `npm/colay-linux-x64/LICENSE`
- Create: `scripts/release/test/license-contract.test.mjs`
- Modify: `.gitignore`
- Modify: `Cargo.toml`
- Modify: every `crates/*/Cargo.toml`

**Interfaces:**
- Consumes: workspace version `0.1.0`, repository URL `https://github.com/kimohy/colay`, and the canonical Apache License 2.0 text.
- Produces: four package templates at version `0.1.0`, exact optional dependency names, `npm test`, and inherited Cargo license metadata used by every later task.

- [ ] **Step 1: Write the failing license contract test**

Create `scripts/release/test/license-contract.test.mjs` with repository-root discovery from `import.meta.url`. The test must read the root `LICENSE`, require the exact heading `Apache License\n                           Version 2.0, January 2004`, run `cargo metadata --no-deps --format-version 1`, and assert every workspace package has `license === "Apache-2.0"`. It must also read these four manifests and assert their names, version, license, and public access:

```js
const expected = new Map([
  ["npm/colay/package.json", "@kimohy/colay"],
  ["npm/colay-win32-x64/package.json", "@kimohy/colay-win32-x64"],
  ["npm/colay-darwin-arm64/package.json", "@kimohy/colay-darwin-arm64"],
  ["npm/colay-linux-x64/package.json", "@kimohy/colay-linux-x64"],
]);

for (const [relative, name] of expected) {
  const manifest = JSON.parse(await readFile(join(repoRoot, relative), "utf8"));
  assert.equal(manifest.name, name);
  assert.equal(manifest.version, "0.1.0");
  assert.equal(manifest.license, "Apache-2.0");
  assert.equal(manifest.publishConfig.access, "public");
}
```

For each manifest directory, read its sibling `LICENSE` as bytes and assert `deepEqual` with the root license bytes so source packages cannot drift before staging.

Assert that `@kimohy/colay` has these exact optional dependencies:

```js
assert.deepEqual(rootPackage.optionalDependencies, {
  "@kimohy/colay-darwin-arm64": "0.1.0",
  "@kimohy/colay-linux-x64": "0.1.0",
  "@kimohy/colay-win32-x64": "0.1.0",
});
```

- [ ] **Step 2: Run the contract test and observe the expected failure**

Run: `node --test scripts/release/test/license-contract.test.mjs`

Expected: FAIL with `ENOENT` for the missing root `LICENSE` or package manifest.

- [ ] **Step 3: Add the canonical license and Cargo inheritance**

Create `LICENSE` with the unmodified text from `https://www.apache.org/licenses/LICENSE-2.0.txt`, then copy those exact bytes into `LICENSE` beside each of the four npm manifests. Add this workspace field:

```toml
[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.95"
authors = ["Enterprise Developer Tooling Team"]
license = "Apache-2.0"
```

Add `license.workspace = true` below the existing workspace-inherited package fields in all ten crate manifests, including `crates/orchestrator-cli/Cargo.toml`.

- [ ] **Step 4: Add the private npm workspace and public package templates**

Create root `package.json`:

```json
{
  "name": "colay-release-workspace",
  "private": true,
  "version": "0.1.0",
  "license": "Apache-2.0",
  "workspaces": ["npm/*"],
  "scripts": {
    "test": "node --test scripts/release/test/*.test.mjs"
  },
  "engines": { "node": ">=22" }
}
```

Create `npm/colay/package.json` with this public metadata and no lifecycle scripts:

```json
{
  "name": "@kimohy/colay",
  "version": "0.1.0",
  "description": "Local-first multi-provider coding-agent orchestrator",
  "license": "Apache-2.0",
  "repository": { "type": "git", "url": "git+https://github.com/kimohy/colay.git" },
  "homepage": "https://github.com/kimohy/colay#readme",
  "bugs": { "url": "https://github.com/kimohy/colay/issues" },
  "type": "commonjs",
  "bin": { "colay": "bin/colay.js" },
  "files": ["bin", "lib", "LICENSE"],
  "engines": { "node": ">=22" },
  "optionalDependencies": {
    "@kimohy/colay-darwin-arm64": "0.1.0",
    "@kimohy/colay-linux-x64": "0.1.0",
    "@kimohy/colay-win32-x64": "0.1.0"
  },
  "publishConfig": { "access": "public" }
}
```

Create each native manifest with the same description/repository/homepage/bugs/license/files/publishConfig metadata and its exact selectors:

```json
{ "name": "@kimohy/colay-win32-x64", "os": ["win32"], "cpu": ["x64"], "binary": "bin/colay.exe" }
{ "name": "@kimohy/colay-darwin-arm64", "os": ["darwin"], "cpu": ["arm64"], "binary": "bin/colay" }
{ "name": "@kimohy/colay-linux-x64", "os": ["linux"], "cpu": ["x64"], "libc": ["musl"], "binary": "bin/colay" }
```

The shown `binary` field is package metadata used by the staging script; every native manifest must also contain `"version": "0.1.0"`, `"files": ["bin", "LICENSE"]`, and `"publishConfig": { "access": "public" }`. Do not add registry dependencies or a lockfile: all developer tests use Node built-ins and release packages are packed independently. Add `/dist/` to `.gitignore`.

- [ ] **Step 5: Run metadata verification**

Run: `node --test --test-name-pattern="Apache|package metadata" scripts/release/test/license-contract.test.mjs`

Expected: PASS; Cargo metadata reports all ten workspace members as Apache-2.0 and all four npm templates match version `0.1.0`.

- [ ] **Step 6: Commit the declarative distribution metadata**

```text
git add LICENSE Cargo.toml crates/*/Cargo.toml package.json npm .gitignore scripts/release/test/license-contract.test.mjs
git commit -m "chore: add Apache-2.0 distribution metadata"
```

### Task 2: Dependency-free npm launcher

**Files:**
- Create: `npm/colay/lib/launcher.cjs`
- Create: `npm/colay/bin/colay.js`
- Create: `npm/colay/test/launcher.test.cjs`
- Modify: `package.json`

**Interfaces:**
- Consumes: the three native package names and `binary` paths from Task 1.
- Produces: `platformPackage(platform, arch) -> { packageName, binary }`, `resolveNativeBinary(options) -> string`, and `launchNative(options) -> Promise<{ code, signal }>` for the CLI shim and tests.

- [ ] **Step 1: Write failing platform and error tests**

Create `npm/colay/test/launcher.test.cjs` using `node:test` and `node:assert/strict`. Cover all supported mappings and exact unsupported/missing-package diagnostics:

```js
test("maps the three supported platform pairs", () => {
  assert.deepEqual(platformPackage("win32", "x64"), {
    packageName: "@kimohy/colay-win32-x64",
    binary: "bin/colay.exe",
  });
  assert.equal(platformPackage("darwin", "arm64").packageName, "@kimohy/colay-darwin-arm64");
  assert.equal(platformPackage("linux", "x64").packageName, "@kimohy/colay-linux-x64");
});

test("reports unsupported platforms without downloading", () => {
  assert.throws(
    () => platformPackage("darwin", "x64"),
    /unsupported platform darwin\/x64.*win32\/x64.*darwin\/arm64.*linux\/x64/s,
  );
});

test("explains how to recover a missing optional package", () => {
  assert.throws(
    () => resolveNativeBinary({
      platform: "linux",
      arch: "x64",
      resolvePackage: () => { throw Object.assign(new Error("missing"), { code: "MODULE_NOT_FOUND" }); },
    }),
    /@kimohy\/colay-linux-x64.*npm install --global @kimohy\/colay.*github.com\/kimohy\/colay\/releases/s,
  );
});
```

- [ ] **Step 2: Run the launcher test and observe the expected failure**

Run: `node --test npm/colay/test/launcher.test.cjs`

Expected: FAIL with `MODULE_NOT_FOUND` for `../lib/launcher.cjs`.

- [ ] **Step 3: Implement platform selection and native resolution**

Implement an immutable descriptor map and dependency injection for package resolution:

```js
const PLATFORM_PACKAGES = Object.freeze({
  "win32/x64": Object.freeze({ packageName: "@kimohy/colay-win32-x64", binary: "bin/colay.exe" }),
  "darwin/arm64": Object.freeze({ packageName: "@kimohy/colay-darwin-arm64", binary: "bin/colay" }),
  "linux/x64": Object.freeze({ packageName: "@kimohy/colay-linux-x64", binary: "bin/colay" }),
});

function platformPackage(platform, arch) {
  const descriptor = PLATFORM_PACKAGES[`${platform}/${arch}`];
  if (!descriptor) {
    throw new Error(`unsupported platform ${platform}/${arch}; supported: win32/x64, darwin/arm64, linux/x64`);
  }
  return descriptor;
}

function resolveNativeBinary({ platform = process.platform, arch = process.arch, resolvePackage = require.resolve }) {
  const descriptor = platformPackage(platform, arch);
  try {
    const manifest = resolvePackage(`${descriptor.packageName}/package.json`);
    return require("node:path").join(require("node:path").dirname(manifest), descriptor.binary);
  } catch (error) {
    throw new Error(
      `missing optional package ${descriptor.packageName}; reinstall with npm install --global @kimohy/colay or use https://github.com/kimohy/colay/releases`,
      { cause: error },
    );
  }
}
```

Export the three public functions named in the interface block.

- [ ] **Step 4: Add failing process-behavior tests**

Use a temporary fake native package whose expected binary is a copy of `process.execPath`. Invoke a fixture JavaScript file through that copied binary to prove argument preservation (including spaces and shell metacharacters), inherited exit status `37`, and no shell interpretation. Use an injected EventEmitter child to prove `SIGINT`, `SIGTERM`, and `SIGHUP` are forwarded with `child.kill(signal)` and listeners are removed after exit. Assert a spawn `error` rejects instead of returning success.

Run: `node --test npm/colay/test/launcher.test.cjs`

Expected: FAIL because `launchNative` is not implemented.

- [ ] **Step 5: Implement separated-argument process launch and the bin shim**

Implement `launchNative` with `child_process.spawn(binary, args, { stdio: "inherit", shell: false })`. Return a Promise resolving the child's `{ code, signal }`. Register `SIGINT`, `SIGTERM`, and `SIGHUP` in a small `try` block because Node may reject a signal on a host platform; retain only successfully registered handlers, forward each to `child.kill(signal)`, and remove every retained handler on `error` or `exit`. Never convert arguments to a command string.

Create executable `npm/colay/bin/colay.js`:

```js
#!/usr/bin/env node
"use strict";

const { launchNative } = require("../lib/launcher.cjs");

launchNative({ args: process.argv.slice(2) })
  .then(({ code, signal }) => {
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exitCode = code ?? 1;
  })
  .catch((error) => {
    console.error(`colay: ${error.message}`);
    process.exitCode = 1;
  });
```

Mark the Unix checkout copy executable with `git update-index --chmod=+x npm/colay/bin/colay.js`. Change the root test script to include both suites:

```json
"test": "node --test scripts/release/test/*.test.mjs npm/colay/test/*.test.cjs"
```

- [ ] **Step 6: Verify and commit the launcher**

Run: `npm test`

Expected: PASS with platform, missing dependency, argument, exit, signal, and spawn-error cases; no network request occurs.

```text
git add npm/colay package.json
git commit -m "feat: add native npm launcher"
```

### Task 3: Release classification and embedded build version

**Files:**
- Create: `scripts/release/metadata.mjs`
- Create: `scripts/release/test/metadata.test.mjs`
- Modify: `crates/orchestrator-cli/src/args.rs`

**Interfaces:**
- Consumes: `GITHUB_REF`, 40-character `GITHUB_SHA`, UTC time, workspace `Cargo.toml`, and `npm/colay/package.json`.
- Produces: `classifyRelease({ ref, sha, now, workspaceVersion, templateVersion }) -> { channel, version, distTag, githubMode, retentionDays }`; CLI options `--ref`, `--sha`, `--now`, and `--github-output`; Rust constant `COLAY_VERSION` selected from `COLAY_BUILD_VERSION` or `CARGO_PKG_VERSION`.

- [ ] **Step 1: Write failing channel/version tests**

Create table-driven Node tests with these exact expectations:

```js
assert.deepEqual(classifyRelease({
  ref: "refs/heads/main",
  sha: "a1b2c3d4567890123456789012345678901234567",
  now: new Date("2026-07-19T12:00:00Z"),
  workspaceVersion: "0.1.0",
  templateVersion: "0.1.0",
}), {
  channel: "nightly",
  version: "0.1.1-nightly.20260719.a1b2c3d",
  distTag: "nightly",
  githubMode: "artifact",
  retentionDays: 14,
});

assert.equal(classifyRelease({
  ref: "refs/tags/v0.2.0-beta.3", sha, now, workspaceVersion: "0.2.0-beta.3", templateVersion: "0.2.0-beta.3",
}).distTag, "beta");

assert.equal(classifyRelease({
  ref: "refs/tags/v0.2.0", sha, now, workspaceVersion: "0.2.0", templateVersion: "0.2.0",
}).distTag, "latest");
```

Also assert prerelease workspace `0.2.0-beta.3` on main becomes `0.2.0-nightly.20260719.a1b2c3d`; reject malformed tags, non-main branches, uppercase/non-40-character SHAs, invalid dates, and tag/Cargo/npm mismatches.

- [ ] **Step 2: Run the metadata tests and observe the expected failure**

Run: `node --test scripts/release/test/metadata.test.mjs`

Expected: FAIL with `ERR_MODULE_NOT_FOUND` for `metadata.mjs`.

- [ ] **Step 3: Implement strict SemVer release classification**

Implement only the accepted grammar:

```js
const STABLE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const BETA = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-beta\.(0|[1-9]\d*)$/;
const SHA = /^[0-9a-f]{40}$/;
```

For `refs/heads/main`, parse the stable core, increment patch only when the workspace version has no prerelease, format UTC as eight digits, and use `sha.slice(0, 7)`. For tags, strip the leading `v`, require the version to equal both checked-in versions, and return `githubMode: "prerelease"` for beta or `"release"` for stable with `retentionDays: null`.

The CLI reads the two checked-in versions and accepts `--ref`, `--sha`, and `--now` with defaults from `GITHUB_REF`, `GITHUB_SHA`, and the current UTC clock. Explicit values use the same validation and exist for deterministic local verification; they do not bypass tag/version equality. It prints JSON to stdout and, when `--github-output` is present, appends newline-delimited `channel=`, `version=`, `dist_tag=`, `github_mode=`, and `retention_days=` entries using `appendFile` rather than a shell echo.

- [ ] **Step 4: Write the failing Rust version-selection test**

In `crates/orchestrator-cli/src/args.rs`, import `clap::CommandFactory` in the test module and add:

```rust
#[test]
fn clap_uses_the_selected_build_version() {
    assert_eq!(selected_build_version(Some("0.1.1-nightly.20260719.a1b2c3d")), "0.1.1-nightly.20260719.a1b2c3d");
    assert_eq!(Cli::command().get_version(), Some(COLAY_VERSION));
}
```

Run: `cargo test -p colay clap_uses_the_selected_build_version`

Expected: FAIL because `selected_build_version` and `COLAY_VERSION` do not exist.

- [ ] **Step 5: Implement the compile-time build version override**

Add above `Cli` and replace the bare Clap `version` attribute:

```rust
const fn selected_build_version(override_version: Option<&'static str>) -> &'static str {
    match override_version {
        Some(version) => version,
        None => env!("CARGO_PKG_VERSION"),
    }
}

const COLAY_VERSION: &str = selected_build_version(option_env!("COLAY_BUILD_VERSION"));

#[derive(Clone, Debug, Parser)]
#[command(
    name = "colay",
    version = COLAY_VERSION,
    about = "Local Enterprise multi-provider coding-agent relay",
    long_about = None
)]
```

- [ ] **Step 6: Verify metadata and binary version agreement**

Run in PowerShell:

```powershell
$env:COLAY_BUILD_VERSION='0.1.1-nightly.20260719.a1b2c3d'
cargo run -p colay -- --version
Remove-Item Env:COLAY_BUILD_VERSION
```

Expected stdout: `colay 0.1.1-nightly.20260719.a1b2c3d`.

Run: `npm test; cargo test -p colay clap_uses_the_selected_build_version`

Expected: both commands PASS.

- [ ] **Step 7: Commit release versioning**

```text
git add scripts/release/metadata.mjs scripts/release/test/metadata.test.mjs crates/orchestrator-cli/src/args.rs
git commit -m "feat: classify release channels"
```

### Task 4: Deterministic package staging and release manifest

**Files:**
- Create: `scripts/release/stage.mjs`
- Create: `scripts/release/smoke.mjs`
- Create: `scripts/release/test/stage.test.mjs`
- Create: `scripts/release/test/smoke.test.mjs`

**Interfaces:**
- Consumes: `stageRelease({ repoRoot, outputRoot, channel, version, sourceCommit, binaries, archives })`, where `binaries` maps package names to existing native binary paths and `archives` contains `{ target, name, sha256 }`.
- Produces: `dist/npm/{package-directory}`, `dist/tarballs/*.tgz`, three copied direct-download archives under `dist/release/`, `dist/release/SHA256SUMS`, and `dist/release/release-manifest.json`; exported helpers `sha256File(path)` and `allowedPackageFiles(packageName)`; staging CLI options `--output`, `--channel`, `--version`, `--source-commit`, and three repeated `--native-descriptor` paths; `smokeInstall({ tarballsDir, prefix, packageName, version, run })` plus matching smoke CLI options.

- [ ] **Step 1: Write failing staging tests**

Use `mkdtemp` and three small executable fixture files. Assert staging:

```js
const result = await stageRelease({
  repoRoot,
  outputRoot,
  channel: "nightly",
  version: "0.1.1-nightly.20260719.a1b2c3d",
  sourceCommit: "a1b2c3d4567890123456789012345678901234567",
  binaries: new Map([
    ["@kimohy/colay-win32-x64", winBinary],
    ["@kimohy/colay-darwin-arm64", macBinary],
    ["@kimohy/colay-linux-x64", linuxBinary],
  ]),
  archives: [
    { target: "x86_64-pc-windows-msvc", name: "colay-v0.1.1-nightly.20260719.a1b2c3d-x86_64-pc-windows-msvc.zip", sha256: "1".repeat(64) },
    { target: "aarch64-apple-darwin", name: "colay-v0.1.1-nightly.20260719.a1b2c3d-aarch64-apple-darwin.tar.gz", sha256: "2".repeat(64) },
    { target: "x86_64-unknown-linux-musl", name: "colay-v0.1.1-nightly.20260719.a1b2c3d-x86_64-unknown-linux-musl.tar.gz", sha256: "3".repeat(64) },
  ],
});

assert.equal(result.manifest.license, "Apache-2.0");
assert.equal(result.manifest.state_schema_version, 3);
assert.equal(result.manifest.config_schema_version, 4);
assert.deepEqual(result.manifest.codex.tested_versions, ["0.144.4", "0.144.5"]);
assert.equal(result.manifest.codex.recommended, "0.144.5");
```

Read all staged manifests and assert the generated version is used everywhere and root optional dependencies are exact. Hash all four staged `LICENSE` files and compare them to root. Run `npm pack --json --dry-run` in each staged directory and compare the returned file list to:

```js
const ALLOWLISTS = {
  "@kimohy/colay": ["LICENSE", "bin/colay.js", "lib/launcher.cjs", "package.json"],
  "@kimohy/colay-win32-x64": ["LICENSE", "bin/colay.exe", "package.json"],
  "@kimohy/colay-darwin-arm64": ["LICENSE", "bin/colay", "package.json"],
  "@kimohy/colay-linux-x64": ["LICENSE", "bin/colay", "package.json"],
};
```

- [ ] **Step 2: Run the staging test and observe the expected failure**

Run: `node --test scripts/release/test/stage.test.mjs`

Expected: FAIL with `ERR_MODULE_NOT_FOUND` for `stage.mjs`.

- [ ] **Step 3: Implement staging with explicit file operations**

Use only Node `fs/promises`, `path`, `crypto`, and `child_process.execFile`. Refuse a non-empty output directory, path traversal, missing binaries, wrong 40-character source commits, unknown channels, versions outside the metadata grammar, and archive names that do not exactly match the target/version convention.

Copy package templates into `outputRoot/npm`, rewrite every `version`, rewrite root `optionalDependencies`, copy root `LICENSE`, place only the mapped binary under its declared `binary` path, and set Unix binary mode `0o755`. Verify each source archive digest before copying it under `outputRoot/release`; never regenerate a native archive in the validation job. Invoke npm as separated arguments:

```js
await execFilePromise("npm", ["pack", "--json", "--dry-run"], { cwd: packageDir, shell: false });
await execFilePromise("npm", ["pack", "--json", "--pack-destination", tarballDir], { cwd: packageDir, shell: false });
```

Parse `crates/orchestrator-state/src/migrations.rs`, `crates/orchestrator-state/src/config.rs`, and `compatibility/codex-version.toml` with strict anchored patterns for the current schema/Codex fields. Sort targets and checksum lines lexically. Serialize `release-manifest.json` with two-space indentation and a trailing newline.

Each native build artifact includes `native-descriptor.json` with exactly `package_name`, `target`, `binary_path`, `archive_path`, and `archive_sha256`. The staging CLI accepts exactly three `--native-descriptor PATH` arguments, validates that they form the supported target set without duplicates, resolves their referenced files beneath the descriptor directory, and calls `stageRelease` with the validated maps. It prints the output paths as JSON for the workflow rather than writing shell commands.

- [ ] **Step 4: Write and implement the cross-platform smoke helper**

Create `scripts/release/test/smoke.test.mjs` first. With an injected command runner, assert `smokeInstall` invokes npm with the separated arguments `install`, `--global`, `--offline`, `--ignore-scripts`, `--prefix`, the isolated prefix, root tarball, and the selected native tarball; then invokes the platform-specific npm command shim with `--version` and requires exact stdout `colay ${version}`. Assert an unknown package name, missing/duplicate tarball, nonzero install, nonzero Colay exit, or version mismatch fails with a specific error.

Run: `node --test scripts/release/test/smoke.test.mjs`

Expected: FAIL with `ERR_MODULE_NOT_FOUND` for `smoke.mjs`.

Implement `smokeInstall` with `execFile` and `shell: false`. Its CLI accepts `--tarballs-dir`, `--prefix`, `--package-name`, and `--version`, resolves the tarballs by reading `npm pack --json` metadata saved by `stageRelease`, and chooses `prefix/bin/colay` on Unix or `prefix/colay.cmd` on Windows without searching `PATH`.

Run: `node --test scripts/release/test/smoke.test.mjs`

Expected: PASS with command, path, exit, and exact-version assertions.

- [ ] **Step 5: Add negative staging cases**

Add tests that independently reject an unexpected npm pack file, mismatched license digest, missing target, wrong archive name, invalid source SHA, future/unknown channel, and a non-empty output root. Each assertion must match the specific diagnostic, not just a generic rejection.

Run: `node --test scripts/release/test/stage.test.mjs`

Expected: PASS with positive and fail-closed cases.

- [ ] **Step 6: Commit deterministic staging and smoke support**

```text
git add scripts/release/stage.mjs scripts/release/smoke.mjs scripts/release/test/stage.test.mjs scripts/release/test/smoke.test.mjs
git commit -m "feat: stage verified release packages"
```

### Task 5: Retry-safe npm publication

**Files:**
- Create: `scripts/release/publish.mjs`
- Create: `scripts/release/test/publish.test.mjs`

**Interfaces:**
- Consumes: `publishRelease({ tarballsDir, version, distTag, npmClient })`; `npmClient.viewIntegrity(name, version)`, `npmClient.publish(tarball, tag)`, and `npmClient.viewChannelVersion(name, distTag)`.
- Produces: native-first publication with internal tag `colay-candidate`, root-last publication with the selected user channel, immutable integrity verification, and a fail-closed manual recovery diagnostic if an existing root version is not the current channel target.

- [ ] **Step 1: Write failing publication-order tests**

Use an in-memory fake npm client that records calls. Test a new release and assert the exact order:

```js
assert.deepEqual(calls, [
  ["view", "@kimohy/colay-darwin-arm64", version],
  ["publish", "@kimohy/colay-darwin-arm64", "colay-candidate"],
  ["view", "@kimohy/colay-linux-x64", version],
  ["publish", "@kimohy/colay-linux-x64", "colay-candidate"],
  ["view", "@kimohy/colay-win32-x64", version],
  ["publish", "@kimohy/colay-win32-x64", "colay-candidate"],
  ["view", "@kimohy/colay", version],
  ["publish", "@kimohy/colay", "nightly"],
  ["channel", "@kimohy/colay", "nightly"],
]);
```

The internal `colay-candidate` tag prevents native package publication from mutating `latest`; users never install native packages by tag because root dependencies use exact versions. Add tests proving a matching existing integrity is skipped, a mismatched integrity fails before root publication, a native failure prevents root publication, root publication uses only `nightly`, `beta`, or `latest`, and an existing root version whose public channel points elsewhere fails with an interactive recovery message formatted as ``npm dist-tag add @kimohy/colay@${version} ${distTag}`` instead of executing that command. Use an injected zero-delay retry scheduler to prove registry reads retry at most six times and then report the exact package/version that did not become visible.

- [ ] **Step 2: Run the publication tests and observe the expected failure**

Run: `node --test scripts/release/test/publish.test.mjs`

Expected: FAIL with `ERR_MODULE_NOT_FOUND` for `publish.mjs`.

- [ ] **Step 3: Implement integrity-aware publication**

Read every `npm pack --json` record and retain its `name`, `version`, `filename`, and `integrity`. Implement the real npm client exclusively with `execFile("npm", args, { shell: false })`:

```js
viewIntegrity(name, version) {
  return npmJson(["view", `${name}@${version}`, "dist.integrity", "--json"]);
}

publish(tarball, tag) {
  return npmRun(["publish", tarball, "--access", "public", "--tag", tag]);
}

viewChannelVersion(name, distTag) {
  return npmJson(["view", `${name}@${distTag}`, "version", "--json"]);
}
```

Treat npm's not-found response as absent before publication; propagate authentication, malformed JSON, and all other failures. After a successful publish, retry the public integrity read up to six times with a two-second delay to tolerate registry propagation, then require exact equality. Verify or publish all native packages in lexical order with `colay-candidate`, then verify or publish the root with the selected channel tag. Finally, retry and require the public root channel to resolve to the release version. Never invoke `npm dist-tag`, because npm Trusted Publishing OIDC authorizes `npm publish` but not tag mutation commands.

- [ ] **Step 4: Verify dry local behavior and commit**

Run: `node --test scripts/release/test/publish.test.mjs`

Expected: PASS; the fake client proves order, retry skip, mismatch rejection, and final-tag atomicity without contacting npm.

```text
git add scripts/release/publish.mjs scripts/release/test/publish.test.mjs
git commit -m "feat: publish npm releases safely"
```

### Task 6: Multi-platform GitHub release workflow

**Files:**
- Create: `.github/workflows/release.yml`
- Create: `scripts/release/test/workflow-contract.test.mjs`

**Interfaces:**
- Consumes: Task 3 metadata outputs, Task 4 staging command, Task 5 publishing command, GitHub environments `npm-nightly`, `npm-beta`, and `npm-stable`, and npm Trusted Publishing.
- Produces: native archives, 14-day nightly workflow artifacts, attested beta/stable bundles, npm channel updates, and immutable GitHub prerelease/release assets.

- [ ] **Step 1: Write the failing workflow contract test**

Read `.github/workflows/release.yml` as text and assert all of these exact contracts:

```js
assert.match(workflow, /branches:\s*\[main\]/);
assert.match(workflow, /tags:\s*\["v\*\.\*\.\*"\]/);
assert.match(workflow, /permissions:\s*\n\s*contents: read/);
assert.match(workflow, /windows-2022/);
assert.match(workflow, /macos-14/);
assert.match(workflow, /ubuntu-22\.04/);
assert.match(workflow, /x86_64-pc-windows-msvc/);
assert.match(workflow, /aarch64-apple-darwin/);
assert.match(workflow, /x86_64-unknown-linux-musl/);
assert.match(workflow, /environment:\s*npm-\$\{\{ needs\.classify\.outputs\.channel \}\}/);
assert.match(workflow, /id-token: write/);
assert.match(workflow, /attestations: write/);
assert.match(workflow, /retention-days:\s*\$\{\{ needs\.classify\.outputs\.retention_days \}\}/);
```

Assert every `uses:` value ends in a 40-character lowercase SHA and the workflow contains these reviewed pins:

```text
actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10
actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38
actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02
actions/download-artifact@018cc2cf5baa6db3ef3c5f8a56943fffe632ef53
actions/attest-build-provenance@43d14bc2b83dec42d39ecae14e916627a18bb661
```

Also assert `publish-npm` depends on `smoke` and `attest`, `publish-github` excludes nightly, provider credential variables are empty, and the workflow never contains the invocation patterns `codex exec`, `claude `, or `gemini `, nor the strings `npm_token`, `--clobber`, `git push`, or `gh pr`.

- [ ] **Step 2: Run the workflow test and observe the expected failure**

Run: `node --test scripts/release/test/workflow-contract.test.mjs`

Expected: FAIL with `ENOENT` for `.github/workflows/release.yml`.

- [ ] **Step 3: Add classify, verify, and native build jobs**

Create `release.yml` with this trigger and top-level security boundary:

```yaml
name: Release

on:
  push:
    branches: [main]
    tags: ["v*.*.*"]
  workflow_dispatch:

permissions:
  contents: read

concurrency:
  group: release-${{ github.ref }}
  cancel-in-progress: false

env:
  COLAY_TEST_FAKE_PROVIDERS_ONLY: "1"
  CODEX_API_KEY: ""
  OPENAI_API_KEY: ""
  ANTHROPIC_API_KEY: ""
  GEMINI_API_KEY: ""
```

The `classify` job runs on `ubuntu-22.04`, checks out without persisted credentials, sets up exact Node `22.14.0` with package-manager caching disabled, runs `npm test`, the three required Cargo commands, and `node scripts/release/metadata.mjs --github-output "$GITHUB_OUTPUT"`. Expose `channel`, `version`, `dist_tag`, `github_mode`, and `retention_days` as job outputs. It performs no npm install because the test tooling has no registry dependencies.

The `build` job depends on `classify` and uses this exact matrix:

```yaml
strategy:
  fail-fast: false
  matrix:
    include:
      - os: windows-2022
        target: x86_64-pc-windows-msvc
        executable: colay.exe
        archive_kind: zip
      - os: macos-14
        target: aarch64-apple-darwin
        executable: colay
        archive_kind: tar.gz
      - os: ubuntu-22.04
        target: x86_64-unknown-linux-musl
        executable: colay
        archive_kind: tar.gz
```

Set `COLAY_BUILD_VERSION` from `${{ needs.classify.outputs.version }}`. Install `${{ matrix.target }}` with `rustup target add`; on Linux install `musl-tools` before building. Run `cargo build --locked --release --target "${{ matrix.target }}" --bin colay`, execute the resulting binary with `--version`, and compare stdout exactly with `colay ${{ needs.classify.outputs.version }}` using a cross-platform Node assertion. Stage `LICENSE` and the binary, create the deterministic `.zip` with `Compress-Archive` on Windows or `.tar.gz` with `tar -czf` on Unix, compute the archive SHA-256, and write `native-descriptor.json` with the five fields defined in Task 4. Upload the raw binary, archive, and descriptor together as `native-${{ matrix.target }}`.

- [ ] **Step 4: Add central validation and native npm smoke jobs**

The `validate` job downloads all three `native-*` artifacts and runs the staging CLI with this argument shape:

```text
node scripts/release/stage.mjs \
  --output dist \
  --channel "${{ needs.classify.outputs.channel }}" \
  --version "${{ needs.classify.outputs.version }}" \
  --source-commit "${{ github.sha }}" \
  --native-descriptor artifacts/x86_64-pc-windows-msvc/native-descriptor.json \
  --native-descriptor artifacts/aarch64-apple-darwin/native-descriptor.json \
  --native-descriptor artifacts/x86_64-unknown-linux-musl/native-descriptor.json
```

The CLI runs `npm pack --json --dry-run` validation, creates the four `.tgz` files, copies the three verified native archives, and writes `SHA256SUMS` plus `release-manifest.json`. Upload the complete `dist/release` and `dist/tarballs` trees as one artifact named `release-${version}`. Set `retention-days` to the classified value for nightly and `90` for the workflow transport copy of beta/stable; GitHub Releases remain permanent.

Add a three-runner `smoke` matrix depending on `validate`, with each entry carrying its exact native package name. Each entry downloads the validated bundle and runs:

```text
node scripts/release/smoke.mjs \
  --tarballs-dir dist/tarballs \
  --prefix "${{ runner.temp }}/colay-smoke" \
  --package-name "${{ matrix.package_name }}" \
  --version "${{ needs.classify.outputs.version }}"
```

The helper performs a global-prefix, offline, lifecycle-script-free install of the root and selected native tarballs, executes the generated `colay` command shim with `--version`, and asserts exact version output. This job must not receive npm OIDC or content-write permission.

- [ ] **Step 5: Add attestation and npm publication jobs**

The `attest` job depends on `validate` and `smoke`, downloads the validated artifact, and grants only:

```yaml
permissions:
  contents: read
  id-token: write
  attestations: write
```

Invoke `actions/attest-build-provenance@43d14bc2b83dec42d39ecae14e916627a18bb661` once with `subject-path` containing the three archives, `SHA256SUMS`, `release-manifest.json`, and four npm tarballs.

The `publish-npm` job depends on `classify`, `validate`, `smoke`, and `attest`; uses `environment: npm-${{ needs.classify.outputs.channel }}`; grants `contents: read` and `id-token: write`; sets up exact Node `22.14.0` with registry URL `https://registry.npmjs.org` and package-manager caching disabled; installs exact `npm@11.18.0`; and runs:

```text
node scripts/release/publish.mjs \
  --tarballs-dir dist/tarballs \
  --version "${{ needs.classify.outputs.version }}" \
  --dist-tag "${{ needs.classify.outputs.dist_tag }}"
```

The script supplies `--access public`, so no `NODE_AUTH_TOKEN` or npm token secret is present. npm Trusted Publishing automatically attaches npm provenance.

- [ ] **Step 6: Add immutable beta/stable GitHub publication**

Add `publish-github` depending on `classify`, `validate`, `smoke`, and `attest`, with `if: needs.classify.outputs.github_mode != 'artifact'` and job-only `contents: write`. Use the preinstalled `gh` CLI with `GH_TOKEN: ${{ github.token }}` and separated quoted file arguments.

For a missing release, run `gh release create "$GITHUB_REF_NAME" dist/release/colay-v* dist/release/SHA256SUMS dist/release/release-manifest.json --verify-tag --notes-file dist/release/RELEASE_NOTES.md` and add `--prerelease` only for beta. For an existing release, download its assets into a fresh directory, compare every local and remote SHA-256 digest, fail on any mismatch, and upload only absent assets without `--clobber`. Generate `RELEASE_NOTES.md` containing the supported/recommended Codex versions, state/config migration requirement, known limitations, and rollback manifest procedure from `docs/release.md` and `release-manifest.json`.

- [ ] **Step 7: Verify workflow contracts and YAML shape**

Run: `npm test`

Expected: PASS including workflow channel, runner, action pin, permission, retention, dependency, and forbidden-command contracts.

Run: `git diff --check`

Expected: exit 0 with no whitespace errors.

- [ ] **Step 8: Commit the workflow**

```text
git add .github/workflows/release.yml scripts/release/test/workflow-contract.test.mjs
git commit -m "ci: publish multi-platform release channels"
```

### Task 7: Installation and release operations documentation

**Files:**
- Modify: `README.md`
- Modify: `docs/release.md`
- Modify: `docs/testing.md`

**Interfaces:**
- Consumes: channel/version/package names, GitHub environment names, release files, and retry behavior implemented in Tasks 1-6.
- Produces: stable-first user installation instructions and a complete maintainer runbook with no undocumented publication prerequisite.

- [ ] **Step 1: Write the documentation acceptance checklist before editing**

Record these assertions in `scripts/release/test/license-contract.test.mjs` so they fail against the current docs:

```js
assert.match(readme, /Apache-2\.0.*LICENSE/s);
assert.match(readme, /npm install (?:--global|-g) @kimohy\/colay/);
assert.match(readme, /@kimohy\/colay@beta/);
assert.match(readme, /@kimohy\/colay@nightly/);
assert.match(releaseGuide, /npm-nightly/);
assert.match(releaseGuide, /npm-beta/);
assert.match(releaseGuide, /npm-stable/);
assert.match(releaseGuide, /Trusted Publishing/);
assert.match(releaseGuide, /one-time interactive bootstrap/);
assert.match(releaseGuide, /colay-candidate/);
assert.match(releaseGuide, /npm trust github/);
assert.match(releaseGuide, /vX\.Y\.Z-beta\.N/);
assert.match(releaseGuide, /vX\.Y\.Z/);
assert.match(testingGuide, /npm test/);
```

Run: `node --test scripts/release/test/license-contract.test.mjs`

Expected: FAIL because the current README and release/testing guides do not document npm channels.

- [ ] **Step 2: Update the user installation experience**

At the top of `README.md`, add an Apache-2.0 badge linked to `LICENSE`, a prerequisites line stating Node.js 22+, and stable-first commands:

```text
npm install --global @kimohy/colay
colay --version

npm install --global @kimohy/colay@beta
npm install --global @kimohy/colay@nightly
```

List the three supported OS/CPU pairs and link GitHub Releases as the Node-free beta/stable fallback. State that nightly GitHub workflow artifacts expire after 14 days and npm is the normal nightly access path.

- [ ] **Step 3: Expand the release operator guide**

Update `docs/release.md` with:

- the channel table from the design;
- exact tag patterns and Cargo/npm version equality requirement;
- `@kimohy` scope ownership and all four public package names;
- a one-time interactive bootstrap using exact `npm@11.18.0`, npm login plus 2FA, the three native tarballs with `--tag colay-candidate`, and the root tarball with its selected channel tag;
- after that first publication, these four commands, deliberately without `--environment` because each package supports only one trusted publisher while one workflow uses three GitHub environments:

  ```text
  npm trust github @kimohy/colay --file release.yml --repo kimohy/colay --allow-publish
  npm trust github @kimohy/colay-win32-x64 --file release.yml --repo kimohy/colay --allow-publish
  npm trust github @kimohy/colay-darwin-arm64 --file release.yml --repo kimohy/colay --allow-publish
  npm trust github @kimohy/colay-linux-x64 --file release.yml --repo kimohy/colay --allow-publish
  ```
- npm Trusted Publisher restriction to `.github/workflows/release.yml` and a note that the package must already exist before that restriction can be configured;
- GitHub environments `npm-nightly`, `npm-beta`, `npm-stable`, with required human approval on stable;
- immutable npm/GitHub retry rules and native-first/root-last ordering;
- SHA256SUMS, release-manifest, artifact attestation, and npm provenance verification;
- the existing supported/recommended Codex, migration, limitation, and rollback-note obligations;
- the explicit statement that workflows never invoke provider inference or store provider credentials.

Update `docs/testing.md` with `npm test`, the dependency-free Node test setup, per-runner local tarball smoke behavior, and the continuing fake-provider-only rule.

- [ ] **Step 4: Run documentation and package tests**

Run: `npm test`

Expected: PASS including README/license/release/testing documentation contracts.

- [ ] **Step 5: Run fresh repository-wide verification**

Run each command independently and require exit code 0:

```text
npm test
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace --all-features
python scripts/generate_codex_matrix.py --check
git diff --check
```

Expected: all Node tests pass; Rust format and Clippy produce no findings; all workspace tests pass using fake providers; the compatibility matrix is current; the diff has no whitespace errors.

- [ ] **Step 6: Perform local packaging smoke checks without publishing**

Run the release metadata CLI with explicit `--ref refs/heads/main`, a fixed 40-character `--sha`, and fixed UTC `--now`, build the host `colay` binary with the returned `COLAY_BUILD_VERSION`, and call the staging CLI with three fixture `--native-descriptor` files whose paths stay below one temporary root. Inspect all four `npm pack --json --dry-run` outputs and execute the host launcher tarball with `--version` from an isolated npm prefix.

Expected: generated version, Rust `--version`, four package versions, exact optional dependencies, archive names, `SHA256SUMS`, and `release-manifest.json` all agree; no registry publish, GitHub release, or provider process occurs.

- [ ] **Step 7: Commit documentation and final verification evidence**

```text
git add README.md docs/release.md docs/testing.md scripts/release/test/license-contract.test.mjs
git commit -m "docs: document release installation and operations"
```

After committing, rerun `git status --short` and the seven commands in Step 5. Do not claim completion if any command is stale or nonzero.

## Execution readiness checklist

- Task 1 establishes license/package identities before launcher or publishing logic uses them.
- Task 2 exposes exactly the launcher interfaces consumed by staged root packages.
- Task 3 provides one channel/version contract shared by Rust, npm, filenames, and workflow outputs.
- Task 4 consumes Tasks 1-3 and produces immutable artifacts plus integrity metadata.
- Task 5 consumes Task 4 tarballs and enforces retry-safe publication order.
- Task 6 composes Tasks 3-5 without duplicating their business logic inside YAML.
- Task 7 documents only commands and operations that Tasks 1-6 implement and proves all repository-required checks.
