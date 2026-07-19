import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { basename, join } from "node:path";
import { promisify } from "node:util";

const execFilePromise = promisify(execFile);
const ROOT_PACKAGE = "@kimohy/colay";
const NATIVE_PACKAGES = Object.freeze([
  "@kimohy/colay-darwin-arm64",
  "@kimohy/colay-linux-x64",
  "@kimohy/colay-win32-x64",
]);
const PUBLIC_TAGS = new Set(["nightly", "beta", "latest"]);
const VISIBILITY_ATTEMPTS = 7;
const RETRY_DELAY_MS = 2_000;

function fail(message) {
  throw new Error(`Invalid npm publication: ${message}`);
}

function isNotFound(error) {
  const output = [error?.code, error?.stdout, error?.stderr, error?.message]
    .filter((value) => typeof value === "string" || typeof value === "number")
    .join("\n");
  return /\bE404\b|\b404 Not Found\b/i.test(output);
}

function parseNpmJson(stdout, label) {
  try {
    return JSON.parse(stdout);
  } catch {
    throw new Error(`npm returned malformed JSON for ${label}`);
  }
}

async function npmJson(args, label, run = execFilePromise) {
  try {
    const { stdout } = await run("npm", args, { shell: false });
    return parseNpmJson(stdout, label);
  } catch (error) {
    if (isNotFound(error)) return undefined;
    throw error;
  }
}

async function npmRun(args, run = execFilePromise) {
  await run("npm", args, { shell: false });
}

export function createNpmClient({ run = execFilePromise } = {}) {
  return Object.freeze({
    viewIntegrity(name, version) {
      return npmJson(["view", `${name}@${version}`, "dist.integrity", "--json"], `${name}@${version}`, run);
    },
    publish(tarball, tag) {
      return npmRun(["publish", tarball, "--access", "public", "--tag", tag], run);
    },
    viewChannelVersion(name, distTag) {
      return npmJson(["view", `${name}@${distTag}`, "version", "--json"], `${name}@${distTag}`, run);
    },
  });
}

function safeFilename(filename) {
  return typeof filename === "string" && filename !== "" && filename !== "." && filename !== ".."
    && filename === basename(filename) && !/[\\/]/.test(filename);
}

function validatePackRecords(records, version) {
  if (!Array.isArray(records) || records.length !== NATIVE_PACKAGES.length + 1) {
    fail("npm-pack.json must contain exactly the root and three native package records");
  }
  const expectedNames = [...NATIVE_PACKAGES, ROOT_PACKAGE].sort();
  const actualNames = records.map(({ name }) => name).sort();
  if (JSON.stringify(actualNames) !== JSON.stringify(expectedNames)) {
    fail("npm-pack.json must contain the exact Colay package set");
  }
  const byName = new Map();
  for (const record of records) {
    if (!record || typeof record !== "object" || typeof record.name !== "string" || record.version !== version
      || !safeFilename(record.filename) || typeof record.integrity !== "string" || record.integrity === "") {
      fail("npm-pack.json contains an invalid name, version, filename, or integrity");
    }
    if (byName.has(record.name)) fail(`npm-pack.json contains duplicate package ${record.name}`);
    byName.set(record.name, Object.freeze({
      name: record.name,
      version: record.version,
      filename: record.filename,
      integrity: record.integrity,
    }));
  }
  return byName;
}

async function loadPackRecords(tarballsDir, version) {
  if (typeof tarballsDir !== "string" || tarballsDir === "") fail("tarballsDir is required");
  let records;
  try {
    records = JSON.parse(await readFile(join(tarballsDir, "npm-pack.json"), "utf8"));
  } catch {
    fail("could not read npm-pack.json");
  }
  return validatePackRecords(records, version);
}

async function defaultRetryDelay() {
  await new Promise((resolve) => setTimeout(resolve, RETRY_DELAY_MS));
}

function requireClient(npmClient) {
  if (!npmClient || typeof npmClient.viewIntegrity !== "function" || typeof npmClient.publish !== "function"
    || typeof npmClient.viewChannelVersion !== "function") {
    fail("npmClient must provide viewIntegrity, publish, and viewChannelVersion");
  }
}

async function waitForIntegrity({ npmClient, name, version, integrity, retryDelay }) {
  for (let attempt = 0; attempt < VISIBILITY_ATTEMPTS; attempt += 1) {
    const visible = await npmClient.viewIntegrity(name, version);
    if (visible === integrity) return;
    if (visible !== undefined && visible !== null) {
      fail(`integrity mismatch for ${name}@${version}: expected ${integrity}, found ${visible}`);
    }
    if (attempt + 1 < VISIBILITY_ATTEMPTS) await retryDelay();
  }
  fail(`${name}@${version} did not become visible with the published integrity after six retries`);
}

async function verifyOrPublish({ npmClient, record, tarballsDir, tag, retryDelay }) {
  const existing = await npmClient.viewIntegrity(record.name, record.version);
  if (existing !== undefined && existing !== null) {
    if (existing !== record.integrity) {
      fail(`integrity mismatch for ${record.name}@${record.version}: expected ${record.integrity}, found ${existing}`);
    }
    return "existing";
  }

  const published = await npmClient.publish(join(tarballsDir, record.filename), tag);
  // Test clients may return the registry's exact immutable integrity directly.
  // npm itself returns no machine-readable integrity here, so production performs
  // the explicit public-registry visibility check below.
  if (published?.integrity !== undefined) {
    if (published.integrity !== record.integrity) {
      fail(`integrity mismatch for ${record.name}@${record.version}: expected ${record.integrity}, found ${published.integrity}`);
    }
    return "published";
  }
  await waitForIntegrity({ npmClient, name: record.name, version: record.version, integrity: record.integrity, retryDelay });
  return "published";
}

async function requireRootChannel({ npmClient, version, distTag, retryDelay }) {
  for (let attempt = 0; attempt < VISIBILITY_ATTEMPTS; attempt += 1) {
    const channelVersion = await npmClient.viewChannelVersion(ROOT_PACKAGE, distTag);
    if (channelVersion === version) return;
    if (channelVersion !== undefined && channelVersion !== null) {
      fail(`root package ${ROOT_PACKAGE}@${version} is not the ${distTag} channel target (currently ${channelVersion}). Recover manually with: npm dist-tag add ${ROOT_PACKAGE}@${version} ${distTag}`);
    }
    if (attempt + 1 < VISIBILITY_ATTEMPTS) await retryDelay();
  }
  fail(`${ROOT_PACKAGE}@${version} did not become the ${distTag} channel target after six retries`);
}

export async function publishRelease({ tarballsDir, version, distTag, npmClient, retryDelay = defaultRetryDelay }) {
  if (typeof version !== "string" || version === "") fail("version is required");
  if (!PUBLIC_TAGS.has(distTag)) fail("distTag must be one of nightly, beta, latest");
  if (typeof retryDelay !== "function") fail("retryDelay must be a function");
  requireClient(npmClient);
  const records = await loadPackRecords(tarballsDir, version);
  const results = [];
  for (const name of NATIVE_PACKAGES) {
    const record = records.get(name);
    results.push({ name, state: await verifyOrPublish({ npmClient, record, tarballsDir, tag: "colay-candidate", retryDelay }) });
  }
  const rootRecord = records.get(ROOT_PACKAGE);
  results.push({ name: ROOT_PACKAGE, state: await verifyOrPublish({ npmClient, record: rootRecord, tarballsDir, tag: distTag, retryDelay }) });
  await requireRootChannel({ npmClient, version, distTag, retryDelay });
  return { version, distTag, packages: results };
}
