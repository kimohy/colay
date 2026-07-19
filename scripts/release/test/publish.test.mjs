import assert from "node:assert/strict";
import { mkdtemp, readFile, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { publishRelease } from "../publish.mjs";

const version = "0.1.1-nightly.20260719.a1b2c3d";
const nativeNames = [
  "@kimohy/colay-darwin-arm64",
  "@kimohy/colay-linux-x64",
  "@kimohy/colay-win32-x64",
];
const rootName = "@kimohy/colay";

async function tarballs() {
  const directory = await mkdtemp(join(tmpdir(), "colay-publish-"));
  const records = [...nativeNames, rootName].map((name) => ({
    name,
    version,
    filename: `${name.replace("@kimohy/", "").replaceAll("/", "-")}-${version}.tgz`,
    integrity: `sha512-${name}`,
  }));
  await writeFile(join(directory, "npm-pack.json"), `${JSON.stringify(records)}\n`);
  await Promise.all(records.map((record) => writeFile(join(directory, record.filename), JSON.stringify(record))));
  return { directory, records };
}

function fakeClient({ existing = new Map(), channel = undefined, publishResult = "matching", visibility = new Map() } = {}) {
  const calls = [];
  return {
    calls,
    async viewIntegrity(name, releaseVersion) {
      calls.push(["view", name, releaseVersion]);
      const queued = visibility.get(`${name}@${releaseVersion}`);
      if (queued?.length) return queued.shift();
      return existing.get(`${name}@${releaseVersion}`);
    },
    async publish(tarball, tag) {
      const record = JSON.parse(await readFile(tarball, "utf8"));
      calls.push(["publish", record.name, tag]);
      return publishResult === "matching" ? { integrity: record.integrity } : publishResult;
    },
    async viewChannelVersion(name, tag) {
      calls.push(["channel", name, tag]);
      return channel;
    },
  };
}

async function releaseWith(client, overrides = {}) {
  const fixture = await tarballs();
  const result = await publishRelease({
    tarballsDir: fixture.directory,
    version,
    distTag: "nightly",
    npmClient: client,
    retryDelay: async () => {},
    ...overrides,
  });
  return { ...fixture, result };
}

test("publishes a new release in lexical native order before the root channel", async () => {
  const fixture = await tarballs();
  const calls = [];
  const client = {
    async viewIntegrity(name, releaseVersion) {
      calls.push(["view", name, releaseVersion]);
      return undefined;
    },
    async publish(tarball, tag) {
      const record = JSON.parse(await (await import("node:fs/promises")).readFile(tarball, "utf8"));
      calls.push(["publish", record.name, tag]);
      return { integrity: record.integrity };
    },
    async viewChannelVersion(name, tag) {
      calls.push(["channel", name, tag]);
      return version;
    },
  };

  await publishRelease({ tarballsDir: fixture.directory, version, distTag: "nightly", npmClient: client, retryDelay: async () => {} });
  assert.deepEqual(calls, [
    ["view", "@kimohy/colay-darwin-arm64", version],
    ["publish", "@kimohy/colay-darwin-arm64", "colay-candidate"],
    ["view", "@kimohy/colay-linux-x64", version],
    ["publish", "@kimohy/colay-linux-x64", "colay-candidate"],
    ["view", "@kimohy/colay-win32-x64", version],
    ["publish", "@kimohy/colay-win32-x64", "colay-candidate"],
    ["view", "@kimohy/colay", version],
    ["publish", "@kimohy/colay", "nightly"],
    ["channel", "@kimohy/colay", "nightly"],
  ]);
});

test("skips an immutable existing package with matching integrity", async () => {
  const existing = new Map([[`@kimohy/colay-darwin-arm64@${version}`, "sha512-@kimohy/colay-darwin-arm64"]]);
  const client = fakeClient({ existing, channel: version });
  await releaseWith(client);
  assert.equal(client.calls.some(([kind, name]) => kind === "publish" && name === "@kimohy/colay-darwin-arm64"), false);
});

test("rejects an existing package with different integrity before root publication", async () => {
  const existing = new Map([[`@kimohy/colay-linux-x64@${version}`, "sha512-wrong"]]);
  const client = fakeClient({ existing, channel: version });
  await assert.rejects(releaseWith(client), /integrity mismatch for @kimohy\/colay-linux-x64@0\.1\.1-nightly/);
  assert.equal(client.calls.some(([kind, name]) => kind === "publish" && name === rootName), false);
});

test("does not publish the root after a native publish failure", async () => {
  const fixture = await tarballs();
  const client = {
    async viewIntegrity() { return undefined; },
    async publish(tarball) {
      const record = JSON.parse(await (await import("node:fs/promises")).readFile(tarball, "utf8"));
      if (record.name === "@kimohy/colay-linux-x64") throw new Error("native publish failed");
      return { integrity: record.integrity };
    },
    async viewChannelVersion() { return version; },
  };
  await assert.rejects(
    publishRelease({ tarballsDir: fixture.directory, version, distTag: "nightly", npmClient: client, retryDelay: async () => {} }),
    /native publish failed/,
  );
});

test("allows only the three public release tags", async () => {
  const client = fakeClient({ channel: version });
  await assert.rejects(releaseWith(client, { distTag: "candidate" }), /distTag must be one of nightly, beta, latest/);
});

test("reports manual root-channel recovery instead of mutating a dist-tag", async () => {
  const client = fakeClient({ channel: "0.1.0" });
  await assert.rejects(
    releaseWith(client),
    new RegExp(`npm dist-tag add @kimohy/colay@${version} nightly`),
  );
  assert.equal(client.calls.some(([kind]) => kind === "dist-tag"), false);
});

test("retries registry integrity visibility at most six times and identifies the missing package", async () => {
  const fixture = await tarballs();
  const visibility = new Map([[`@kimohy/colay-darwin-arm64@${version}`, [undefined, undefined, undefined, undefined, undefined, undefined, undefined]]]);
  let waits = 0;
  const client = fakeClient({ visibility, channel: version, publishResult: null });
  await assert.rejects(
    publishRelease({
      tarballsDir: fixture.directory,
      version,
      distTag: "nightly",
      npmClient: client,
      retryDelay: async () => { waits += 1; },
    }),
    /@kimohy\/colay-darwin-arm64@0\.1\.1-nightly.*did not become visible/,
  );
  assert.equal(client.calls.filter(([kind, name]) => kind === "view" && name === "@kimohy/colay-darwin-arm64").length, 8);
  assert.equal(waits, 6);
});
