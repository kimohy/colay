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
  assert.match(workflow, /macos-14/);
  assert.match(workflow, /ubuntu-22\.04/);
  assert.match(workflow, /x86_64-pc-windows-msvc/);
  assert.match(workflow, /aarch64-apple-darwin/);
  assert.match(workflow, /x86_64-unknown-linux-musl/);
  assert.match(workflow, /environment:\s*npm-\$\{\{ needs\.classify\.outputs\.channel \}\}/);
  assert.match(workflow, /id-token: write/);
  assert.match(workflow, /attestations: write/);
  assert.match(workflow, /retention-days:\s*\$\{\{ needs\.classify\.outputs\.retention_days \}\}/);

  const actionPins = [
    "actions/checkout@df4cb1c069e1874edd31b4311f1884172cec0e10",
    "actions/setup-node@249970729cb0ef3589644e2896645e5dc5ba9c38",
    "actions/upload-artifact@ea165f8d65b6e75b540449e92b4886f43607fa02",
    "actions/download-artifact@018cc2cf5baa6db3ef3c5f8a56943fffe632ef53",
    "actions/attest-build-provenance@43d14bc2b83dec42d39ecae14e916627a18bb661",
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
