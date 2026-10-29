#!/usr/bin/env node
// Launcher for the npm distribution of Crow Term. The native binary comes from
// the platform package npm installed through optionalDependencies (the
// esbuild/biome pattern): nothing is downloaded at install or run time.
"use strict";

const { spawnSync } = require("node:child_process");
const path = require("node:path");

// `${process.platform} ${process.arch}` -> [package, binary path inside it]
const TARGETS = {
  "darwin arm64": ["@andespindola/crow-term-darwin-arm64", "bin/Crow Term.app/Contents/MacOS/crow-term"],
  "darwin x64": ["@andespindola/crow-term-darwin-x64", "bin/Crow Term.app/Contents/MacOS/crow-term"],
  "linux arm64": ["@andespindola/crow-term-linux-arm64", "bin/crow-term"],
  "linux x64": ["@andespindola/crow-term-linux-x64", "bin/crow-term"],
  "win32 x64": ["@andespindola/crow-term-win32-x64", "bin/crow-term.exe"],
};

function binaryPath() {
  const key = `${process.platform} ${process.arch}`;
  const target = TARGETS[key];
  if (!target) {
    throw new Error(
      `Crow Term has no prebuilt binary for ${key}. Supported: ${Object.keys(TARGETS).join(", ")}.`,
    );
  }
  const [name, relative] = target;
  let root;
  try {
    root = path.dirname(require.resolve(`${name}/package.json`));
  } catch {
    throw new Error(
      `The platform package ${name} is missing. Reinstall without --no-optional / --omit=optional ` +
        `(npm i -g @andespindola/crow-term).`,
    );
  }
  return path.join(root, relative);
}

function main() {
  let binary;
  try {
    binary = binaryPath();
  } catch (error) {
    process.stderr.write(`crow-term: ${error.message}\n`);
    return 1;
  }
  const result = spawnSync(binary, process.argv.slice(2), { stdio: "inherit" });
  if (result.error) {
    process.stderr.write(`crow-term: cannot run ${binary}: ${result.error.message}\n`);
    return 1;
  }
  if (result.signal) {
    // Re-raise so the caller sees the same signal (e.g. Ctrl-C in a script).
    process.kill(process.pid, result.signal);
    return 1;
  }
  return result.status ?? 1;
}

process.exitCode = main();
