"""X11 driver: xdotool (XTEST input), wmctrl (EWMH close), xclip, ImageMagick.

Expects DISPLAY to point at a running X server with an EWMH window manager
(CI: Xvfb + openbox). Input goes through XTEST, i.e. the same path as a real
keyboard/mouse for the client; nothing is injected into the PTY.
"""
from __future__ import annotations

import os
import shutil
import subprocess
import time

from . import Driver


def run(*args, timeout=15, check=True, input=None):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True, timeout=timeout, input=input)
    if check and result.returncode:
        raise AssertionError(f"{args[0]} {args[1:]} failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout.strip()


# xdotool keysym names for the chords the journeys use.
KEYSYMS = {"slash": "slash", "Return": "Return", "Escape": "Escape"}


class X11(Driver):
    name = "x11"
    caps = {"keys", "mouse", "resize", "clipboard", "focus", "close", "screenshot"}

    def __init__(self):
        if not os.environ.get("DISPLAY"):
            raise SystemExit("x11 driver needs DISPLAY")
        for tool in ("xdotool", "wmctrl", "xclip", "import"):
            if not shutil.which(tool):
                raise SystemExit(f"x11 driver needs {tool}")
        self.windows = {}
        self.helpers = []

    def environment(self):
        return {
            "display": os.environ.get("DISPLAY"),
            "xdotool": run("xdotool", "version", check=False),
            "wm": run("wmctrl", "-m", check=False).splitlines()[:1],
        }

    def attach(self, gui, timeout=20):
        if gui.name in self.windows:
            return self.windows[gui.name]
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            out = run("xdotool", "search", "--onlyvisible", "--pid", gui.process.pid, check=False)
            ids = [line for line in out.splitlines() if line.strip()]
            if not ids:
                # Without _NET_WM_PID, fall back to the newest window of the class.
                known = set(self.windows.values())
                out = run("xdotool", "search", "--onlyvisible", "--class", "crow-term", check=False)
                ids = [line for line in out.splitlines() if line.strip() and line not in known]
            if ids:
                self.windows[gui.name] = ids[-1]
                self.activate(gui)
                return ids[-1]
            time.sleep(0.2)
        raise AssertionError(f"no X11 window for pid {gui.process.pid}")

    def activate(self, gui):
        window = self.windows.get(gui.name) or self.attach(gui)
        run("xdotool", "windowactivate", "--sync", window, check=False)
        run("xdotool", "windowfocus", "--sync", window, check=False)
        return window

    def key(self, gui, chord):
        self.activate(gui)
        parts = chord.split("+")
        parts[-1] = KEYSYMS.get(parts[-1], parts[-1])
        run("xdotool", "key", "--clearmodifiers", "+".join(parts))
        time.sleep(0.05)

    def type(self, gui, text):
        self.activate(gui)
        run("xdotool", "type", "--clearmodifiers", "--delay", "30", text)
        time.sleep(0.05)

    def click(self, gui, x, y):
        window = self.activate(gui)
        run("xdotool", "mousemove", "--window", window, "--sync", int(x), int(y))
        run("xdotool", "click", "1")

    def resize(self, gui, width, height):
        window = self.activate(gui)
        run("xdotool", "windowsize", "--sync", window, int(width), int(height), check=False)

    def set_clipboard(self, text):
        # xclip forks and keeps serving the selection; it exits when replaced.
        # -quiet keeps xclip in the foreground serving the selection, so the driver
        # owns it and stops it at shutdown (no orphaned clipboard server).
        self.stop_clipboard()
        holder = subprocess.Popen(["xclip", "-quiet", "-selection", "clipboard", "-i"], stdin=subprocess.PIPE,
                                  stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        holder.stdin.write(text.encode())
        holder.stdin.close()
        self.helpers.append(("clipboard", holder))
        time.sleep(0.2)

    def stop_clipboard(self):
        for name, process in [h for h in self.helpers if h[0] == "clipboard"]:
            process.terminate()
            process.wait(timeout=5)
            self.helpers.remove((name, process))

    def focus_away(self, gui):
        if not any(name == "sink" for name, _ in self.helpers):
            sink = subprocess.Popen(["xmessage", "-name", "xmessage", "-geometry", "+900+600", "crow focus sink"])
            self.helpers.append(("sink", sink))
            until = time.monotonic() + 10
            while time.monotonic() < until:
                # xmessage sets no _NET_WM_PID: find it by its window name.
                ids = run("xdotool", "search", "--onlyvisible", "--name", "^xmessage$", check=False).split()
                if ids:
                    self.windows["sink"] = ids[-1]
                    break
                time.sleep(0.2)
            else:
                raise AssertionError("focus sink window (xmessage) did not appear")
        run("xdotool", "windowactivate", "--sync", self.windows["sink"], check=False)
        run("xdotool", "windowfocus", "--sync", self.windows["sink"], check=False)

    def focus_back(self, gui):
        self.activate(gui)

    def close(self, gui):
        window = self.windows.pop(gui.name, None) or self.attach(gui)
        self.windows.pop(gui.name, None)
        run("wmctrl", "-i", "-c", hex(int(window)))

    def screenshot(self, path):
        # PNG24 forces 8-bit RGB (ImageMagick would otherwise pick a palette PNG).
        return subprocess.run(["import", "-window", "root", f"PNG24:{path}"], capture_output=True, timeout=30).returncode == 0

    def shutdown(self):
        for _, process in self.helpers:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)
        self.helpers.clear()


DRIVER = X11
