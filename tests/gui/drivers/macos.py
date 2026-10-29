"""macOS driver: System Events (osascript) for keys, window size, close and
focus; Quartz events (CGEventPost via ctypes) for the mouse; pbcopy for the
clipboard; screencapture for screenshots.

All of that needs the Accessibility permission. GitHub's macOS images grant it
to the runner's tools (their TCC database is preconfigured); on a machine
without it, `UI elements enabled` is false and the driver degrades to
screenshots only, reporting every input journey as skipped with that reason.
"""
from __future__ import annotations

import ctypes
import ctypes.util
import json
import subprocess
import time

from . import Driver

NO_AX = "no Accessibility permission (System Events reports UI elements disabled): synthetic input and window control are refused"
# US ANSI virtual key codes (Carbon kVK_*), for chords posted as Quartz events.
KEY_CODES = {
    "a": 0, "s": 1, "d": 2, "f": 3, "h": 4, "g": 5, "z": 6, "x": 7, "c": 8, "v": 9, "b": 11, "q": 12,
    "w": 13, "e": 14, "r": 15, "y": 16, "t": 17, "1": 18, "2": 19, "3": 20, "4": 21, "6": 22, "5": 23,
    "=": 24, "9": 25, "7": 26, "-": 27, "8": 28, "0": 29, "]": 30, "o": 31, "u": 32, "[": 33, "i": 34,
    "p": 35, "Return": 36, "l": 37, "j": 38, "'": 39, "k": 40, ";": 41, "\\": 42, ",": 43, "slash": 44,
    "/": 44, "n": 45, "m": 46, ".": 47, "Tab": 48, "space": 49, "`": 50, "BackSpace": 51, "Escape": 53,
}
# Modifier key codes and their event flags (a real keyboard sends the modifier's
# own key event first; System Events' `using ... down` only sets flags).
MODIFIERS = {"super": (55, 0x100000), "shift": (56, 0x20000), "alt": (58, 0x80000), "ctrl": (59, 0x40000)}


def osascript(script, timeout=30, check=True):
    result = subprocess.run(["osascript", "-e", script], capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise AssertionError(f"osascript failed ({result.returncode}): {result.stderr.strip()}")
    return result.stdout.strip()


def quote(text):
    return json.dumps(text)  # AppleScript string literals accept the same escapes for ASCII


class CGPoint(ctypes.Structure):
    _fields_ = [("x", ctypes.c_double), ("y", ctypes.c_double)]


class MacOS(Driver):
    name = "macos"
    paste_chord = "super+v"

    def __init__(self):
        probe = subprocess.run(["osascript", "-e", 'tell application "System Events" to get UI elements enabled'],
                               capture_output=True, text=True, timeout=30)
        self.ax = probe.returncode == 0 and probe.stdout.strip() == "true"
        self.probe = (probe.stdout or probe.stderr).strip()[:200]
        if self.ax:
            self.caps = {"keys", "mouse", "resize", "clipboard", "focus", "close", "screenshot"}
            self.missing = {}
        else:
            self.caps = {"screenshot"}
            self.missing = {cap: NO_AX for cap in ("keys", "mouse", "resize", "focus", "close", "clipboard")}
        self.cg = ctypes.cdll.LoadLibrary(ctypes.util.find_library("CoreGraphics") or
                                          "/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")
        self.cg.CGEventCreateMouseEvent.restype = ctypes.c_void_p
        self.cg.CGEventCreateMouseEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint32, CGPoint, ctypes.c_uint32]
        self.cg.CGEventPost.argtypes = [ctypes.c_uint32, ctypes.c_void_p]
        self.cg.CGEventCreateKeyboardEvent.restype = ctypes.c_void_p
        self.cg.CGEventCreateKeyboardEvent.argtypes = [ctypes.c_void_p, ctypes.c_uint16, ctypes.c_bool]
        self.cg.CGEventSetFlags.argtypes = [ctypes.c_void_p, ctypes.c_uint64]
        self.cf = ctypes.cdll.LoadLibrary(ctypes.util.find_library("CoreFoundation"))
        self.cf.CFRelease.argtypes = [ctypes.c_void_p]

    def environment(self):
        return {"ui_elements_enabled": self.ax, "probe": self.probe,
                "macos": subprocess.run(["sw_vers", "-productVersion"], capture_output=True, text=True).stdout.strip()}

    def process(self, gui):
        return f"(first process whose unix id is {gui.process.pid})"

    def attach(self, gui, timeout=20):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if not self.ax:
                return
            count = osascript(f'tell application "System Events" to count windows of {self.process(gui)}', check=False)
            if count.isdigit() and int(count) > 0:
                self.focus_back(gui)
                return
            time.sleep(0.2)
        raise AssertionError(f"no window for pid {gui.process.pid}")

    def focus_back(self, gui):
        osascript(f'tell application "System Events" to set frontmost of {self.process(gui)} to true')
        time.sleep(0.2)

    def post_key(self, code, down, flags):
        event = self.cg.CGEventCreateKeyboardEvent(None, code, down)
        self.cg.CGEventSetFlags(event, flags)
        self.cg.CGEventPost(0, event)
        self.cf.CFRelease(event)
        time.sleep(0.03)

    def key(self, gui, chord):
        """Chords as Quartz key events, modifier keys included, like hardware."""
        self.focus_back(gui)
        parts = chord.split("+")
        if parts[-1] not in KEY_CODES:
            raise AssertionError(f"no key code for {chord}")
        flags = 0
        for name in parts[:-1]:
            code, flag = MODIFIERS[name]
            flags |= flag
            self.post_key(code, True, flags)
        self.post_key(KEY_CODES[parts[-1]], True, flags)
        self.post_key(KEY_CODES[parts[-1]], False, flags)
        for name in reversed(parts[:-1]):
            code, flag = MODIFIERS[name]
            flags &= ~flag
            self.post_key(code, False, flags)
        time.sleep(0.05)

    def type(self, gui, text):
        self.focus_back(gui)
        osascript(f'tell application "System Events" to keystroke {quote(text)}')
        time.sleep(0.05)

    def window_frame(self, gui):
        out = osascript(f'tell application "System Events" to tell {self.process(gui)} to get {{position, size}} of window 1')
        x, y, w, h = (int(float(v)) for v in out.split(", "))
        return x, y, w, h

    def client_point(self, gui, x, y):
        frame = gui.frame()
        if frame is None:
            raise AssertionError("no frame to locate the client area")
        meta = frame[1]
        wx, wy, ww, wh = self.window_frame(gui)
        # The client area is the bottom of the window frame (title bar above it).
        scale = meta["scale"]
        top = wh - meta["height"] / scale
        return CGPoint(wx + x / scale, wy + top + y / scale)

    def mouse(self, kinds, point):
        for kind in kinds:
            event = self.cg.CGEventCreateMouseEvent(None, kind, point, 0)
            # Modifiers held with modifier() apply to the click, as on hardware.
            self.cg.CGEventSetFlags(event, getattr(self, "held", 0))
            self.cg.CGEventPost(0, event)
            self.cf.CFRelease(event)
            time.sleep(0.08)

    def click(self, gui, x, y):
        self.focus_back(gui)
        self.mouse((5, 1, 2), self.client_point(gui, x, y))  # mouse moved, left down, left up

    def move(self, gui, x, y):
        self.focus_back(gui)
        self.mouse((5,), self.client_point(gui, x, y))

    def modifier(self, gui, name, down):
        self.focus_back(gui)
        code, flag = MODIFIERS[name]
        self.held = (getattr(self, "held", 0) | flag) if down else (getattr(self, "held", 0) & ~flag)
        self.post_key(code, down, self.held)

    def resize(self, gui, width, height):
        frame = gui.frame()
        scale = frame[1]["scale"] if frame else 1.0
        _, _, ww, wh = self.window_frame(gui)
        title = wh - frame[1]["height"] / scale if frame else 0
        osascript(f'tell application "System Events" to tell {self.process(gui)} to set size of window 1 to '
                  f'{{{int(width / scale)}, {int(height / scale + title)}}}')

    def set_clipboard(self, text):
        subprocess.run(["pbcopy"], input=text, text=True, check=True, timeout=15)

    def focus_away(self, gui):
        osascript('tell application "Finder" to activate')
        time.sleep(0.3)

    def close(self, gui):
        osascript(f'tell application "System Events" to tell {self.process(gui)} to click '
                  '(first button of window 1 whose subrole is "AXCloseButton")')

    def screenshot(self, path):
        return subprocess.run(["screencapture", "-x", "-t", "png", str(path)], capture_output=True, timeout=60).returncode == 0


DRIVER = MacOS
