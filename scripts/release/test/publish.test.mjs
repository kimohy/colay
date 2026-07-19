import assert from "node:assert/strict";
import { createHash } from "node:crypto";
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

function sha512Integrity(name) {
  return `sha512-${createHash("sha512").update(`${name}\n`).digest("base64")}`;
}

async function tarballs() {
  const directory = await mkdtemp(join(tmpdir(), "colay-publish-"));
  const records = [...nativeNames, rootName].map((name) => ({
    name,
    version,
    filename: `${name.replace("@kimohy/", "").replaceAll("/", "-")}-${version}.tgz`,
    integrity: sha512Integrity(name),
  }));
  await Promise.all(records.map((record) => writeFile(join(directory, record.filename), `${record.name}\n`)));
  await writeFile(join(directory, "npm-pack.json"), `${JSON.stringify(records)}\n`);
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
      const name = (await readFile(tarball, "utf8")).trim();
      calls.push(["publish", name, tag]);
      return publishResult === "matching" ? { integrity: sha512Integrity(name) } : publishResult;
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
      const name = (await readFile(tarball, "utf8")).trim();
      calls.push(["publish", name, tag]);
      return { integrity: sha512Integrity(name) };
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
  const existing = new Map([[`@kimohy/colay-darwin-arm64@${version}`, sha512Integrity("@kimohy/colay-darwin-arm64")]]);
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
      const name = (await readFile(tarball, "utf8")).trim();
      if (name === "@kimohy/colay-linux-x64") throw new Error("native publish failed");
      return { integrity: sha512Integrity(name) };
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
  const fixture = await tarballs();
  const existing = new Map(fixture.records.map((record) => [`${record.name}@${version}`, record.integrity]));
  const client = fakeClient({ existing, channel: "0.1.0" });
  await assert.rejects(
    publishRelease({ tarballsDir: fixture.directory, version, distTag: "nightly", npmClient: client, retryDelay: async () => {} }),
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

test("rejects a tampered tarball before any package is published", async () => {
  const fixture = await tarballs();
  const target = fixture.records.find(({ name }) => name === "@kimohy/colay-win32-x64");
  await writeFile(join(fixture.directory, target.filename), "tampered\n");
  const calls = [];
  const client = {
    async viewIntegrity(name) { calls.push(["view", name]); return undefined; },
    async publish() { calls.push(["publish"]); },
    async viewChannelVersion() { return version; },
  };
  await assert.rejects(
    publishRelease({ tarballsDir: fixture.directory, version, distTag: "nightly", npmClient: client, retryDelay: async () => {} }),
    /tarball integrity mismatch for @kimohy\/colay-win32-x64/,
  );
  assert.equal(calls.some(([kind]) => kind === "publish"), false);
});

test("retries a newly published root channel while its previous tag target propagates", async () => {
  const fixture = await tarballs();
  let channelReads = 0;
  let waits = 0;
  const client = {
    async viewIntegrity() { return undefined; },
    async publish(tarball) {
      const record = fixture.records.find(({ filename }) => tarball.endsWith(filename));
      return { integrity: record.integrity };
    },
    async viewChannelVersion() {
      channelReads += 1;
      return channelReads < 3 ? "0.1.0" : version;
    },
  };
  await publishRelease({
    tarballsDir: fixture.directory,
    version,
    distTag: "nightly",
    npmClient: client,
    retryDelay: async () => { waits += 1; },
  });
  assert.equal(channelReads, 3);
  assert.equal(waits, 2);
});

test("requires manual recovery when an existing root remains absent from its public channel", async () => {
  const fixture = await tarballs();
  const existing = new Map(fixture.records.map((record) => [`${record.name}@${version}`, record.integrity]));
  let waits = 0;
  const client = fakeClient({ existing, channel: undefined });
  await assert.rejects(
    publishRelease({
      tarballsDir: fixture.directory,
      version,
      distTag: "nightly",
      npmClient: client,
      retryDelay: async () => { waits += 1; },
    }),
    new RegExp(`npm dist-tag add @kimohy/colay@${version} nightly`),
  );
  assert.equal(client.calls.filter(([kind]) => kind === "channel").length, 7);
  assert.equal(waits, 6);
});
