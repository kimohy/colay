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
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
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
  Object.freeze({ name: "@kimohy/colay-win32-x64", directory: "colay-win32-x64", binary: "bin/colay.exe", target: "x86_64-pc-windows-msvc", os: ["win32"], cpu: ["x64"], libc: null }),
  Object.freeze({ name: "@kimohy/colay-darwin-arm64", directory: "colay-darwin-arm64", binary: "bin/colay", target: "aarch64-apple-darwin", os: ["darwin"], cpu: ["arm64"], libc: null }),
  Object.freeze({ name: "@kimohy/colay-linux-x64", directory: "colay-linux-x64", binary: "bin/colay", target: "x86_64-unknown-linux-musl", os: ["linux"], cpu: ["x64"], libc: null }),
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

function codeUnitCompare(left, right) {
  return left < right ? -1 : Number(left > right);
}

function equalArrays(actual, expected) {
  return Array.isArray(actual) && actual.length === expected.length && actual.every((value, index) => value === expected[index]);
}

function expectedArchiveName(version, target) {
  return `colay-v${version}-${target}${target === "x86_64-pc-windows-msvc" ? ".zip" : ".tar.gz"}`;
}

function safeBasename(filename) {
  return typeof filename === "string" && filename !== "" && filename !== "." && filename !== ".." && !/[\\/]/.test(filename) && filename === basename(filename);
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
  return [...files].sort(codeUnitCompare);
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
  const lines = text.split("\n");
  const firstTable = lines.findIndex((line) => /^\s*\[/.test(line));
  const topLevel = lines.slice(0, firstTable === -1 ? lines.length : firstTable);
  const tableLines = firstTable === -1 ? [] : lines.slice(firstTable);
  const authority = (name, expression) => {
    const matches = topLevel.map((line) => expression.exec(line)).filter(Boolean);
    const relocated = tableLines.some((line) => new RegExp(`^\\s*${name}\\s*=`).test(line));
    if (matches.length !== 1 || relocated) fail("Codex authority fields must appear exactly once before the first TOML table");
    return matches[0][1];
  };
  const supportedMin = authority("supported_min", /^supported_min = "([0-9]+\.[0-9]+\.[0-9]+)"$/);
  const tested = authority("tested_versions", /^tested_versions = \[([^\]]*)\]$/);
  const recommended = authority("recommended", /^recommended = "([0-9]+\.[0-9]+\.[0-9]+)"$/);
  const pinnedRevision = authority("pinned_revision", /^pinned_revision = "([0-9a-f]{40})"$/);
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
    const expectedName = expectedArchiveName(version, target);
    if (archive.name !== expectedName) fail(`archive name for ${target} must be ${expectedName}`);
    if (typeof archive.path !== "string") fail(`archive path for ${target} is required`);
    if (typeof archive.sha256 !== "string" || !SHA256.test(archive.sha256)) fail(`archive SHA-256 for ${target} must be lowercase hexadecimal`);
    await requireRegularFile(archive.path, `archive for ${target}`);
    if ((await sha256File(archive.path)) !== archive.sha256) fail(`archive digest mismatch for ${target}`);
    validated.push({ target, name: archive.name, path: archive.path, sha256: archive.sha256, packageName: packageDescriptor.name });
  }
  return validated.sort((left, right) => codeUnitCompare(left.target, right.target));
}

function rejectRuntimeDependencies(manifest, packageName, allowedOptionalDependencies) {
  for (const field of ["dependencies", "peerDependencies", "bundledDependencies", "bundleDependencies"]) {
    if (Object.hasOwn(manifest, field)) fail(`${packageName === "@kimohy/colay" ? "root package" : `native package ${packageName}`} must not declare ${field}`);
  }
  if (allowedOptionalDependencies) {
    if (!manifest.optionalDependencies || typeof manifest.optionalDependencies !== "object" || Array.isArray(manifest.optionalDependencies)) {
      fail("root package must declare exact optionalDependencies");
    }
    const names = Object.keys(manifest.optionalDependencies).sort(codeUnitCompare);
    const expected = NATIVE_PACKAGES.map((item) => item.name).sort(codeUnitCompare);
    if (!equalArrays(names, expected) || names.some((name) => typeof manifest.optionalDependencies[name] !== "string" || manifest.optionalDependencies[name] === "")) {
      fail("root package optionalDependencies must exactly name the native packages");
    }
  } else if (Object.hasOwn(manifest, "optionalDependencies")) {
    fail(`native package ${packageName} must not declare optionalDependencies`);
  }
}

function validateManifestContract(manifest, descriptor) {
  if (manifest.name !== descriptor.name) fail(`template metadata does not match ${descriptor.name}`);
  if (manifest.license !== "Apache-2.0") fail(`template license must be Apache-2.0 for ${descriptor.name}`);
  for (const lifecycle of ["preinstall", "install", "postinstall", "prepublish", "prepublishOnly", "prepare", "prepack", "postpack", "preversion", "version", "postversion"]) {
    if (manifest.scripts && Object.hasOwn(manifest.scripts, lifecycle)) fail(`package ${descriptor.name} must not declare lifecycle script ${lifecycle}`);
  }
  if (descriptor.binary === null) {
    if (manifest.type !== "commonjs" || JSON.stringify(manifest.bin) !== JSON.stringify({ colay: "bin/colay.js" }) || manifest.engines?.node !== ">=22") {
      fail("root package metadata does not match the launcher contract");
    }
    if (Object.hasOwn(manifest, "os") || Object.hasOwn(manifest, "cpu") || Object.hasOwn(manifest, "libc")) fail("root package must not declare native platform metadata");
    rejectRuntimeDependencies(manifest, descriptor.name, true);
    return;
  }
  if (manifest.binary !== descriptor.binary) fail(`template metadata does not match ${descriptor.name}`);
  for (const field of ["os", "cpu", "libc"]) {
    const expected = descriptor[field];
    if (expected === null) {
      if (Object.hasOwn(manifest, field)) fail(`native package ${descriptor.name} must not declare ${field}`);
    } else if (!equalArrays(manifest[field], expected)) {
      fail(`native package ${descriptor.name} ${field} must be ${JSON.stringify(expected)}`);
    }
  }
  rejectRuntimeDependencies(manifest, descriptor.name, false);
}

async function copyAndRewritePackage({ repoRoot, npmRoot, rootLicense, descriptor, version, binaries }) {
  const source = join(repoRoot, "npm", descriptor.directory);
  const destination = join(npmRoot, descriptor.directory);
  await cp(source, destination, { recursive: true, force: false, errorOnExist: true });
  const manifestPath = join(destination, "package.json");
  const manifest = JSON.parse(await readFile(manifestPath, "utf8"));
  validateManifestContract(manifest, descriptor);
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

export function parseNpmPackRecord(output, packageName, version) {
  let records;
  try {
    records = JSON.parse(output);
  } catch {
    fail(`npm pack did not return JSON for ${packageName}`);
  }
  if (!Array.isArray(records) || records.length !== 1) fail(`npm pack must return exactly one record for ${packageName}`);
  const [record] = records;
  if (!record || typeof record !== "object" || record.name !== packageName) fail(`npm pack returned wrong package name for ${packageName}`);
  if (record.version !== version) fail(`npm pack returned wrong version for ${packageName}`);
  if (!safeBasename(record.filename)) fail(`npm pack returned unsafe filename for ${packageName}`);
  return record;
}

function assertPackFiles(record, packageName) {
  const actual = Array.isArray(record.files) ? record.files.map(({ path }) => path).sort(codeUnitCompare) : [];
  const expected = allowedPackageFiles(packageName);
  if (JSON.stringify(actual) !== JSON.stringify(expected)) {
    const unexpected = actual.filter((file) => !expected.includes(file));
    const missing = expected.filter((file) => !actual.includes(file));
    fail(`unexpected package files for ${packageName}: ${[...unexpected, ...missing.map((file) => `missing ${file}`)].join(", ")}`);
  }
}

async function packAndVerify(packageName, version, packageDir, tarballsDir) {
  const dryRunInvocation = npmInvocation(["pack", "--json", "--dry-run"]);
  const dryRun = await execFilePromise(dryRunInvocation.command, dryRunInvocation.args, { cwd: packageDir, shell: false });
  const record = parseNpmPackRecord(dryRun.stdout, packageName, version);
  assertPackFiles(record, packageName);
  const packedInvocation = npmInvocation(["pack", "--json", "--pack-destination", tarballsDir]);
  const packed = await execFilePromise(packedInvocation.command, packedInvocation.args, { cwd: packageDir, shell: false });
  const packedRecord = parseNpmPackRecord(packed.stdout, packageName, version);
  assertPackFiles(packedRecord, packageName);
  const filename = packedRecord.filename;
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
  for (const descriptor of PACKAGES) records.push(await packAndVerify(descriptor.name, version, packageDirs.get(descriptor.name), tarballsDir));
  await writeFile(join(tarballsDir, "npm-pack.json"), `${JSON.stringify(records, null, 2)}\n`, "utf8");

  for (const archive of validatedArchives) await copyFile(archive.path, join(releaseDir, archive.name));
  const checksumLines = validatedArchives
    .map(({ name, sha256 }) => `${sha256}  ${name}`)
    .sort(codeUnitCompare);
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

export async function loadNativeDescriptors(descriptorPaths, { version } = {}) {
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
    if (version !== undefined && basename(descriptor.archive_path) !== expectedArchiveName(version, descriptor.target)) {
      fail(`archive_path basename for ${descriptor.target} must be ${expectedArchiveName(version, descriptor.target)}`);
    }
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
  const descriptors = await loadNativeDescriptors(options.descriptors, { version: options["--version"] });
  const archives = descriptors.archives.map((archive) => {
    const targetDescriptor = TARGET_BY_NAME.get(archive.target);
    return {
      ...archive,
      name: expectedArchiveName(options["--version"], archive.target),
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
