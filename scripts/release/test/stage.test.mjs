import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdtemp, mkdir, cp, readFile, writeFile, chmod, readdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import { promisify } from "node:util";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  allowedPackageFiles,
  loadNativeDescriptors,
  sha256File,
  stageRelease,
} from "../stage.mjs";

const execFilePromise = promisify(execFile);
const sourceRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const version = "0.1.1-nightly.20260719.a1b2c3d";
const sourceCommit = "a1b2c3d456789012345678901234567890123456";

const packageDirectories = new Map([
  ["@kimohy/colay", "colay"],
  ["@kimohy/colay-darwin-arm64", "colay-darwin-arm64"],
  ["@kimohy/colay-linux-x64", "colay-linux-x64"],
  ["@kimohy/colay-win32-x64", "colay-win32-x64"],
]);

async function fixture() {
  const root = await mkdtemp(join(tmpdir(), "colay-stage-"));
  const repoRoot = join(root, "repo");
  const outputRoot = join(root, "dist");
  await mkdir(repoRoot, { recursive: true });
  await cp(join(sourceRoot, "LICENSE"), join(repoRoot, "LICENSE"));
  await cp(join(sourceRoot, "npm"), join(repoRoot, "npm"), { recursive: true });
  for (const relative of [
    "crates/orchestrator-state/src/migrations.rs",
    "crates/orchestrator-state/src/config.rs",
    "compatibility/codex-version.toml",
  ]) {
    await mkdir(dirname(join(repoRoot, relative)), { recursive: true });
    await cp(join(sourceRoot, relative), join(repoRoot, relative));
  }

  const nativeRoot = join(root, "native");
  await mkdir(nativeRoot, { recursive: true });
  const binaries = new Map();
  const archives = [];
  const specs = [
    ["@kimohy/colay-win32-x64", "x86_64-pc-windows-msvc", "colay.exe", ".zip"],
    ["@kimohy/colay-darwin-arm64", "aarch64-apple-darwin", "colay", ".tar.gz"],
    ["@kimohy/colay-linux-x64", "x86_64-unknown-linux-musl", "colay", ".tar.gz"],
  ];
  for (const [packageName, target, executable, extension] of specs) {
    const targetRoot = join(nativeRoot, target);
    await mkdir(targetRoot, { recursive: true });
    const binaryPath = join(targetRoot, executable);
    await writeFile(binaryPath, `fixture binary for ${target}\n`);
    await chmod(binaryPath, 0o755);
    binaries.set(packageName, binaryPath);

    const name = `colay-v${version}-${target}${extension}`;
    const archivePath = join(targetRoot, name);
    await writeFile(archivePath, `fixture archive for ${target}\n`);
    archives.push({ target, name, path: archivePath, sha256: await sha256File(archivePath) });
  }
  return { root, repoRoot, outputRoot, binaries, archives };
}

async function stage(overrides = {}) {
  const values = await fixture();
  const result = await stageRelease({
    repoRoot: values.repoRoot,
    outputRoot: values.outputRoot,
    channel: "nightly",
    version,
    sourceCommit,
    binaries: values.binaries,
    archives: values.archives,
    ...overrides,
  });
  return { ...values, result };
}

test("stages exact packages, archives, checksums, and authoritative release metadata", async () => {
  const { repoRoot, outputRoot, result } = await stage();
  assert.equal(result.manifest.license, "Apache-2.0");
  assert.equal(result.manifest.state_schema_version, 3);
  assert.equal(result.manifest.config_schema_version, 4);
  assert.deepEqual(result.manifest.codex.tested_versions, ["0.144.4", "0.144.5"]);
  assert.equal(result.manifest.codex.recommended, "0.144.5");

  const rootLicenseDigest = await sha256File(join(repoRoot, "LICENSE"));
  for (const [packageName, directory] of packageDirectories) {
    const packageRoot = join(outputRoot, "npm", directory);
    const manifest = JSON.parse(await readFile(join(packageRoot, "package.json"), "utf8"));
    assert.equal(manifest.version, version);
    assert.equal(await sha256File(join(packageRoot, "LICENSE")), rootLicenseDigest);
    if (packageName === "@kimohy/colay") {
      assert.deepEqual(manifest.optionalDependencies, {
        "@kimohy/colay-darwin-arm64": version,
        "@kimohy/colay-linux-x64": version,
        "@kimohy/colay-win32-x64": version,
      });
    }
    const npmInvocation = process.platform === "win32"
      ? [process.execPath, [join(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js"), "pack", "--json", "--dry-run"]]
      : ["npm", ["pack", "--json", "--dry-run"]];
    const { stdout } = await execFilePromise(...npmInvocation, {
      cwd: packageRoot,
      shell: false,
    });
    const [record] = JSON.parse(stdout);
    assert.deepEqual(
      record.files.map(({ path }) => path).sort(),
      allowedPackageFiles(packageName),
    );
  }

  const releaseFiles = (await readdir(join(outputRoot, "release"))).sort();
  assert.deepEqual(releaseFiles, [
    "SHA256SUMS",
    `colay-v${version}-aarch64-apple-darwin.tar.gz`,
    `colay-v${version}-x86_64-pc-windows-msvc.zip`,
    `colay-v${version}-x86_64-unknown-linux-musl.tar.gz`,
    "release-manifest.json",
  ]);
  const checksumLines = (await readFile(join(outputRoot, "release", "SHA256SUMS"), "utf8"))
    .trimEnd()
    .split("\n");
  assert.deepEqual(checksumLines, [...checksumLines].sort());
  assert.equal(result.packages.length, 4);
  assert.equal(
    (await readFile(join(outputRoot, "release", "release-manifest.json"), "utf8")).endsWith("\n"),
    true,
  );
});

test("rejects fail-closed staging inputs with specific diagnostics", async (t) => {
  await t.test("invalid source commit", async () => {
    const values = await fixture();
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit: "ABC", binaries: values.binaries, archives: values.archives }),
      /source commit must be exactly 40 lowercase hexadecimal characters/,
    );
  });
  await t.test("unknown channel", async () => {
    const values = await fixture();
    await assert.rejects(
      stageRelease({ ...values, channel: "future", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /unsupported release channel "future"/,
    );
  });
  await t.test("non-empty output", async () => {
    const values = await fixture();
    await mkdir(values.outputRoot, { recursive: true });
    await writeFile(join(values.outputRoot, "keep"), "do not overwrite");
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /output directory must be empty/,
    );
  });
  await t.test("missing target", async () => {
    const values = await fixture();
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives.slice(1) }),
      /missing archive target x86_64-pc-windows-msvc/,
    );
  });
  await t.test("unknown archive target", async () => {
    const values = await fixture();
    values.archives.push({
      target: "aarch64-unknown-freebsd",
      name: "colay-v0.1.1-nightly.20260719.a1b2c3d-aarch64-unknown-freebsd.tar.gz",
      path: values.archives[0].path,
      sha256: values.archives[0].sha256,
    });
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /unsupported archive target aarch64-unknown-freebsd/,
    );
  });
  await t.test("wrong archive name", async () => {
    const values = await fixture();
    values.archives[0].name = "colay-wrong.zip";
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /archive name for x86_64-pc-windows-msvc must be/,
    );
  });
  await t.test("archive digest mismatch", async () => {
    const values = await fixture();
    values.archives[0].sha256 = "0".repeat(64);
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /archive digest mismatch for x86_64-pc-windows-msvc/,
    );
  });
  await t.test("template license mismatch", async () => {
    const values = await fixture();
    await writeFile(join(values.repoRoot, "npm/colay-linux-x64/LICENSE"), "wrong license\n");
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /license digest mismatch for @kimohy\/colay-linux-x64/,
    );
  });
  await t.test("root runtime dependency", async () => {
    const values = await fixture();
    const manifestPath = join(values.repoRoot, "npm/colay/package.json");
    const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
    manifest.dependencies = { surprise: "1.0.0" };
    await writeFile(manifestPath, `${JSON.stringify(manifest)}\n`);
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /root package must not declare runtime dependencies/,
    );
  });
  await t.test("unexpected npm pack file", async () => {
    const values = await fixture();
    await writeFile(join(values.repoRoot, "npm/colay/lib/unexpected.cjs"), "module.exports = {};\n");
    await assert.rejects(
      stageRelease({ ...values, channel: "nightly", version, sourceCommit, binaries: values.binaries, archives: values.archives }),
      /unexpected package files for @kimohy\/colay: lib\/unexpected\.cjs/,
    );
  });
});

test("native descriptors reject referenced paths outside their directory", async () => {
  const root = await mkdtemp(join(tmpdir(), "colay-descriptor-"));
  const descriptorRoot = join(root, "descriptor");
  await mkdir(descriptorRoot);
  const outside = join(root, "outside.exe");
  await writeFile(outside, "outside");
  const descriptor = join(descriptorRoot, "native-descriptor.json");
  await writeFile(descriptor, JSON.stringify({
    package_name: "@kimohy/colay-win32-x64",
    target: "x86_64-pc-windows-msvc",
    binary_path: "../outside.exe",
    archive_path: "../outside.zip",
    archive_sha256: "0".repeat(64),
  }));
  await assert.rejects(loadNativeDescriptors([descriptor]), /binary_path must stay beneath descriptor directory/);
});
