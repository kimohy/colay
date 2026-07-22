"use strict";

const { spawn: defaultSpawn } = require("node:child_process");
const path = require("node:path");

const PLATFORM_PACKAGES = Object.freeze({
  "win32/x64": Object.freeze({
    packageName: "@kimohy/colay-win32-x64",
    binary: "bin/colay.exe",
  }),
  "darwin/arm64": Object.freeze({
    packageName: "@kimohy/colay-darwin-arm64",
    binary: "bin/colay",
  }),
  "linux/x64": Object.freeze({
    packageName: "@kimohy/colay-linux-x64",
    binary: "bin/colay",
  }),
});

const MINIMUM_NODE_MAJOR = 22;

function assertSupportedNodeVersion(nodeVersion = process.versions.node) {
  const major = Number.parseInt(String(nodeVersion).split(".", 1)[0], 10);
  if (!Number.isSafeInteger(major) || major < MINIMUM_NODE_MAJOR) {
    throw new Error(
      `Node.js ${nodeVersion} is unsupported by Colay; install Node.js 22 or newer and ensure it is first on PATH (for NVM: nvm install 22 && nvm use 22)`,
    );
  }
}

function platformPackage(platform, arch) {
  const descriptor = PLATFORM_PACKAGES[`${platform}/${arch}`];
  if (!descriptor) {
    throw new Error(
      `unsupported platform ${platform}/${arch}; supported: win32/x64, darwin/arm64, linux/x64`,
    );
  }
  return descriptor;
}

function resolveNativeBinary({
  platform = process.platform,
  arch = process.arch,
  resolvePackage = require.resolve,
} = {}) {
  const descriptor = platformPackage(platform, arch);
  try {
    const manifest = resolvePackage(`${descriptor.packageName}/package.json`);
    return path.join(path.dirname(manifest), descriptor.binary);
  } catch (error) {
    throw new Error(
      `missing optional package ${descriptor.packageName}; reinstall with npm install --global @kimohy/colay or use https://github.com/kimohy/colay/releases`,
      { cause: error },
    );
  }
}

function launchNative({
  args = [],
  binary,
  nodeVersion = process.versions.node,
  spawn = defaultSpawn,
  processObject = process,
  signalNames = ["SIGINT", "SIGTERM", "SIGHUP"],
  ...resolutionOptions
} = {}) {
  return new Promise((resolve, reject) => {
    try {
      assertSupportedNodeVersion(nodeVersion);
    } catch (error) {
      reject(error);
      return;
    }
    const executable = binary ?? resolveNativeBinary(resolutionOptions);
    let child;
    try {
      child = spawn(executable, args, { shell: false, stdio: "inherit" });
    } catch (error) {
      reject(error);
      return;
    }

    const signalHandlers = [];
    const removeSignalHandlers = () => {
      for (const [signal, handler] of signalHandlers) {
        processObject.removeListener(signal, handler);
      }
    };
    const settle = (handler) => (value, signal) => {
      removeSignalHandlers();
      handler(value, signal);
    };

    for (const signal of signalNames) {
      const handler = () => child.kill(signal);
      try {
        processObject.on(signal, handler);
        signalHandlers.push([signal, handler]);
      } catch {
        // Hosts may not support every signal; only retain registered handlers.
      }
    }

    child.once(
      "error",
      settle((error) => reject(error)),
    );
    child.once(
      "exit",
      settle((code, signal) => resolve({ code, signal })),
    );
  });
}

module.exports = {
  assertSupportedNodeVersion,
  launchNative,
  platformPackage,
  resolveNativeBinary,
};
