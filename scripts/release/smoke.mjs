import { execFile } from "node:child_process";
import { lstat, readFile, realpath } from "node:fs/promises";
import { basename, dirname, isAbsolute, join, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

const execFilePromise = promisify(execFile);
const ROOT_PACKAGE = "@kimohy/colay";
const NATIVE_PACKAGES = new Map([
  ["@kimohy/colay-win32-x64", "win32"],
  ["@kimohy/colay-darwin-arm64", "darwin"],
  ["@kimohy/colay-linux-x64", "linux"],
]);
const NPM_CLI = join(dirname(process.execPath), "node_modules", "npm", "bin", "npm-cli.js");

function fail(message) {
  throw new Error(`Invalid release smoke test: ${message}`);
}

async function defaultRun(command, args, options) {
  const invocation = command === "npm" && process.platform === "win32"
    ? { command: process.execPath, args: [NPM_CLI, ...args] }
    : { command, args };
  try {
    const { stdout, stderr } = await execFilePromise(invocation.command, invocation.args, options);
    return { code: 0, stdout, stderr };
  } catch (error) {
    return { code: typeof error.code === "number" ? error.code : 1, stdout: error.stdout ?? "", stderr: error.stderr ?? error.message };
  }
}

function resultText(result) {
  return typeof result?.stderr === "string" && result.stderr !== "" ? result.stderr : "no diagnostic";
}

async function loadTarball(tarballsDir, packageName, version) {
  let records;
  try {
    records = JSON.parse(await readFile(join(tarballsDir, "npm-pack.json"), "utf8"));
  } catch {
    fail("could not read npm pack metadata");
  }
  if (!Array.isArray(records)) fail("npm pack metadata must be an array");
  const matches = records.filter((record) => record?.name === packageName && record?.version === version);
  if (matches.length === 0) fail(`missing tarball metadata for ${packageName}@${version}`);
  if (matches.length > 1) fail(`duplicate tarball metadata for ${packageName}@${version}`);
  const filename = matches[0].filename;
  if (typeof filename !== "string" || filename === "" || filename === "." || filename === ".." || /[\\/]/.test(filename) || filename !== basename(filename)) {
    fail(`unsafe tarball filename for ${packageName}@${version}`);
  }
  const resolvedDirectory = resolve(tarballsDir);
  const path = resolve(resolvedDirectory, filename);
  const lexicalRelative = relative(resolvedDirectory, path);
  if (lexicalRelative === "" || lexicalRelative.startsWith("..") || isAbsolute(lexicalRelative)) fail(`unsafe tarball filename for ${packageName}@${version}`);
  let stat;
  try {
    stat = await lstat(path);
  } catch {
    fail(`tarball is missing for ${packageName}@${version}`);
  }
  if (!stat.isFile()) fail(`tarball must be a regular file for ${packageName}@${version}`);
  const [realDirectory, realTarball] = await Promise.all([realpath(resolvedDirectory), realpath(path)]);
  const physicalRelative = relative(realDirectory, realTarball);
  if (physicalRelative === "" || physicalRelative.startsWith("..") || isAbsolute(physicalRelative)) fail(`tarball must stay beneath tarballsDir for ${packageName}@${version}`);
  return path;
}

export async function smokeInstall({ tarballsDir, prefix, packageName, version, platform = process.platform, run = defaultRun }) {
  if (!NATIVE_PACKAGES.has(packageName)) fail(`unsupported native package ${packageName}`);
  if (typeof tarballsDir !== "string" || typeof prefix !== "string" || typeof version !== "string") fail("tarballsDir, prefix, and version are required");
  const [rootTarball, nativeTarball] = await Promise.all([
    loadTarball(tarballsDir, ROOT_PACKAGE, version),
    loadTarball(tarballsDir, packageName, version),
  ]);
  const install = await run("npm", ["install", "--global", "--offline", "--ignore-scripts", "--prefix", prefix, rootTarball, nativeTarball], { shell: false });
  if (install?.code !== 0) fail(`npm install failed with exit code ${install?.code}: ${resultText(install)}`);
  const shim = platform === "win32" ? join(prefix, "colay.cmd") : join(prefix, "bin", "colay");
  const installedLauncher = join(prefix, "node_modules", "@kimohy", "colay", "bin", "colay.js");
  const launched = platform === "win32"
    ? await run(process.execPath, [installedLauncher, "--version"], { shell: false })
    : await run(shim, ["--version"], { shell: false });
  if (launched?.code !== 0) fail(`colay --version failed with exit code ${launched?.code}: ${resultText(launched)}`);
  const expected = `colay ${version}`;
  const received = typeof launched.stdout === "string" ? launched.stdout.replace(/\r?\n$/, "") : "";
  if (received !== expected) fail(`colay version mismatch: expected ${JSON.stringify(expected)}, received ${JSON.stringify(received)}`);
  return { shim, rootTarball, nativeTarball };
}

function parseCliArguments(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const option = args[index];
    if (!["--tarballs-dir", "--prefix", "--package-name", "--version"].includes(option)) fail(`unsupported CLI option ${JSON.stringify(option)}`);
    if (Object.hasOwn(options, option)) fail(`duplicate CLI option ${option}`);
    const value = args[index + 1];
    if (!value || value.startsWith("--")) fail(`missing value for ${option}`);
    options[option] = value;
    index += 1;
  }
  for (const option of ["--tarballs-dir", "--prefix", "--package-name", "--version"]) if (!options[option]) fail(`missing required CLI option ${option}`);
  return options;
}

export async function main(args = process.argv.slice(2), { log = console.log } = {}) {
  const options = parseCliArguments(args);
  const result = await smokeInstall({
    tarballsDir: resolve(options["--tarballs-dir"]),
    prefix: resolve(options["--prefix"]),
    packageName: options["--package-name"],
    version: options["--version"],
  });
  log(JSON.stringify(result));
  return result;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
