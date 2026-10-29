"""Windows driver: a small C# helper (wingui.cs, built once with the .NET
Framework csc) for SendInput keys/mouse, SetWindowPos resize, WM_CLOSE,
clipboard and a virtual-screen capture. Requires an interactive desktop
(GitHub-hosted Windows runners provide one).
"""
from __future__ import annotations

import base64
import os
import shutil
from pathlib import Path
import subprocess
import tempfile
import time

from . import Driver

HERE = Path(__file__).resolve().parent
CSC = Path(os.environ.get("WINDIR", r"C:\Windows")) / "Microsoft.NET" / "Framework64" / "v4.0.30319" / "csc.exe"


class Windows(Driver):
    name = "windows"
    caps = {"keys", "mouse", "resize", "clipboard", "focus", "close", "screenshot"}

    def __init__(self):
        self.build = Path(tempfile.mkdtemp(prefix="wingui-"))
        self.exe = self.build / "wingui.exe"
        result = subprocess.run(
            [str(CSC), "/nologo", "/optimize", "/target:exe", f"/out:{self.exe}",
             "/r:System.Windows.Forms.dll", "/r:System.Drawing.dll", str(HERE / "wingui.cs")],
            capture_output=True, text=True, timeout=120)
        if result.returncode:
            raise SystemExit(f"csc failed: {result.stdout}{result.stderr}")
        self.windows = {}
        self.sink = None

    def helper(self, *args, check=True, timeout=30):
        result = subprocess.run([str(self.exe), *map(str, args)], capture_output=True, text=True, timeout=timeout)
        if check and result.returncode:
            raise AssertionError(f"wingui {args[0]} failed ({result.returncode}): {result.stderr.strip()}")
        return result.stdout.strip()

    def environment(self):
        return {"desktop": self.helper("info", check=False)}

    def hwnd(self, gui):
        return self.windows.get(gui.name) or self.attach(gui)

    def attach(self, gui, timeout=20):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            out = self.helper("find", gui.process.pid, check=False)
            if out:
                self.windows[gui.name] = out.splitlines()[0]
                self.helper("activate", self.windows[gui.name])
                return self.windows[gui.name]
            time.sleep(0.2)
        raise AssertionError(f"no window for pid {gui.process.pid}")

    def activate(self, gui):
        self.helper("activate", self.hwnd(gui))

    def key(self, gui, chord):
        self.activate(gui)
        self.helper("key", chord)

    def type(self, gui, text):
        self.activate(gui)
        self.helper("type", base64.b64encode(text.encode()).decode())

    def click(self, gui, x, y):
        self.activate(gui)
        self.helper("click", self.hwnd(gui), int(x), int(y))

    def move(self, gui, x, y):
        self.activate(gui)
        self.helper("move", self.hwnd(gui), int(x), int(y))

    def modifier(self, gui, name, down):
        self.activate(gui)
        self.helper("keydown" if down else "keyup", name)

    def resize(self, gui, width, height):
        self.helper("resize", self.hwnd(gui), int(width), int(height))

    def set_clipboard(self, text):
        # Through a file: a command line holds at most 32K characters.
        path = self.build / "clipboard.txt"
        path.write_text(text, encoding="utf-8", newline="")
        self.helper("clipboard-file", path)

    def focus_away(self, gui):
        if self.sink is None:
            self.sink = subprocess.Popen([str(self.exe), "sink"])
            until = time.monotonic() + 15
            while time.monotonic() < until and "sink" not in self.windows:
                out = self.helper("find", self.sink.pid, check=False)
                if out:
                    self.windows["sink"] = out.splitlines()[0]
                time.sleep(0.2)
        self.helper("activate", self.windows["sink"])

    def focus_back(self, gui):
        self.activate(gui)

    def close(self, gui):
        hwnd = self.windows.pop(gui.name, None) or self.attach(gui)
        self.windows.pop(gui.name, None)
        self.helper("close", hwnd)

    def screenshot(self, path):
        return subprocess.run([str(self.exe), "screenshot", str(path)], capture_output=True, timeout=60).returncode == 0

    def shutdown(self):
        if self.sink is not None:
            self.sink.terminate()
            self.sink.wait(timeout=10)
        shutil.rmtree(self.build, ignore_errors=True)


DRIVER = Windows
