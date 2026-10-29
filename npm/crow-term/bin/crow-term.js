#!/usr/bin/env node
// Launcher for the npm distribution of Crow Term. The native binary comes from
// the platform package npm installed through optionalDependencies (the
// esbuild/biome pattern): nothing is downloaded at install or run time.
"use strict";

const { spawn } = require("node:child_process");
const path = require("node:path");

// `${platform} ${arch}` -> [package, binary path inside it]
const TARGETS = {
  "darwin arm64": ["@andespindola/crow-term-darwin-arm64", "bin/Crow Term.app/Contents/MacOS/crow-term"],
  "darwin x64": ["@andespindola/crow-term-darwin-x64", "bin/Crow Term.app/Contents/MacOS/crow-term"],
  "linux arm64": ["@andespindola/crow-term-linux-arm64", "bin/crow-term"],
  "linux x64": ["@andespindola/crow-term-linux-x64", "bin/crow-term"],
  "win32 x64": ["@andespindola/crow-term-win32-x64", "bin/crow-term.exe"],
};

/** [package, relative binary path] for a platform/arch, or an Error. */
function target(platform, arch) {
  const key = `${platform} ${arch}`;
  const found = TARGETS[key];
  if (!found) {
    return new Error(
      `Crow Term has no prebuilt binary for ${key}. Supported: ${Object.keys(TARGETS).join(", ")}.`,
    );
  }
  return found;
}

/** -1, 0 or 1 comparing dotted versions numerically ("2.9" < "2.35"). */
function compareVersions(a, b) {
  const left = String(a).split(".").map(Number);
  const right = String(b).split(".").map(Number);
  for (let i = 0; i < Math.max(left.length, right.length); i++) {
    const difference = (left[i] || 0) - (right[i] || 0);
    if (difference) return difference < 0 ? -1 : 1;
  }
  return 0;
}

/**
 * Why the Linux binary cannot run here, or null. `required` is the glibc the
 * binary was built against; `runtime` is the system's glibc version, or
 * undefined on a system without glibc (musl, e.g. Alpine).
 */
function glibcProblem(required, runtime) {
  if (!required) return null;
  if (!runtime) {
    return `this binary needs glibc >= ${required}, and this system has no glibc (musl, e.g. Alpine, is not supported)`;
  }
  if (compareVersions(runtime, required) < 0) {
    return `this binary needs glibc >= ${required}; this system has glibc ${runtime}`;
  }
  return null;
}

function runtimeGlibc() {
  try {
    return process.report.getReport().header.glibcVersionRuntime;
  } catch {
    return undefined;
  }
}

function locate() {
  const found = target(process.platform, process.arch);
  if (found instanceof Error) throw found;
  const [name, relative] = found;
  let manifest;
  try {
    manifest = require.resolve(`${name}/package.json`);
  } catch {
    throw new Error(
      `The platform package ${name} is missing. Reinstall without --no-optional / --omit=optional ` +
        `(npm i -g @andespindola/crow-term).`,
    );
  }
  let glibc;
  try {
    glibc = require(manifest).crowTerm?.glibc;
  } catch {
    glibc = undefined;
  }
  return { binary: path.join(path.dirname(manifest), relative), glibc };
}

function main() {
  let located;
  try {
    located = locate();
  } catch (error) {
    process.stderr.write(`crow-term: ${error.message}\n`);
    process.exitCode = 1;
    return;
  }
  const { binary, glibc } = located;
  if (process.platform === "linux") {
    const problem = glibcProblem(glibc, runtimeGlibc());
    if (problem) {
      process.stderr.write(`crow-term: ${problem}.\n`);
      process.exitCode = 1;
      return;
    }
  }
  // A signal sent to the launcher alone (a supervisor, `timeout`, an IDE) must
  // reach the native process instead of orphaning it. Ctrl-C and a terminal
  // hangup already reach the whole foreground group, so on a TTY they are not
  // forwarded a second time. Handlers go in before spawning: no window in
  // which a signal kills the launcher and orphans the child.
  let child = null;
  const forward = (signal) => {
    if (signal !== "SIGTERM" && process.stdin.isTTY) return;
    if (child && child.exitCode === null && child.signalCode === null) child.kill(signal);
  };
  const signals = process.platform === "win32" ? ["SIGINT", "SIGTERM"] : ["SIGINT", "SIGTERM", "SIGHUP"];
  for (const signal of signals) process.on(signal, forward);
  child = spawn(binary, process.argv.slice(2), { stdio: "inherit" });
  child.on("error", (error) => {
    let hint = "";
    // A missing ELF interpreter (no glibc loader) also surfaces as ENOENT.
    if (process.platform === "linux" && glibc && (error.code === "ENOENT" || error.code === "ENOEXEC")) {
      hint = ` (this binary needs glibc >= ${glibc})`;
    }
    process.stderr.write(`crow-term: cannot run ${binary}: ${error.message}${hint}\n`);
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

module.exports = { TARGETS, target, compareVersions, glibcProblem };

if (require.main === module) main();
