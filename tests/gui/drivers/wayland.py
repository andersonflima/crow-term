"""Wayland driver for a headless sway compositor.

Input: wayvnc gives the seat a virtual keyboard and pointer (wlr virtual
keyboard/pointer protocols) for as long as one VNC client stays connected; the
driver is that client (a minimal RFB 3.8 implementation below). A persistent
device matters: a headless seat without input devices advertises no keyboard or
pointer, and short-lived injectors (one device per command) race the client's
re-binding. Window management via swaymsg, clipboard via wl-copy, screenshots
via grim.

Expects WAYLAND_DISPLAY and SWAYSOCK from a running sway (CI: headless backend,
pixman renderer, output scale 1, no Xwayland so the GUI cannot fall back to X11).
"""
from __future__ import annotations

import json
import os
import shutil
import socket
import struct
import subprocess
import time

from . import Driver

# X keysyms (RFB key events carry keysyms).
KEYSYMS = {
    "ctrl": 0xFFE3, "shift": 0xFFE1, "alt": 0xFFE9, "super": 0xFFEB,
    "Return": 0xFF0D, "Escape": 0xFF1B, "Tab": 0xFF09, "BackSpace": 0xFF08, "slash": 0x2F,
}
SHIFTED = set('~!@#$%^&*()_+{}|:"<>?ABCDEFGHIJKLMNOPQRSTUVWXYZ')


def run(*args, timeout=15, check=True):
    result = subprocess.run(list(map(str, args)), capture_output=True, text=True, timeout=timeout)
    if check and result.returncode:
        raise AssertionError(f"{args[0]} {args[1:]} failed ({result.returncode}): {result.stderr.strip()} {result.stdout.strip()}")
    return result.stdout.strip()


def walk(node):
    yield node
    for child in node.get("nodes", []) + node.get("floating_nodes", []):
        yield from walk(child)


class Rfb:
    """Just enough RFB to send key and pointer events (no framebuffer updates)."""

    def __init__(self, host="127.0.0.1", port=5900, timeout=20):
        until = time.monotonic() + timeout
        while True:
            try:
                self.sock = socket.create_connection((host, port), timeout=5)
                break
            except OSError:
                if time.monotonic() > until:
                    raise
                time.sleep(0.2)
        version = self.recv(12)
        if not version.startswith(b"RFB 003."):
            raise AssertionError(f"not an RFB server: {version!r}")
        self.sock.sendall(b"RFB 003.008\n")
        (count,) = self.recv(1)
        if count == 0:
            (length,) = struct.unpack(">I", self.recv(4))
            raise AssertionError(f"RFB refused: {self.recv(length)!r}")
        types = self.recv(count)
        if 1 not in types:
            raise AssertionError(f"RFB server requires authentication (types {list(types)})")
        self.sock.sendall(b"\x01")
        (result,) = struct.unpack(">I", self.recv(4))
        if result != 0:
            raise AssertionError("RFB security handshake failed")
        self.sock.sendall(b"\x01")  # ClientInit: shared
        width, height = struct.unpack(">HH", self.recv(4))
        self.recv(16)
        (name_length,) = struct.unpack(">I", self.recv(4))
        self.name = self.recv(name_length).decode(errors="replace")
        self.size = (width, height)
        self.buttons = 0

    def recv(self, count):
        data = b""
        while len(data) < count:
            chunk = self.sock.recv(count - len(data))
            if not chunk:
                raise AssertionError("RFB connection closed")
            data += chunk
        return data

    def key(self, keysym, down):
        self.sock.sendall(struct.pack(">BBxxI", 4, 1 if down else 0, keysym))

    def pointer(self, x, y, buttons=0):
        self.sock.sendall(struct.pack(">BBHH", 5, buttons, x, y))

    def close(self):
        self.sock.close()


class Wayland(Driver):
    name = "wayland"
    caps = {"keys", "mouse", "resize", "clipboard", "focus", "close", "screenshot"}

    def __init__(self):
        for variable in ("WAYLAND_DISPLAY", "SWAYSOCK"):
            if not os.environ.get(variable):
                raise SystemExit(f"wayland driver needs {variable}")
        if os.environ.get("DISPLAY"):
            raise SystemExit("unset DISPLAY: the GUI must not fall back to X11")
        for tool in ("wayvnc", "swaymsg", "wl-copy", "grim"):
            if not shutil.which(tool):
                raise SystemExit(f"wayland driver needs {tool}")
        self.clipboard = None
        self.log = open(os.environ.get("CROW_WAYVNC_LOG", os.devnull), "w")
        self.vnc = subprocess.Popen(["wayvnc", "--output=HEADLESS-1", "127.0.0.1", "5900"], stdout=self.log, stderr=self.log)
        self.rfb = Rfb()
        # Park the pointer away from the terminal window.
        self.rfb.pointer(self.rfb.size[0] - 1, self.rfb.size[1] - 1)

    def environment(self):
        return {
            "wayland_display": os.environ.get("WAYLAND_DISPLAY"),
            "sway": json.loads(run("swaymsg", "-t", "get_version", "-r", check=False) or "{}").get("human_readable"),
            "vnc": {"name": self.rfb.name, "size": self.rfb.size},
            "seats": json.loads(run("swaymsg", "-t", "get_seats", "-r", check=False) or "[]"),
        }

    def node(self, gui):
        tree = json.loads(run("swaymsg", "-t", "get_tree", "-r"))
        for node in walk(tree):
            if node.get("pid") == gui.process.pid:
                return node
        return None

    def attach(self, gui, timeout=20):
        until = time.monotonic() + timeout
        while time.monotonic() < until:
            if self.node(gui):
                self.focus_back(gui)
                return
            time.sleep(0.2)
        raise AssertionError(f"no sway window for pid {gui.process.pid}")

    def criteria(self, gui):
        return f"[pid={gui.process.pid}]"

    def press(self, keysyms):
        for keysym in keysyms:
            self.rfb.key(keysym, True)
            time.sleep(0.01)
        for keysym in reversed(keysyms):
            self.rfb.key(keysym, False)
            time.sleep(0.01)
        time.sleep(0.03)

    def key(self, gui, chord):
        parts = chord.split("+")
        keysyms = [KEYSYMS[p] for p in parts[:-1]]
        last = parts[-1]
        if "shift" in parts[:-1] and len(last) == 1:
            # A VNC client sends the shifted keysym (V, not v) while Shift is down;
            # wayvnc otherwise picks the unshifted level and drops the modifiers.
            last = last.upper()
        keysyms.append(KEYSYMS.get(last, ord(last) if len(last) == 1 else None))
        if None in keysyms:
            raise AssertionError(f"no keysym for {chord}")
        self.press(keysyms)

    def type(self, gui, text):
        for char in text:
            if not 0x20 <= ord(char) < 0x7F:
                raise AssertionError(f"type() is ASCII only: {char!r}")
            self.press(([KEYSYMS["shift"]] if char in SHIFTED else []) + [ord(char)])

    def click(self, gui, x, y):
        node = self.node(gui)
        rect, inner = node["rect"], node["window_rect"]
        absolute_x = rect["x"] + inner["x"] + int(x)
        absolute_y = rect["y"] + inner["y"] + int(y)
        self.rfb.pointer(absolute_x, absolute_y)
        time.sleep(0.15)
        self.rfb.pointer(absolute_x, absolute_y, 1)
        time.sleep(0.05)
        self.rfb.pointer(absolute_x, absolute_y, 0)

    def resize(self, gui, width, height):
        run("swaymsg", f"{self.criteria(gui)} resize set width {int(width)} px height {int(height)} px")

    def set_clipboard(self, text):
        # --foreground: the driver owns the clipboard server and stops it at shutdown.
        if self.clipboard is not None:
            self.clipboard.terminate()
            self.clipboard.wait(timeout=5)
        self.clipboard = subprocess.Popen(["wl-copy", "--foreground"], stdin=subprocess.PIPE,
                                          stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.clipboard.stdin.write(text.encode())
        self.clipboard.stdin.close()
        time.sleep(0.2)

    def focus_away(self, gui):
        run("swaymsg", "workspace", "2")

    def focus_back(self, gui):
        run("swaymsg", "workspace", "1")
        run("swaymsg", f"{self.criteria(gui)} focus", check=False)

    def close(self, gui):
        run("swaymsg", f"{self.criteria(gui)} kill")

    def screenshot(self, path):
        return subprocess.run(["grim", str(path)], capture_output=True, timeout=30).returncode == 0

    def shutdown(self):
        try:
            self.rfb.close()
        finally:
            for process in (self.clipboard, self.vnc):
                if process is not None:
                    process.terminate()
                    process.wait(timeout=5)
            self.log.close()


DRIVER = Wayland
