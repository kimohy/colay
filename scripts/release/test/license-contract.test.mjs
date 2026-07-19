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
  const cargoToml = await readFile(join(repoRoot, "Cargo.toml"), "utf8");
  const workspaceVersion = /^\[workspace\.package\]\r?\n[\s\S]*?^version\s*=\s*"([^"]+)"\s*$/m.exec(cargoToml)?.[1];
  assert.ok(workspaceVersion, "workspace package version must be declared in Cargo.toml");
  const expected = new Map([
    ["npm/colay/package.json", "@kimohy/colay"],
    ["npm/colay-win32-x64/package.json", "@kimohy/colay-win32-x64"],
    ["npm/colay-darwin-arm64/package.json", "@kimohy/colay-darwin-arm64"],
    ["npm/colay-linux-x64/package.json", "@kimohy/colay-linux-x64"],
  ]);

  for (const [relative, name] of expected) {
    const manifest = JSON.parse(await readFile(join(repoRoot, relative), "utf8"));
    assert.equal(manifest.name, name);
    assert.equal(manifest.version, workspaceVersion);
    assert.equal(manifest.license, "Apache-2.0");
    assert.equal(manifest.publishConfig.access, "public");

    const packageLicense = await readFile(join(repoRoot, dirname(relative), "LICENSE"));
    assert.deepEqual(packageLicense, rootLicense);
  }

  const rootPackage = JSON.parse(
    await readFile(join(repoRoot, "npm/colay/package.json"), "utf8"),
  );
  assert.deepEqual(rootPackage.optionalDependencies, {
    "@kimohy/colay-darwin-arm64": workspaceVersion,
    "@kimohy/colay-linux-x64": workspaceVersion,
    "@kimohy/colay-win32-x64": workspaceVersion,
  });

  const linuxPackage = JSON.parse(
    await readFile(join(repoRoot, "npm/colay-linux-x64/package.json"), "utf8"),
  );
  assert.deepEqual(linuxPackage.os, ["linux"]);
  assert.deepEqual(linuxPackage.cpu, ["x64"]);
  assert.equal(Object.hasOwn(linuxPackage, "libc"), false);
});

test("license metadata contract derives its expected version instead of embedding a release number", async () => {
  const source = await readFile(fileURLToPath(import.meta.url), "utf8");
  assert.doesNotMatch(source, /"0\.1\.0"/);
});

test("installation and release documentation describe the supported distribution contract", async () => {
  const [readme, releaseGuide, testingGuide] = await Promise.all([
    readFile(join(repoRoot, "README.md"), "utf8"),
    readFile(join(repoRoot, "docs/release.md"), "utf8"),
    readFile(join(repoRoot, "docs/testing.md"), "utf8"),
  ]);

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
  assert.match(releaseGuide, /dist\/tarballs\/npm-pack\.json/);
  assert.match(releaseGuide, /dist\/tarballs\/kimohy-colay-darwin-arm64-<version>\.tgz/);
  assert.match(releaseGuide, /dist\/tarballs\/kimohy-colay-linux-x64-<version>\.tgz/);
  assert.match(releaseGuide, /dist\/tarballs\/kimohy-colay-win32-x64-<version>\.tgz/);
  assert.match(releaseGuide, /dist\/tarballs\/kimohy-colay-<version>\.tgz/);
  assert.doesNotMatch(releaseGuide, /tarballs\/@kimohy-colay/);
  assert.match(releaseGuide, /npm trust github/);
  assert.match(releaseGuide, /vX\.Y\.Z-beta\.N/);
  assert.match(releaseGuide, /vX\.Y\.Z/);
  assert.match(releaseGuide, /\(cd dist\/release && sha256sum --check SHA256SUMS\)/);
  assert.match(releaseGuide, /gh attestation verify <archive> --repo kimohy\/colay/);
  assert.match(releaseGuide, /npm audit signatures --json --include-attestations/);
  assert.match(releaseGuide, /Trusted Publishing[\s\S]*automatically.*provenance/i);
  assert.match(testingGuide, /npm test/);
  assert.match(testingGuide, /scripts\/release\/test\/workflow-contract\.test\.mjs/);
  assert.match(testingGuide, /git diff --check/);
});
