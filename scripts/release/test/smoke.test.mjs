import assert from "node:assert/strict";
import { mkdtemp, mkdir, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { smokeInstall } from "../smoke.mjs";

const version = "0.1.1-nightly.20260719.a1b2c3d";

async function fixture({ records } = {}) {
  const root = await mkdtemp(join(tmpdir(), "colay-smoke-"));
  const tarballsDir = join(root, "tarballs");
  const prefix = join(root, "prefix");
  await mkdir(tarballsDir);
  const packageRecords = records ?? [
    { name: "@kimohy/colay", version, filename: "kimohy-colay.tgz", integrity: "sha512-root" },
    { name: "@kimohy/colay-linux-x64", version, filename: "kimohy-colay-linux-x64.tgz", integrity: "sha512-native" },
  ];
  await writeFile(join(tarballsDir, "npm-pack.json"), `${JSON.stringify(packageRecords, null, 2)}\n`);
  for (const { filename } of packageRecords) {
    await writeFile(join(tarballsDir, filename), filename);
  }
  return { tarballsDir, prefix };
}

test("installs root and selected native tarballs offline, then runs the isolated shim", async () => {
  const values = await fixture();
  const calls = [];
  const run = async (command, args, options) => {
    calls.push([command, args, options]);
    if (command === "npm") return { code: 0, stdout: "", stderr: "" };
    return { code: 0, stdout: `colay ${version}\n`, stderr: "" };
  };
  await smokeInstall({
    ...values,
    packageName: "@kimohy/colay-linux-x64",
    version,
    platform: "linux",
    run,
  });
  assert.deepEqual(calls[0], [
    "npm",
    [
      "install", "--global", "--offline", "--ignore-scripts", "--prefix", values.prefix,
      join(values.tarballsDir, "kimohy-colay.tgz"),
      join(values.tarballsDir, "kimohy-colay-linux-x64.tgz"),
    ],
    { shell: false },
  ]);
  assert.deepEqual(calls[1], [join(values.prefix, "bin", "colay"), ["--version"], { shell: false }]);
});

test("uses the Windows npm command shim without searching PATH", async () => {
  const values = await fixture({ records: [
    { name: "@kimohy/colay", version, filename: "root.tgz", integrity: "sha512-root" },
    { name: "@kimohy/colay-win32-x64", version, filename: "win.tgz", integrity: "sha512-win" },
  ] });
  const calls = [];
  await smokeInstall({
    ...values,
    packageName: "@kimohy/colay-win32-x64",
    version,
    platform: "win32",
    run: async (command, args, options) => {
      calls.push([command, args, options]);
      return command === "npm"
        ? { code: 0, stdout: "", stderr: "" }
        : { code: 0, stdout: `colay ${version}\r\n`, stderr: "" };
    },
  });
  assert.equal(calls[1][0], join(values.prefix, "colay.cmd"));
});

test("fails specifically for invalid packages, metadata, commands, and versions", async (t) => {
  await t.test("unknown native package", async () => {
    const values = await fixture();
    await assert.rejects(
      smokeInstall({ ...values, packageName: "@kimohy/colay-freebsd-x64", version, run: async () => ({ code: 0 }) }),
      /unsupported native package @kimohy\/colay-freebsd-x64/,
    );
  });
  await t.test("missing tarball record", async () => {
    const values = await fixture({ records: [{ name: "@kimohy/colay", version, filename: "root.tgz" }] });
    await assert.rejects(
      smokeInstall({ ...values, packageName: "@kimohy/colay-linux-x64", version, run: async () => ({ code: 0 }) }),
      /missing tarball metadata for @kimohy\/colay-linux-x64@/,
    );
  });
  await t.test("duplicate tarball record", async () => {
    const duplicate = { name: "@kimohy/colay", version, filename: "root.tgz" };
    const values = await fixture({ records: [duplicate, duplicate, { name: "@kimohy/colay-linux-x64", version, filename: "native.tgz" }] });
    await assert.rejects(
      smokeInstall({ ...values, packageName: "@kimohy/colay-linux-x64", version, run: async () => ({ code: 0 }) }),
      /duplicate tarball metadata for @kimohy\/colay@/,
    );
  });
  await t.test("npm install failure", async () => {
    const values = await fixture();
    await assert.rejects(
      smokeInstall({ ...values, packageName: "@kimohy/colay-linux-x64", version, run: async () => ({ code: 17, stderr: "offline miss" }) }),
      /npm install failed with exit code 17: offline miss/,
    );
  });
  await t.test("Colay failure", async () => {
    const values = await fixture();
    let call = 0;
    await assert.rejects(
      smokeInstall({
        ...values,
        packageName: "@kimohy/colay-linux-x64",
        version,
        run: async () => (++call === 1 ? { code: 0 } : { code: 9, stderr: "broken" }),
      }),
      /colay --version failed with exit code 9: broken/,
    );
  });
  await t.test("version mismatch", async () => {
    const values = await fixture();
    let call = 0;
    await assert.rejects(
      smokeInstall({
        ...values,
        packageName: "@kimohy/colay-linux-x64",
        version,
        run: async () => (++call === 1 ? { code: 0 } : { code: 0, stdout: "colay wrong\n" }),
      }),
      /colay version mismatch: expected "colay .*", received "colay wrong"/,
    );
  });
});
