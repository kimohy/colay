import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";
import test from "node:test";
import { promisify } from "node:util";

const repoRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");
const execFileAsync = promisify(execFile);

test("Apache license and Cargo metadata use Apache-2.0", async () => {
  const license = await readFile(join(repoRoot, "LICENSE"));
  assert.match(
    license.toString("utf8"),
    /Apache License\n                           Version 2\.0, January 2004/,
  );

  const { stdout } = await execFileAsync(
    "cargo",
    ["metadata", "--no-deps", "--format-version", "1"],
    { cwd: repoRoot },
  );
  const metadata = JSON.parse(stdout);
  const workspacePackages = metadata.packages.filter((pkg) =>
    metadata.workspace_members.includes(pkg.id),
  );

  for (const pkg of workspacePackages) {
    assert.equal(pkg.license, "Apache-2.0", pkg.name);
  }
});

test("package metadata and licenses match the release contract", async () => {
  const rootLicense = await readFile(join(repoRoot, "LICENSE"));
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

    const packageLicense = await readFile(join(repoRoot, dirname(relative), "LICENSE"));
    assert.deepEqual(packageLicense, rootLicense);
  }

  const rootPackage = JSON.parse(
    await readFile(join(repoRoot, "npm/colay/package.json"), "utf8"),
  );
  assert.deepEqual(rootPackage.optionalDependencies, {
    "@kimohy/colay-darwin-arm64": "0.1.0",
    "@kimohy/colay-linux-x64": "0.1.0",
    "@kimohy/colay-win32-x64": "0.1.0",
  });
});
