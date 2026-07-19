import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import {
  access,
  chmod,
  copyFile,
  cp,
  lstat,
  mkdir,
  readFile,
  readdir,
  realpath,
  writeFile,
} from "node:fs/promises";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execFilePromise = promisify(execFile);
const REPOSITORY_ROOT = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const SHA256 = /^[0-9a-f]{64}$/;
const SOURCE_COMMIT = /^[0-9a-f]{40}$/;
const STABLE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const BETA = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-beta\.(0|[1-9]\d*)$/;
const NIGHTLY = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-nightly\.\d{8}\.[0-9a-f]{7}$/;

const PACKAGES = Object.freeze([
  Object.freeze({ name: "@kimohy/colay", directory: "colay", binary: null, target: null }),
  Object.freeze({ name: "@kimohy/colay-win32-x64", directory: "colay-win32-x64", binary: "bin/colay.exe", target: "x86_64-pc-windows-msvc" }),
  Object.freeze({ name: "@kimohy/colay-darwin-arm64", directory: "colay-darwin-arm64", binary: "bin/colay", target: "aarch64-apple-darwin" }),
  Object.freeze({ name: "@kimohy/colay-linux-x64", directory: "colay-linux-x64", binary: "bin/colay", target: "x86_64-unknown-linux-musl" }),
]);
const NATIVE_PACKAGES = PACKAGES.filter(({ target }) => target !== null);
const PACKAGE_BY_NAME = new Map(PACKAGES.map((item) => [item.name, item]));
const TARGET_BY_NAME = new Map(NATIVE_PACKAGES.map((item) => [item.target, item]));
const ALLOWLISTS = Object.freeze({
  "@kimohy/colay": Object.freeze(["LICENSE", "bin/colay.js", "lib/launcher.cjs", "package.json"]),
  "@kimohy/colay-win32-x64": Object.freeze(["LICENSE", "bin/colay.exe", "package.json"]),
  "@kimohy/colay-darwin-arm64": Object.freeze(["LICENSE", "bin/colay", "package.json"]),
  "@kimohy/colay-linux-x64": Object.freeze(["LICENSE", "bin/colay", "package.json"]),
});
const NPM_CLI = join(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");

function npmInvocation(args) {
  return process.platform === "win32"
    ? { command: process.execPath, args: [NPM_CLI, ...args] }
    : { command: "npm", args };
}

function fail(message) {
  throw new Error(`Invalid release staging: ${message}`);
}

function matchesChannelVersion(channel, version) {
  return (channel === "nightly" && NIGHTLY.test(version))
    || (channel === "beta" && BETA.test(version))
    || (channel === "stable" && STABLE.test(version));
}

function requireContained(base, candidate, label) {
  if (isAbsolute(candidate)) fail(`${label} must be relative`);
  const resolvedBase = resolve(base);
  const resolvedCandidate = resolve(resolvedBase, candidate);
  const rel = relative(resolvedBase, resolvedCandidate);
  if (rel === "" || rel.startsWith("..") || isAbsolute(rel)) {
    fail(`${label} must stay beneath descriptor directory`);
  }
  return resolvedCandidate;
}

async function requireRegularFile(path, label) {
  let stat;
  try {
    stat = await lstat(path);
  } catch {
    fail(`${label} does not exist: ${path}`);
  }
  if (!stat.isFile()) fail(`${label} must be a regular file: ${path}`);
}

function packageForName(name) {
  const descriptor = PACKAGE_BY_NAME.get(name);
  if (!descriptor) fail(`unsupported package ${JSON.stringify(name)}`);
  return descriptor;
}

export function allowedPackageFiles(packageName) {
  const files = ALLOWLISTS[packageName];
  if (!files) fail(`unsupported package ${JSON.stringify(packageName)}`);
  return [...files].sort();
}

export async function sha256File(path) {
  const contents = await readFile(path);
  return createHash("sha256").update(contents).digest("hex");
}

async function readSchemaVersions(repoRoot) {
  const [migrations, config] = await Promise.all([
    readFile(join(repoRoot, "crates/orchestrator-state/src/migrations.rs"), "utf8"),
    readFile(join(repoRoot, "crates/orchestrator-state/src/config.rs"), "utf8"),
  ]);
  const state = /^pub const STATE_SCHEMA_VERSION: u32 = (\d+);$/m.exec(migrations)?.[1];
  const configVersion = /^pub const CONFIG_SCHEMA_VERSION: u32 = (\d+);$/m.exec(config)?.[1];
  if (!state || !configVersion) fail("could not read current state/config schema versions");
  return { stateSchemaVersion: Number.parseInt(state, 10), configSchemaVersion: Number.parseInt(configVersion, 10) };
}

async function readCodexAuthority(repoRoot) {
  const text = (await readFile(join(repoRoot, "compatibility/codex-version.toml"), "utf8")).replaceAll("\r\n", "\n");
  const supportedMin = /^supported_min = "([0-9]+\.[0-9]+\.[0-9]+)"$/m.exec(text)?.[1];
  const tested = /^tested_versions = \[([^\]]*)\]$/m.exec(text)?.[1];
  const recommended = /^recommended = "([0-9]+\.[0-9]+\.[0-9]+)"$/m.exec(text)?.[1];
  const pinnedRevision = /^pinned_revision = "([0-9a-f]{40})"$/m.exec(text)?.[1];
  if (!supportedMin || tested === undefined || !recommended || !pinnedRevision) {
    fail("could not read Codex compatibility authority");
  }
  const testedVersions = [...tested.matchAll(/"([0-9]+\.[0-9]+\.[0-9]+)"/g)].map((match) => match[1]);
  if (testedVersions.length === 0 || tested.replace(/"[0-9]+\.[0-9]+\.[0-9]+"|,|\s/g, "") !== "") {
    fail("Codex tested_versions must be a non-empty strict version list");
  }
  return { supported_min: supportedMin, tested_versions: testedVersions, recommended, pinned_revision: pinnedRevision };
}

async function verifyTemplateLicenses(repoRoot) {
  const rootLicense = join(repoRoot, "LICENSE");
  const rootDigest = await sha256File(rootLicense);
  for (const descriptor of PACKAGES) {
    const digest = await sha256File(join(repoRoot, "npm", descriptor.directory, "LICENSE"));
    if (digest !== rootDigest) fail(`license digest mismatch for ${descriptor.name}`);
  }
  return rootLicense;
}

async function assertOutputEmpty(outputRoot) {
  try {
    const stat = await lstat(outputRoot);
    if (!stat.isDirectory()) fail(`output path must be a directory: ${outputRoot}`);
    if ((await readdir(outputRoot)).length !== 0) fail("output directory must be empty");
  } catch (error) {
    if (error?.code !== "ENOENT") throw error;
    await mkdir(outputRoot, { recursive: true });
  }
}

function validateInputs({ channel, version, sourceCommit, binaries, archives }) {
  if (!Object.hasOwn({ nightly: true, beta: true, stable: true }, channel)) {
    fail(`unsupported release channel ${JSON.stringify(channel)}`);
  }
  if (typeof version !== "string" || !matchesChannelVersion(channel, version)) {
    fail(`version ${JSON.stringify(version)} is invalid for ${channel}`);
  }
  if (typeof sourceCommit !== "string" || !SOURCE_COMMIT.test(sourceCommit)) {
    fail("source commit must be exactly 40 lowercase hexadecimal characters");
  }
  if (!(binaries instanceof Map) || binaries.size !== NATIVE_PACKAGES.length) {
    fail("binaries must map exactly the three supported native packages");
  }
  for (const descriptor of NATIVE_PACKAGES) {
    if (typeof binaries.get(descriptor.name) !== "string") fail(`missing binary for ${descriptor.name}`);
  }
  if (!Array.isArray(archives)) fail("archives must be an array");
}

async function validateArchives(version, archives) {
  const byTarget = new Map();
  for (const archive of archives) {
    if (!archive || typeof archive !== "object" || typeof archive.target !== "string") fail("archive descriptor is invalid");
    if (!TARGET_BY_NAME.has(archive.target)) fail(`unsupported archive target ${archive.target}`);
    if (byTarget.has(archive.target)) fail(`duplicate archive target ${archive.target}`);
    byTarget.set(archive.target, archive);
  }
  const validated = [];
  for (const [target, packageDescriptor] of TARGET_BY_NAME) {
    const archive = byTarget.get(target);
    if (!archive) fail(`missing archive target ${target}`);
    const expectedName = `colay-v${version}-${target}${target === "x86_64-pc-windows-msvc" ? ".zip" : ".tar.gz"}`;
    if (archive.name !== expectedName) fail(`archive name for ${target} must be ${expectedName}`);
    if (typeof archive.path !== "string") fail(`archive path for ${target} is required`);
    if (typeof archive.sha256 !== "string" || !SHA256.test(archive.sha256)) fail(`archive SHA-256 for ${target} must be lowercase hexadecimal`);
    await requireRegularFile(archive.path, `archive for ${target}`);
    if ((await sha256File(archive.path)) !== archive.sha256) fail(`archive digest mismatch for ${target}`);
    validated.push({ target, name: archive.name, path: archive.path, sha256: archive.sha256, packageName: packageDescriptor.name });
  }
  return validated.sort((left, right) => left.target.localeCompare(right.target));
}

async function copyAndRewritePackage({ repoRoot, npmRoot, rootLicense, descriptor, version, binaries }) {
  const source = join(repoRoot, "npm", descriptor.directory);
  const destination = join(npmRoot, descriptor.directory);
  await cp(source, destination, { recursive: true, force: false, errorOnExist: true });
  const manifestPath = join(destination, "package.json");
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  if (manifest.name !== descriptor.name || (descriptor.binary !== null && manifest.binary !== descriptor.binary)) {
    fail(`template metadata does not match ${descriptor.name}`);
  }
  if (manifest.license !== "Apache-2.0") fail(`template license must be Apache-2.0 for ${descriptor.name}`);
  if (manifest.dependencies && Object.keys(manifest.dependencies).length !== 0) {
    fail(`${descriptor.name === "@kimohy/colay" ? "root package" : `native package ${descriptor.name}`} must not declare runtime dependencies`);
  }
  if (descriptor.binary !== null && manifest.optionalDependencies && Object.keys(manifest.optionalDependencies).length !== 0) {
    fail(`native package ${descriptor.name} must not declare optional dependencies`);
  }
  manifest.version = version;
  if (descriptor.name === "@kimohy/colay") {
    manifest.optionalDependencies = Object.fromEntries(NATIVE_PACKAGES.map((item) => [item.name, version]));
  }
  await writeFile(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  await copyFile(rootLicense, join(destination, "LICENSE"));
  if (descriptor.binary) {
    const binaryPath = join(destination, ...descriptor.binary.split("/"));
    await mkdir(dirname(binaryPath), { recursive: true });
    await copyFile(binaries.get(descriptor.name), binaryPath);
    await chmod(binaryPath, 0o755);
  }
  return destination;
}

async function packAndVerify(packageName, packageDir, tarballsDir) {
  const dryRunInvocation = npmInvocation(["pack", "--json", "--dry-run"]);
  const dryRun = await execFilePromise(dryRunInvocation.command, dryRunInvocation.args, { cwd: packageDir, shell: false });
  let record;
  try {
    [record] = JSON.parse(dryRun.stdout);
  } catch {
    fail(`npm pack did not return JSON for ${packageName}`);
  }
  const actual = (record?.files ?? []).map(({ path }) => path).sort();
  const expected = allowedPackageFiles(packageName);
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    const unexpected = actual.filter((file) => !expected.includes(file));
    const missing = expected.filter((file) => !actual.includes(file));
    fail(`unexpected package files for ${packageName}: ${[...unexpected, ...missing.map((file) => `missing ${file}`)].join(", ")}`);
  }
  const packedInvocation = npmInvocation(["pack", "--json", "--pack-destination", tarballsDir]);
  const packed = await execFilePromise(packedInvocation.command, packedInvocation.args, { cwd: packageDir, shell: false });
  let packedRecord;
  try {
    [packedRecord] = JSON.parse(packed.stdout);
  } catch {
    fail(`npm pack did not return package metadata for ${packageName}`);
  }
  if (!packedRecord || packedRecord.name !== packageName || typeof packedRecord.filename !== "string") {
    fail(`npm pack returned invalid metadata for ${packageName}`);
  }
  const filename = packedRecord.filename;
  if (filename !== filename.split(/[\\/]/).pop()) fail(`npm pack returned unsafe filename for ${packageName}`);
  await requireRegularFile(join(tarballsDir, filename), `tarball for ${packageName}`);
  return packedRecord;
}

export async function stageRelease({ repoRoot, outputRoot, channel, version, sourceCommit, binaries, archives }) {
  if (typeof repoRoot !== "string" || typeof outputRoot !== "string") fail("repoRoot and outputRoot are required paths");
  validateInputs({ channel, version, sourceCommit, binaries, archives });
  await assertOutputEmpty(outputRoot);
  const rootLicense = await verifyTemplateLicenses(repoRoot);
  const validatedArchives = await validateArchives(version, archives);
  const { stateSchemaVersion, configSchemaVersion } = await readSchemaVersions(repoRoot);
  const codex = await readCodexAuthority(repoRoot);
  const npmRoot = join(outputRoot, "npm");
  const tarballsDir = join(outputRoot, "tarballs");
  const releaseDir = join(outputRoot, "release");
  await Promise.all([mkdir(npmRoot), mkdir(tarballsDir), mkdir(releaseDir)]);

  const packageDirs = new Map();
  for (const descriptor of PACKAGES) {
    packageDirs.set(descriptor.name, await copyAndRewritePackage({ repoRoot, npmRoot, rootLicense, descriptor, version, binaries }));
  }
  const records = [];
  for (const descriptor of PACKAGES) records.push(await packAndVerify(descriptor.name, packageDirs.get(descriptor.name), tarballsDir));
  await writeFile(join(tarballsDir, "npm-pack.json"), `${JSON.stringify(records, null, 2)}\n`, "utf8");

  for (const archive of validatedArchives) await copyFile(archive.path, join(releaseDir, archive.name));
  const checksumLines = validatedArchives
    .map(({ name, sha256 }) => `${sha256}  ${name}`)
    .sort((left, right) => left.localeCompare(right));
  await writeFile(join(releaseDir, "SHA256SUMS"), `${checksumLines.join("\n")}\n`, "utf8");
  const manifest = {
    channel,
    version,
    source_commit: sourceCommit,
    license: "Apache-2.0",
    state_schema_version: stateSchemaVersion,
    config_schema_version: configSchemaVersion,
    codex,
    artifacts: validatedArchives.map(({ target, name, sha256 }) => ({ target, name, sha256 })),
  };
  await writeFile(join(releaseDir, "release-manifest.json"), `${JSON.stringify(manifest, null, 2)}\n`, "utf8");
  return { manifest, packages: records, npmRoot, tarballsDir, releaseDir };
}

export async function loadNativeDescriptors(descriptorPaths) {
  if (!Array.isArray(descriptorPaths)) fail("native descriptors must be an array");
  const binaries = new Map();
  const archives = [];
  const seenTargets = new Set();
  for (const descriptorPath of descriptorPaths) {
    const descriptorDirectory = dirname(resolve(descriptorPath));
    let descriptor;
    try {
      descriptor = JSON.parse(await readFile(descriptorPath, "utf8"));
    } catch {
      fail(`could not parse native descriptor ${descriptorPath}`);
    }
    const expectedKeys = ["archive_path", "archive_sha256", "binary_path", "package_name", "target"];
    if (!descriptor || typeof descriptor !== "object" || JSON.stringify(Object.keys(descriptor).sort()) !== JSON.stringify(expectedKeys)) {
      fail(`native descriptor has unexpected fields: ${descriptorPath}`);
    }
    const packageDescriptor = packageForName(descriptor.package_name);
    if (!packageDescriptor.target || descriptor.target !== packageDescriptor.target || seenTargets.has(descriptor.target)) {
      fail(`native descriptors must form the supported target set without duplicates`);
    }
    seenTargets.add(descriptor.target);
    if (typeof descriptor.binary_path !== "string" || typeof descriptor.archive_path !== "string" || typeof descriptor.archive_sha256 !== "string" || !SHA256.test(descriptor.archive_sha256)) {
      fail(`native descriptor fields are invalid: ${descriptorPath}`);
    }
    const binaryPath = requireContained(descriptorDirectory, descriptor.binary_path, "binary_path");
    const archivePath = requireContained(descriptorDirectory, descriptor.archive_path, "archive_path");
    await Promise.all([requireRegularFile(binaryPath, "descriptor binary"), requireRegularFile(archivePath, "descriptor archive")]);
    const realDirectory = await realpath(descriptorDirectory);
    const [realBinary, realArchive] = await Promise.all([realpath(binaryPath), realpath(archivePath)]);
    for (const [label, actual] of [["binary_path", realBinary], ["archive_path", realArchive]]) {
      const rel = relative(realDirectory, actual);
      if (rel.startsWith("..") || isAbsolute(rel)) fail(`${label} must stay beneath descriptor directory`);
    }
    binaries.set(packageDescriptor.name, binaryPath);
    archives.push({ target: descriptor.target, name: "", path: archivePath, sha256: descriptor.archive_sha256 });
  }
  if (seenTargets.size !== NATIVE_PACKAGES.length) fail("native descriptors must form the supported target set without duplicates");
  return { binaries, archives };
}

function parseCliArguments(args) {
  const options = { descriptors: [] };
  const repeated = new Set(["--native-descriptor"]);
  for (let index = 0; index < args.length; index += 1) {
    const option = args[index];
    if (!["--output", "--channel", "--version", "--source-commit", "--native-descriptor"].includes(option)) fail(`unsupported CLI option ${JSON.stringify(option)}`);
    const value = args[index + 1];
    if (!value || value.startsWith("--")) fail(`missing value for ${option}`);
    if (option === "--native-descriptor") options.descriptors.push(value);
    else if (Object.hasOwn(options, option)) fail(`duplicate CLI option ${option}`);
    else options[option] = value;
    index += 1;
  }
  for (const option of ["--output", "--channel", "--version", "--source-commit"]) if (!options[option]) fail(`missing required CLI option ${option}`);
  if (!repeated.has("--native-descriptor") || options.descriptors.length !== 3) fail("exactly three --native-descriptor options are required");
  return options;
}

export async function main(args = process.argv.slice(2), { log = console.log } = {}) {
  const options = parseCliArguments(args);
  const descriptors = await loadNativeDescriptors(options.descriptors);
  const archives = descriptors.archives.map((archive) => {
    const targetDescriptor = TARGET_BY_NAME.get(archive.target);
    return {
      ...archive,
      name: `colay-v${options["--version"]}-${archive.target}${archive.target === "x86_64-pc-windows-msvc" ? ".zip" : ".tar.gz"}`,
      packageName: targetDescriptor.name,
    };
  });
  const result = await stageRelease({
    repoRoot: REPOSITORY_ROOT,
    outputRoot: resolve(options["--output"]),
    channel: options["--channel"],
    version: options["--version"],
    sourceCommit: options["--source-commit"],
    binaries: descriptors.binaries,
    archives,
  });
  log(JSON.stringify({ npm: result.npmRoot, tarballs: result.tarballsDir, release: result.releaseDir }));
  return result;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
