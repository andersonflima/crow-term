#!/usr/bin/env python3
"""The daemon link under load: eight panes flooding output must not slow the
link to the point of a disconnect, and typing elsewhere must stay responsive.

Before the fix, a multi-pane snapshot crossed the Unix socket in ~8 KiB pieces
with a 1 ms sleep each: 8 x 200x200 panes took ~2.5 s per Watch (release build,
macOS), close to the GUI's 5 s deadline on a loaded machine.
"""
import os
import re
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
# Client-side samples include launching the CLI process, which a loaded CI
# runner occasionally delays (one 0.8 s sample in 86 on macOS, with the daemon
# logging nothing slow): p95 bounds the typical case, max a real stall, and the
# daemon's own log is checked for slow snapshots.
SNAPSHOT_P95 = float(os.environ.get("CROW_STRESS_SNAPSHOT_P95", "0.5"))
SNAPSHOT_MAX = 2.0
# Daemon-side bounds sized for the smallest CI runner (3 vCPUs shared by the
# daemon's 8 parser threads and 8 producers): measured there p50 ~42 ms, max
# ~294 ms; on 12 saturated cores max ~124 ms. The old behaviour this guards
# against was >= 500 ms per Watch in the daemon and seconds on the client,
# against a 5 s deadline that the GUI no longer treats as a disconnect.
DAEMON_P50_MS = 100
DAEMON_SLOW_MS = 500
ECHO_P99 = 0.2
# The producers run at the lowest CPU priority: they model background output,
# and on a 3-core runner eight unthrottled producers would otherwise starve
# every process, the shell being typed into included (measuring the machine,
# not the terminal). The daemon itself runs at normal priority.
FLOOD = """import os, sys
try:
    os.nice(19)
except (AttributeError, OSError):
    try:
        import ctypes
        ctypes.windll.kernel32.SetPriorityClass(ctypes.windll.kernel32.GetCurrentProcess(), 0x40)
    except Exception:
        pass
line = 'crow flood ' * 15 + '\\n'
while True:
    sys.stdout.write(line)
"""


def percentile(samples, fraction):
    ordered = sorted(samples)
    return ordered[min(len(ordered) - 1, int(len(ordered) * fraction))]


def p99(samples):
    return percentile(samples, 0.99)


class LinkUnderLoad(daemon_e2e.DaemonHarness):
    def setUp(self):
        # The daemon logs every Watch snapshot with its duration: the
        # daemon-side cost is asserted, not only the client's wall time.
        os.environ["CROW_TERM_SLOW_MS"] = "0"
        self.addCleanup(os.environ.pop, "CROW_TERM_SLOW_MS", None)
        super().setUp()

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
        # Measure typing, not shell start-up: wait until the shell's prompt is
        # drawn and the screen has been quiet for a second (a login shell may
        # still be reading its profile under this load).
        quiet = {"screen": None, "since": time.monotonic()}

        def ready():
            screen = self.capture(echo)
            if screen != quiet["screen"]:
                quiet.update(screen=screen, since=time.monotonic())
                return False
            return screen.strip() and time.monotonic() - quiet["since"] >= 1.0
        self.eventually(ready, timeout=60)

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

        slowest = sorted(snapshots)[-3:]
        if os.environ.get("CROW_STRESS_SAMPLES"):
            print("samples:", " ".join(f"{x:.3f}" for x in snapshots), file=sys.stderr)
        report = (f"{len(snapshots)} snapshots of {PANES} flooding panes: p50 {sorted(snapshots)[len(snapshots) // 2]:.3f} s, "
                  f"p95 {percentile(snapshots, 0.95):.3f} s, "
                  f"p99 {p99(snapshots):.3f} s, max {max(snapshots):.3f} s; typing echo p99 {p99(echoes):.3f} s, "
                  f"max {max(echoes):.3f} s; errors {len(errors)}")
        print(report, file=sys.stderr)
        self.assertEqual(errors, [], report)
        self.assertLess(percentile(snapshots, 0.95), SNAPSHOT_P95, report)
        self.assertLess(max(snapshots), SNAPSHOT_MAX, report)
        log = (self.runtime / "test-daemon.log").read_text(errors="replace")
        daemon = [int(ms) for ms in re.findall(r"Watch snapshot of 8 pane\(s\) took (\d+) ms", log)]
        self.assertTrue(daemon, "the daemon logged no Watch snapshot timings")
        daemon_report = (f"daemon-side snapshots of 8 panes: {len(daemon)}, p50 {percentile(daemon, 0.5)} ms, "
                         f"p99 {percentile(daemon, 0.99)} ms, max {max(daemon)} ms")
        print(daemon_report, file=sys.stderr)
        self.assertLess(percentile(daemon, 0.5), DAEMON_P50_MS, f"{daemon_report}; {report}")
        self.assertLess(max(daemon), DAEMON_SLOW_MS, f"{daemon_report}; {report}")
        self.assertLess(p99(echoes), ECHO_P99, report)


if __name__ == "__main__":
    unittest.main(verbosity=2)
