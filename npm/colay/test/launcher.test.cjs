"use strict";

const assert = require("node:assert/strict");
const { EventEmitter } = require("node:events");
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");
const { after, test } = require("node:test");

const {
  launchNative,
  platformPackage,
  resolveNativeBinary,
} = require("../lib/launcher.cjs");

const temporaryDirectories = [];

after(() => {
  for (const directory of temporaryDirectories) {
    fs.rmSync(directory, { force: true, recursive: true });
  }
});

function temporaryDirectory() {
  const directory = fs.mkdtempSync(path.join(os.tmpdir(), "colay-launcher-"));
  temporaryDirectories.push(directory);
  return directory;
}

test("maps the three supported platform pairs", () => {
  assert.deepEqual(platformPackage("win32", "x64"), {
    packageName: "@kimohy/colay-win32-x64",
    binary: "bin/colay.exe",
  });
  assert.equal(
    platformPackage("darwin", "arm64").packageName,
    "@kimohy/colay-darwin-arm64",
  );
  assert.equal(
    platformPackage("linux", "x64").packageName,
    "@kimohy/colay-linux-x64",
  );
});

test("reports unsupported platforms without downloading", () => {
  assert.throws(
    () => platformPackage("darwin", "x64"),
    /unsupported platform darwin\/x64.*win32\/x64.*darwin\/arm64.*linux\/x64/s,
  );
});

test("explains how to recover a missing optional package", () => {
  assert.throws(
    () =>
      resolveNativeBinary({
        platform: "linux",
        arch: "x64",
        resolvePackage: () => {
          throw Object.assign(new Error("missing"), { code: "MODULE_NOT_FOUND" });
        },
      }),
    /@kimohy\/colay-linux-x64.*npm install --global @kimohy\/colay.*github.com\/kimohy\/colay\/releases/s,
  );
});

test("preserves native arguments without shell interpretation and returns the exit code", async () => {
  const directory = temporaryDirectory();
  const resultPath = path.join(directory, "result.json");
  const fixturePath = path.join(directory, "fixture.cjs");
  const packageDirectory = path.join(
    directory,
    "node_modules",
    "@kimohy",
    "colay-win32-x64",
  );
  const manifestPath = path.join(packageDirectory, "package.json");
  const nativeBinary = path.join(packageDirectory, "bin", "colay.exe");
  fs.mkdirSync(path.dirname(nativeBinary), { recursive: true });
  fs.writeFileSync(manifestPath, "{}\n");
  fs.copyFileSync(process.execPath, nativeBinary);
  fs.chmodSync(nativeBinary, 0o755);
  fs.writeFileSync(
    fixturePath,
    [
      '"use strict";',
      'const fs = require("node:fs");',
      "fs.writeFileSync(process.argv[2], JSON.stringify(process.argv.slice(3)));",
      "process.exit(37);",
      "",
    ].join("\n"),
  );

  const argumentsToPreserve = ["has spaces", "$(not-a-command)", "; && | < >"];
  const result = await launchNative({
    platform: "win32",
    arch: "x64",
    resolvePackage: (request) => {
      assert.equal(request, "@kimohy/colay-win32-x64/package.json");
      return manifestPath;
    },
    args: [fixturePath, resultPath, ...argumentsToPreserve],
    signalNames: [],
  });

  assert.deepEqual(JSON.parse(fs.readFileSync(resultPath, "utf8")), argumentsToPreserve);
  assert.deepEqual(result, { code: 37, signal: null });
});

test("uses separated spawn arguments with shell disabled", async () => {
  const child = new EventEmitter();
  const calls = [];
  const promise = launchNative({
    binary: "native-binary",
    args: ["one arg", "$(not-a-command)"],
    processObject: new EventEmitter(),
    signalNames: [],
    spawn: (...arguments_) => {
      calls.push(arguments_);
      return child;
    },
  });
  child.emit("exit", 0, null);

  assert.deepEqual(await promise, { code: 0, signal: null });
  assert.deepEqual(calls, [
    ["native-binary", ["one arg", "$(not-a-command)"], { shell: false, stdio: "inherit" }],
  ]);
});

test("forwards supported termination signals and removes listeners after exit", async () => {
  const child = new EventEmitter();
  const processObject = new EventEmitter();
  const forwardedSignals = [];
  child.kill = (signal) => forwardedSignals.push(signal);

  const promise = launchNative({
    binary: "native-binary",
    processObject,
    spawn: () => child,
  });

  processObject.emit("SIGINT");
  processObject.emit("SIGTERM");
  processObject.emit("SIGHUP");
  assert.deepEqual(forwardedSignals, ["SIGINT", "SIGTERM", "SIGHUP"]);

  child.emit("exit", 0, null);
  assert.deepEqual(await promise, { code: 0, signal: null });
  assert.equal(processObject.listenerCount("SIGINT"), 0);
  assert.equal(processObject.listenerCount("SIGTERM"), 0);
  assert.equal(processObject.listenerCount("SIGHUP"), 0);
});

test("rejects when spawning the native binary emits an error", async () => {
  const child = new EventEmitter();
  const processObject = new EventEmitter();
  const promise = launchNative({
    binary: "missing-native-binary",
    processObject,
    signalNames: [],
    spawn: () => child,
  });
  const failure = new Error("spawn failed");
  child.emit("error", failure);

  await assert.rejects(promise, failure);
  assert.equal(processObject.listenerCount("SIGINT"), 0);
});
