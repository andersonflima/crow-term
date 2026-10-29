"""OS drivers for the GUI journeys: synthetic native input, window control and
screenshots. Each driver declares its capabilities; a journey that needs a
missing capability is reported as skipped with the reason, never as passed.

Coordinates passed to drivers are physical pixels relative to the window's
client area (the frame dump's coordinate space).
"""
from __future__ import annotations

import importlib

CAPABILITIES = {"keys", "mouse", "resize", "clipboard", "focus", "close", "screenshot"}


class Driver:
    name = "none"
    caps: set = set()
    # Why each missing capability is missing (shown in the journey matrix).
    missing: dict = {}
    # Paste shortcut the GUI binds on this platform.
    paste_chord = "ctrl+shift+v"

    def why(self, cap):
        return self.missing.get(cap, f"driver {self.name} has no {cap} support")

    def environment(self):
        """Facts recorded in results.json (display server, versions)."""
        return {}

    def attach(self, gui, timeout=20):
        """Find the GUI's native window (called once per GUI after its first frame)."""

    def key(self, gui, chord):
        """Press and release a chord such as "ctrl+t", "Escape", "ctrl+Return"."""
        raise NotImplementedError

    def type(self, gui, text):
        """Type ASCII text as individual key presses."""
        raise NotImplementedError

    def click(self, gui, x, y):
        raise NotImplementedError

    def move(self, gui, x, y):
        """Move the pointer over a client-area point without clicking."""
        raise NotImplementedError

    def modifier(self, gui, name, down):
        """Hold ("ctrl", "super") or release a modifier key on its own."""
        raise NotImplementedError

    def resize(self, gui, width, height):
        """Request a client-area size in physical pixels."""
        raise NotImplementedError

    def set_clipboard(self, text):
        raise NotImplementedError

    def focus_away(self, gui):
        raise NotImplementedError

    def focus_back(self, gui):
        raise NotImplementedError

    def close(self, gui):
        """Ask the window to close the way a user would (window manager close)."""
        raise NotImplementedError

    def screenshot(self, path):
        """Capture the whole screen as PNG; return True on success."""
        raise NotImplementedError

    def shutdown(self):
        """Release helper processes."""


class NoInput(Driver):
    """Frame dumps only: proves rendering without any OS automation."""
    name = "none"
    missing = {cap: "no OS automation in this environment" for cap in CAPABILITIES}


def load(name):
    if name == "none":
        return NoInput()
    module = importlib.import_module(f"drivers.{name}")
    return module.DRIVER()
