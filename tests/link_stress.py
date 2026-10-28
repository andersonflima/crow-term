#!/usr/bin/env python3
"""The daemon link under load: eight panes flooding output must not slow the
link to the point of a disconnect, and typing elsewhere must stay responsive.

Before the fix, a multi-pane snapshot crossed the Unix socket in ~8 KiB pieces
with a 1 ms sleep each: 8 x 200x200 panes took ~2.5 s per Watch (release build,
macOS), close to the GUI's 5 s deadline on a loaded machine.
"""
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import daemon_e2e  # noqa: E402

PANES = 8
SECONDS = float(os.environ.get("CROW_STRESS_SECONDS", "12"))
# For an optimized build (CI: `cargo build --profile stress`). Before the fix
# such a build needed ~2.4 s per snapshot here; after it, ~0.03-0.05 s locally.
# A debug build is ~20x slower in snapshot code: set CROW_STRESS_SNAPSHOT_P99.
SNAPSHOT_P99 = float(os.environ.get("CROW_STRESS_SNAPSHOT_P99", "0.5"))
ECHO_P99 = 0.2
FLOOD = "import sys\nline = 'crow flood ' * 15 + '\\n'\nwhile True:\n    sys.stdout.write(line)\n"


def p99(samples):
    ordered = sorted(samples)
    return ordered[min(len(ordered) - 1, int(len(ordered) * 0.99))]


class LinkUnderLoad(daemon_e2e.DaemonHarness):
    def run_python(self, pane, script):
        python = sys.executable
        command = f"& {python} {script}" if os.name == "nt" else f"{shlex.quote(python)} {shlex.quote(str(script))}"
        self.send(pane, command)

    def test_snapshots_and_typing_stay_fast_while_eight_panes_flood(self):
        script = self.runtime / "flood.py"
        script.write_text(FLOOD)
        first = self.rpc("new-session", "flood")["Created"]
        panes = [first["pane"]]
        for _ in range(PANES - 1):
            panes.append(self.rpc("--session", "flood", "split-pane", panes[0], "--window", first["window"])["Created"]["pane"])
        for pane in panes:
            self.command("resize-pane", pane, "--rows", 200, "--cols", 200)
            self.run_python(pane, script)
        self.eventually(lambda: "crow flood" in self.capture(panes[-1]), timeout=20)
        echo = self.rpc("new-session", "echo")["Created"]["pane"]
        self.eventually(lambda: self.capture(echo).strip(), timeout=20)  # prompt drawn

        snapshots, errors = [], []
        until = time.monotonic() + SECONDS
        while time.monotonic() < until:
            started = time.monotonic()
            result = self.command("capture-pane", panes[0], check=False)
            snapshots.append(time.monotonic() - started)
            if result.returncode:
                errors.append(result.stderr.strip()[:300])

        echoes = []
        for index in range(20):
            token = f"crowkey{index:02d}"
            started = time.monotonic()
            self.command("send-keys", echo, token)
            self.eventually(lambda: token in self.capture(echo), timeout=10)
            echoes.append(time.monotonic() - started)
            # Erase the typed line (keeps tokens from wrapping across rows).
            self.command("send-keys", echo, "\x15" if os.name != "nt" else "\x1b")
            self.eventually(lambda: token not in self.capture(echo), timeout=10)

        report = (f"{len(snapshots)} snapshots of {PANES} flooding panes: p50 {sorted(snapshots)[len(snapshots) // 2]:.3f} s, "
                  f"p99 {p99(snapshots):.3f} s, max {max(snapshots):.3f} s; typing echo p99 {p99(echoes):.3f} s, "
                  f"max {max(echoes):.3f} s; errors {len(errors)}")
        print(report, file=sys.stderr)
        self.assertEqual(errors, [], report)
        self.assertLess(p99(snapshots), SNAPSHOT_P99, report)
        self.assertLess(p99(echoes), ECHO_P99, report)


if __name__ == "__main__":
    unittest.main(verbosity=2)
