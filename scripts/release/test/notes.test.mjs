import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

import { renderReleaseNotes } from "../notes.mjs";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

test("release notes derive every current limitation and compatibility obligation from the guide", async () => {
  const [guide, manifestText] = await Promise.all([
    readFile(resolve(repositoryRoot, "docs/release.md"), "utf8"),
    readFile(resolve(repositoryRoot, "scripts/release/test/fixtures/release-manifest.json"), "utf8"),
  ]);
  const notes = renderReleaseNotes({ guide, manifest: JSON.parse(manifestText) });

  assert.match(notes, /Supported Codex versions: 0\.144\.4, 0\.144\.5/);
  assert.match(notes, /Recommended Codex version: 0\.144\.5/);
  assert.match(notes, /State schema 3 and config schema 4/);
  assert.match(notes, /codex exec --json/);
  assert.match(notes, /sensitive local source artifacts/);
  assert.match(notes, /concurrent read-only fan-out is not implemented/);
  assert.match(notes, /release manifest, explicit approval, recovery journal, and restart procedure/);
});
