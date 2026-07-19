import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import test from "node:test";
import { fileURLToPath } from "node:url";

const repositoryRoot = resolve(dirname(fileURLToPath(import.meta.url)), "../../..");

test("release workflow uses the reviewed multi-platform channel contract", async () => {
  const workflow = await readFile(resolve(repositoryRoot, ".github/workflows/release.yml"), "utf8");

  assert.match(workflow, /branches:\s*\[main\]/);
  assert.match(workflow, /tags:\s*\["v\*\.\*\.\*"\]/);
  assert.match(workflow, /permissions:\s*\n\s*contents: read/);
  assert.match(workflow, /windows-2022/);
  assert.match(workflow, /macos-15/);
  assert.match(workflow, /ubuntu-22\.04/);
  assert.match(workflow, /x86_64-pc-windows-msvc/);
  assert.match(workflow, /aarch64-apple-darwin/);
  assert.match(workflow, /x86_64-unknown-linux-musl/);
  assert.match(workflow, /environment:\s*npm-\$\{\{ needs\.classify\.outputs\.channel \}\}/);
  assert.match(workflow, /id-token: write/);
  assert.match(workflow, /attestations: write/);
  assert.match(workflow, /retention-days:\s*\$\{\{ needs\.classify\.outputs\.retention_days \}\}/);
  assert.match(workflow, /name:\s*native-\$\{\{ matrix\.target \}\}[\s\S]*?retention-days:\s*\$\{\{ needs\.classify\.outputs\.retention_days \}\}/);

  const releaseDownloads = [...workflow.matchAll(/name:\s*release-\$\{\{ needs\.classify\.outputs\.version \}\}\s*\n\s*path:\s*dist/g)];
  assert.equal(releaseDownloads.length, 4, "each release-bundle consumer must restore the dist root");
  assert.match(workflow, /Expand-Archive[\s\S]*archive-check[\s\S]*Get-FileHash -Algorithm SHA256/);
  assert.match(workflow, /tar -xzf[\s\S]*archive-check[\s\S]*unexpected archived version[\s\S]*sha256sum/);
  assert.equal((workflow.match(/unexpected archived version/g) ?? []).length, 3);
  assert.doesNotMatch(workflow, /chmod\s+\d+\s+"?archive-check/);
  assert.match(workflow, /scripts\/release\/notes\.mjs[\s\S]*docs\/release\.md/);
  assert.match(workflow, /--json tagName,isDraft,isPrerelease,body/);
  assert.match(workflow, /release\.body !== notes/);
  assert.match(workflow, /shopt -s nullglob/);
  assert.match(workflow, /asset_count=.*--json assets/);
  assert.match(workflow, /if \[ "\$asset_count" -gt 0 \]; then/);

  const actionPins = [
    "actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10",
    "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
    "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
    "actions/download-artifact@018cc2cf5baa6db3ef3c5f8a56943fffe632ef53",
    "actions/attest-build-provenance@a2bbfa25375fe432b6a289bc6b6cd05ecd0c4c32",
  ];
  const uses = [...workflow.matchAll(/^\s*-\s*uses:\s*([^\s#]+)\s*$/gm)].map((match) => match[1]);
  assert.ok(uses.length > 0, "workflow must use reviewed GitHub actions");
  for (const action of uses) assert.match(action, /@[0-9a-f]{40}$/);
  for (const pin of actionPins) assert.ok(uses.includes(pin), `missing reviewed action pin ${pin}`);

  assert.match(workflow, /publish-npm:\s*\n[\s\S]*?needs:\s*\[classify, validate, smoke, attest\]/);
  assert.match(workflow, /publish-github:\s*\n[\s\S]*?if:\s*needs\.classify\.outputs\.github_mode != 'artifact'/);
  for (const credential of ["CODEX_API_KEY", "OPENAI_API_KEY", "ANTHROPIC_API_KEY", "GEMINI_API_KEY"]) {
    assert.match(workflow, new RegExp(`${credential}:\\s*""`));
  }
  for (const forbidden of [/codex exec/, /claude /, /gemini /, /npm_token/, /--clobber/, /git push/, /gh pr/]) {
    assert.doesNotMatch(workflow, forbidden);
  }
});
