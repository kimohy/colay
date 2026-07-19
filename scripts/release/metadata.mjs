import { appendFile, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, resolve } from "node:path";

const STABLE = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)$/;
const BETA = /^(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)-beta\.(0|[1-9]\d*)$/;
const SHA = /^[0-9a-f]{40}$/;

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../..");

function fail(message) {
  throw new Error(`Invalid release metadata: ${message}`);
}

function parseWorkspaceVersion(version) {
  const stable = STABLE.exec(version);
  if (stable) {
    return { major: stable[1], minor: stable[2], patch: stable[3], prerelease: false };
  }
  const beta = BETA.exec(version);
  if (beta) {
    return { major: beta[1], minor: beta[2], patch: beta[3], prerelease: true };
  }
  fail(`unsupported workspace version ${JSON.stringify(version)}`);
}

function validateInputs({ sha, now, workspaceVersion, templateVersion }) {
  if (!SHA.test(sha)) {
    fail("SHA must be exactly 40 lowercase hexadecimal characters");
  }
  if (!(now instanceof Date) || Number.isNaN(now.getTime())) {
    fail("now must be a valid Date");
  }
  if (workspaceVersion !== templateVersion) {
    fail("Cargo and npm template versions must match");
  }
  return parseWorkspaceVersion(workspaceVersion);
}

function incrementDecimal(value) {
  let carry = 1;
  const digits = value.split("");
  for (let index = digits.length - 1; index >= 0 && carry === 1; index -= 1) {
    const next = digits[index].charCodeAt(0) - 48 + carry;
    digits[index] = String(next % 10);
    carry = Math.floor(next / 10);
  }
  return carry === 1 ? `1${digits.join("")}` : digits.join("");
}

function nightlyVersion(version, now, sha) {
  const date = [
    now.getUTCFullYear().toString().padStart(4, "0"),
    (now.getUTCMonth() + 1).toString().padStart(2, "0"),
    now.getUTCDate().toString().padStart(2, "0"),
  ].join("");
  const patch = version.prerelease ? version.patch : incrementDecimal(version.patch);
  const shortSha = sha.slice(0, 7);
  const semverIdentifier = /^\d{7}$/.test(shortSha) ? `g${shortSha}` : shortSha;
  return `${version.major}.${version.minor}.${patch}-nightly.${date}.${semverIdentifier}`;
}

export function classifyRelease({ ref, sha, now, workspaceVersion, templateVersion }) {
  const parsedVersion = validateInputs({ sha, now, workspaceVersion, templateVersion });

  if (ref === "refs/heads/main") {
    return {
      channel: "nightly",
      version: nightlyVersion(parsedVersion, now, sha),
      distTag: "nightly",
      githubMode: "artifact",
      retentionDays: 14,
    };
  }

  const tagPrefix = "refs/tags/v";
  if (!ref.startsWith(tagPrefix)) {
    fail(`unsupported ref ${JSON.stringify(ref)}`);
  }

  const version = ref.slice(tagPrefix.length);
  if (version !== workspaceVersion || version !== templateVersion) {
    fail("tag, Cargo, and npm template versions must match");
  }
  if (BETA.test(version)) {
    return {
      channel: "beta",
      version,
      distTag: "beta",
      githubMode: "prerelease",
      retentionDays: null,
    };
  }
  if (STABLE.test(version)) {
    return {
      channel: "stable",
      version,
      distTag: "latest",
      githubMode: "release",
      retentionDays: null,
    };
  }
  fail(`unsupported release tag ${JSON.stringify(ref)}`);
}

function parseArguments(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const option = args[index];
    if (!["--ref", "--sha", "--now", "--github-output"].includes(option)) {
      fail(`unsupported CLI option ${JSON.stringify(option)}`);
    }
    if (Object.hasOwn(options, option)) {
      fail(`duplicate CLI option ${JSON.stringify(option)}`);
    }
    const value = args[index + 1];
    if (value === undefined || value.startsWith("--")) {
      fail(`missing value for ${option}`);
    }
    options[option] = value;
    index += 1;
  }
  return options;
}

async function readCheckedInVersions() {
  const [cargoToml, packageJson] = await Promise.all([
    readFile(resolve(repositoryRoot, "Cargo.toml"), "utf8"),
    readFile(resolve(repositoryRoot, "npm/colay/package.json"), "utf8"),
  ]);
  const workspaceVersion = /^version\s*=\s*"([^"]+)"\s*$/m.exec(cargoToml)?.[1];
  const templateVersion = JSON.parse(packageJson).version;
  if (!workspaceVersion || typeof templateVersion !== "string") {
    fail("could not read checked-in Cargo and npm template versions");
  }
  return { workspaceVersion, templateVersion };
}

async function appendGithubOutput(path, metadata) {
  const entries = [
    ["channel", metadata.channel],
    ["version", metadata.version],
    ["dist_tag", metadata.distTag],
    ["github_mode", metadata.githubMode],
    ["retention_days", metadata.retentionDays ?? ""],
  ];
  await appendFile(path, `${entries.map(([key, value]) => `${key}=${value}`).join("\n")}\n`, "utf8");
}

export async function main(args = process.argv.slice(2), { env = process.env, log = console.log } = {}) {
  const options = parseArguments(args);
  const { workspaceVersion, templateVersion } = await readCheckedInVersions();
  const metadata = classifyRelease({
    ref: options["--ref"] ?? env.GITHUB_REF,
    sha: options["--sha"] ?? env.GITHUB_SHA,
    now: options["--now"] === undefined ? new Date() : new Date(options["--now"]),
    workspaceVersion,
    templateVersion,
  });

  if (options["--github-output"] !== undefined) {
    await appendGithubOutput(options["--github-output"], metadata);
  }
  log(JSON.stringify(metadata));
  return metadata;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
