#!/usr/bin/env node
// Launcher for the npm distribution of Crow Term. The native binary comes from
// the platform package npm installed through optionalDependencies (the
// esbuild/biome pattern): nothing is downloaded at install or run time.
"use strict";

const { spawn } = require("node:child_process");
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
    process.exitCode = 1;
    return;
  }
  const child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
  // A signal sent to the launcher alone (a supervisor, `timeout`, an IDE) must
  // reach the native process instead of orphaning it.
  const forward = (signal) => {
    if (child.exitCode === null && child.signalCode === null) child.kill(signal);
  };
  const signals = process.platform === "win32" ? ["SIGINT", "SIGTERM"] : ["SIGINT", "SIGTERM", "SIGHUP"];
  for (const signal of signals) process.on(signal, forward);
  child.on("error", (error) => {
    process.stderr.write(`crow-term: cannot run ${binary}: ${error.message}\n`);
    process.exitCode = 1;
  });
  child.on("exit", (code, signal) => {
    for (const name of signals) process.removeListener(name, forward);
    if (signal) {
      // Re-raise so the caller sees the same signal.
      process.kill(process.pid, signal);
      return;
    }
    process.exitCode = code ?? 1;
  });
}

main();
