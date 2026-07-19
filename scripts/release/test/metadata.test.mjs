import assert from "node:assert/strict";
import { mkdtemp, readFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

import { classifyRelease, main } from "../metadata.mjs";

const sha = "a1b2c3d456789012345678901234567890123456";
const now = new Date("2026-07-19T12:00:00Z");

test("classifyRelease derives each supported channel", () => {
  const cases = [
    {
      input: {
        ref: "refs/heads/main",
        sha,
        now,
        workspaceVersion: "0.1.0",
        templateVersion: "0.1.0",
      },
      expected: {
        channel: "nightly",
        version: "0.1.1-nightly.20260719.a1b2c3d",
        distTag: "nightly",
        githubMode: "artifact",
        retentionDays: 14,
      },
    },
    {
      input: {
        ref: "refs/tags/v0.2.0-beta.3",
        sha,
        now,
        workspaceVersion: "0.2.0-beta.3",
        templateVersion: "0.2.0-beta.3",
      },
      expected: {
        channel: "beta",
        version: "0.2.0-beta.3",
        distTag: "beta",
        githubMode: "prerelease",
        retentionDays: null,
      },
    },
    {
      input: {
        ref: "refs/tags/v0.2.0",
        sha,
        now,
        workspaceVersion: "0.2.0",
        templateVersion: "0.2.0",
      },
      expected: {
        channel: "stable",
        version: "0.2.0",
        distTag: "latest",
        githubMode: "release",
        retentionDays: null,
      },
    },
  ];

  for (const { input, expected } of cases) {
    assert.deepEqual(classifyRelease(input), expected);
  }
});

test("classifyRelease replaces a prerelease with a nightly prerelease on main", () => {
  assert.equal(
    classifyRelease({
      ref: "refs/heads/main",
      sha,
      now,
      workspaceVersion: "0.2.0-beta.3",
      templateVersion: "0.2.0-beta.3",
    }).version,
    "0.2.0-nightly.20260719.a1b2c3d",
  );
});

test("classifyRelease prefixes an all-decimal short SHA so its SemVer identifier has no leading zero", () => {
  assert.equal(
    classifyRelease({
      ref: "refs/heads/main",
      sha: "0123456789abcdef0123456789abcdef01234567",
      now,
      workspaceVersion: "0.2.0",
      templateVersion: "0.2.0",
    }).version,
    "0.2.1-nightly.20260719.g0123456",
  );
});

test("classifyRelease increments an arbitrarily large stable patch without precision loss", () => {
  assert.equal(
    classifyRelease({
      ref: "refs/heads/main",
      sha,
      now,
      workspaceVersion: "0.2.9007199254740993",
      templateVersion: "0.2.9007199254740993",
    }).version,
    "0.2.9007199254740994-nightly.20260719.a1b2c3d",
  );
});

test("classifyRelease rejects unsupported refs and invalid immutable inputs", () => {
  const base = {
    ref: "refs/heads/main",
    sha,
    now,
    workspaceVersion: "0.1.0",
    templateVersion: "0.1.0",
  };

  for (const input of [
    { ...base, ref: "refs/heads/feature" },
    { ...base, ref: "refs/tags/v0.1.0-rc.1" },
    { ...base, sha: sha.toUpperCase() },
    { ...base, sha: sha.slice(0, 39) },
    { ...base, now: new Date("invalid") },
    { ...base, ref: "refs/tags/v0.1.0", workspaceVersion: "0.1.1" },
    { ...base, ref: "refs/tags/v0.1.0", templateVersion: "0.1.1" },
  ]) {
    assert.throws(() => classifyRelease(input));
  }
});

test("metadata CLI emits JSON and appends GitHub output without replacing existing entries", async () => {
  const directory = await mkdtemp(join(tmpdir(), "colay-release-metadata-"));
  const output = join(directory, "github-output.txt");
  const writes = [];
  const log = (value) => writes.push(value);

  await main([
    "--ref", "refs/heads/main",
    "--sha", sha,
    "--now", "2026-07-19T12:00:00Z",
    "--github-output", output,
  ], { log });
  await main([
    "--ref", "refs/heads/main",
    "--sha", sha,
    "--now", "2026-07-19T12:00:00Z",
    "--github-output", output,
  ], { log });

  assert.deepEqual(JSON.parse(writes[0]), {
    channel: "nightly",
    version: "0.1.1-nightly.20260719.a1b2c3d",
    distTag: "nightly",
    githubMode: "artifact",
    retentionDays: 14,
  });
  const lines = (await readFile(output, "utf8")).trim().split("\n");
  assert.deepEqual(lines.slice(0, 5), [
    "channel=nightly",
    "version=0.1.1-nightly.20260719.a1b2c3d",
    "dist_tag=nightly",
    "github_mode=artifact",
    "retention_days=14",
  ]);
  assert.equal(lines.length, 10);
});
