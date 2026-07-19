import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { fileURLToPath } from "node:url";

function fail(message) {
  throw new Error(`Invalid release notes: ${message}`);
}

function requiredSection(guide, start, end, label) {
  const startIndex = guide.indexOf(start);
  if (startIndex === -1) fail(`could not find ${label}`);
  const contentStart = startIndex + start.length;
  const endIndex = guide.indexOf(end, contentStart);
  if (endIndex === -1) fail(`could not find the end of ${label}`);
  return guide.slice(contentStart, endIndex).trim();
}

export function renderReleaseNotes({ guide, manifest }) {
  if (typeof guide !== "string" || !manifest || typeof manifest !== "object") fail("guide and manifest are required");
  const normalizedGuide = guide.replaceAll("\r\n", "\n");
  const { version, state_schema_version: stateSchema, config_schema_version: configSchema, codex } = manifest;
  if (typeof version !== "string" || !Number.isInteger(stateSchema) || !Number.isInteger(configSchema)
    || !codex || typeof codex !== "object" || typeof codex.supported_min !== "string"
    || !Array.isArray(codex.tested_versions) || typeof codex.recommended !== "string") {
    fail("manifest lacks required release compatibility metadata");
  }
  const obligation = requiredSection(
    normalizedGuide,
    "Every Colay release must state ",
    "\n\nCurrent known limitations:",
    "release-note obligation",
  );
  const limitations = requiredSection(
    normalizedGuide,
    "Current known limitations:\n",
    "\n\nSee [`rollback.md`](rollback.md) for ",
    "current known limitations",
  );
  const rollback = requiredSection(
    normalizedGuide,
    "\n\nSee [`rollback.md`](rollback.md) for ",
    ".",
    "rollback procedure",
  );
  return [
    `# Colay v${version}`,
    "",
    `Supported Codex versions: ${codex.tested_versions.join(", ")} (minimum ${codex.supported_min}).`,
    `Recommended Codex version: ${codex.recommended}.`,
    `State schema ${stateSchema} and config schema ${configSchema} may require the documented migration procedure before use.`,
    "",
    "## Release compatibility obligation",
    obligation,
    "",
    "## Known compatibility limitations",
    limitations,
    "",
    "## Rollback",
    `Use the release manifest and ${rollback}.`,
    "",
    "Verify the attached SHA256SUMS, release-manifest.json, artifact attestation, and npm provenance before deployment.",
    "",
  ].join("\n");
}

function parseArguments(args) {
  const options = {};
  for (let index = 0; index < args.length; index += 1) {
    const option = args[index];
    if (!["--manifest", "--release-guide", "--output"].includes(option)) fail(`unsupported CLI option ${JSON.stringify(option)}`);
    if (Object.hasOwn(options, option)) fail(`duplicate CLI option ${JSON.stringify(option)}`);
    const value = args[index + 1];
    if (!value || value.startsWith("--")) fail(`missing value for ${option}`);
    options[option] = value;
    index += 1;
  }
  for (const option of ["--manifest", "--release-guide", "--output"]) {
    if (!Object.hasOwn(options, option)) fail(`missing required CLI option ${option}`);
  }
  return options;
}

export async function main(args = process.argv.slice(2), { log = console.log } = {}) {
  const options = parseArguments(args);
  const [guide, manifestText] = await Promise.all([
    readFile(resolve(options["--release-guide"]), "utf8"),
    readFile(resolve(options["--manifest"]), "utf8"),
  ]);
  const notes = renderReleaseNotes({ guide, manifest: JSON.parse(manifestText) });
  await writeFile(resolve(options["--output"]), notes, "utf8");
  log(JSON.stringify({ output: resolve(options["--output"]) }));
  return notes;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  main().catch((error) => {
    console.error(error.message);
    process.exitCode = 1;
  });
}
