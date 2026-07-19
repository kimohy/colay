#!/usr/bin/env node
"use strict";

const { launchNative } = require("../lib/launcher.cjs");

launchNative({ args: process.argv.slice(2) })
  .then(({ code, signal }) => {
    if (signal) {
      process.kill(process.pid, signal);
      return;
    }
    process.exitCode = code ?? 1;
  })
  .catch((error) => {
    console.error(`colay: ${error.message}`);
    process.exitCode = 1;
  });
