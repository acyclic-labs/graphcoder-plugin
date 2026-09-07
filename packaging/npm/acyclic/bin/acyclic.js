#!/usr/bin/env node
// Thin launcher: resolves the platform package that carries the real static
// binary (fs engine compiled in) and exec-replaces into it. The npm layer
// adds no runtime — node exits as soon as the binary takes over.
"use strict";
const { execFileSync, spawnSync } = require("node:child_process");

const PLATFORMS = {
  "darwin arm64": "@acyclic-labs/plugin-darwin-arm64",
  "darwin x64": "@acyclic-labs/plugin-darwin-x64",
  "linux x64": "@acyclic-labs/plugin-linux-x64",
  "linux arm64": "@acyclic-labs/plugin-linux-arm64",
};

const key = `${process.platform} ${process.arch}`;
const pkg = PLATFORMS[key];
if (!pkg) {
  console.error(`acyclic: unsupported platform ${key}`);
  process.exit(1);
}

let binary;
try {
  binary = require.resolve(`${pkg}/bin/acyclic`);
} catch {
  console.error(
    `acyclic: platform package ${pkg} is not installed.\n` +
      "Your package manager likely skipped optional dependencies " +
      "(--no-optional / --omit=optional). Reinstall without that flag.",
  );
  process.exit(1);
}

const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
if (result.error) {
  console.error(`acyclic: ${result.error.message}`);
  process.exit(1);
}
process.exit(result.status ?? 1);
