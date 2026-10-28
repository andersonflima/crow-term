"""Isolated daemon + real GUI process + frame-dump reader for the GUI journeys.

The GUI must be built with `--features frame-dump`. Each run owns a private
runtime directory (socket, config, frames); nothing touches the user's config,
daemon or clipboard history beyond what a driver explicitly sets.
"""
from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]
sys.path.insert(0, str(HERE))
import imaging  # noqa: E402

EXE = "crow-term.exe" if os.name == "nt" else "crow-term"
BIN = Path(os.environ.get("CROW_TERM_BIN", ROOT / "target" / "debug" / EXE))

CONFIG = """\
# GUI journeys: Crowquill Ink Light defaults, crow status off (no external
# command, no clock), fixed size so frames are comparable.
font_size = 18.0
line_height = 1.25
ligatures = true

[crow]
enabled = false
"""


class Timeout(AssertionError):
    pass


def eventually(callback, timeout, interval=0.05, what="condition"):
    until = time.monotonic() + timeout
    last = None
    while True:
        try:
            value = callback()
            if value:
                return value
        except (AssertionError, KeyError, ValueError, IndexError, OSError, json.JSONDecodeError) as error:
            last = error
        if time.monotonic() >= until:
            raise Timeout(f"{what} timed out after {timeout}s (last error: {last!r})")
        time.sleep(interval)


class Run:
    """One isolated daemon; GUIs are started/stopped by journeys."""

    def __init__(self, artifacts: Path, shell=None):
        self.artifacts = Path(artifacts)
        self.artifacts.mkdir(parents=True, exist_ok=True)
        # Short path: Unix socket paths are limited to ~100 bytes.
        self.temp = tempfile.mkdtemp(prefix="crowgui-")
        self.runtime = Path(self.temp) / "rt"
        self.runtime.mkdir(mode=0o700)
        self.frames = Path(self.temp) / "frames"
        self.frames.mkdir()
        self.work = Path(self.temp) / "work"
        self.work.mkdir()
        self.config = Path(self.temp) / "config.toml"
        self.config.write_text(CONFIG)
        self.env = dict(os.environ, CROW_TERM_RUNTIME_DIR=str(self.runtime))
        self.shell = shell or ("powershell.exe" if os.name == "nt" else "/bin/sh")
        self.daemon_log = (self.artifacts / "daemon.log").open("w+")
        self.daemon = subprocess.Popen(
            [str(BIN), "--config", str(self.config), "daemon"],
            env=self.env, stdout=self.daemon_log, stderr=self.daemon_log,
        )
        self.guis = []
        try:
            # `doctor` never starts a daemon; other commands would autostart one
            # if they reached the socket before this daemon bound it.
            eventually(lambda: "daemon: running" in self.cli("doctor").stdout, 15, what="daemon start")
            if self.daemon.poll() is not None:
                raise AssertionError(f"test daemon exited with {self.daemon.returncode}")
        except BaseException:
            self.close()
            raise

    # ------------------------------------------------------------- daemon CLI
    def cli(self, *args, check=False, timeout=15):
        result = subprocess.run(
            [str(BIN), "--config", str(self.config), "--shell", self.shell, *map(str, args)],
            env=self.env, capture_output=True, text=True, timeout=timeout,
            encoding="utf-8", errors="replace",
        )
        if check and result.returncode:
            raise AssertionError(f"crow-term {args}: {result.stderr.strip()}")
        return result

    def rpc(self, *args):
        return json.loads(self.cli(*args, check=True).stdout)

    def sessions(self):
        return self.rpc("list-sessions")["View"]["sessions"]

    def revision(self):
        """Current mux revision: a frame drawn at this revision or later shows
        every PTY change made before this call."""
        return self.rpc("list-sessions")["View"]["revision"]

    def session(self, name):
        for session in self.sessions():
            if session["name"] == name:
                return session
        raise AssertionError(f"session {name!r} does not exist")

    def capture(self, pane):
        return self.cli("capture-pane", pane, check=True).stdout

    def capture_json(self, pane):
        return self.rpc("capture-pane", pane, "--json")

    def send_keys(self, pane, text):
        self.cli("send-keys", pane, text, check=True)

    # -------------------------------------------------------------------- GUI
    def launch(self, session, *program, attach=False, name=None):
        """Start a GUI. `program` runs without a shell (`start -- program...`)."""
        name = name or "gui"
        if any(gui.name == name for gui in self.guis):
            name = f"{name}-{len(self.guis) + 1}"
        frame_dir = self.frames / name
        frame_dir.mkdir()
        log = (self.artifacts / f"{name}.log").open("w")
        args = [str(BIN), "--config", str(self.config), "--shell", self.shell, "--session", session]
        if attach:
            args += ["attach", session]
        elif program:
            args += ["start", "--", *map(str, program)]
        else:
            args += ["start"]
        env = dict(self.env, CROW_TERM_FRAME_DUMP=str(frame_dir))
        process = subprocess.Popen(args, env=env, stdout=log, stderr=log, cwd=self.work)
        gui = Gui(self, name, process, frame_dir, log)
        self.guis.append(gui)
        return gui

    def close(self):
        for gui in self.guis:
            gui.kill()
        if self.daemon.poll() is None:
            try:
                self.cli("stop", "--force", timeout=10)
            except (OSError, subprocess.SubprocessError):
                pass
            try:
                self.daemon.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
                self.daemon.wait(timeout=5)
        self.daemon_log.close()
        shutil.rmtree(self.temp, ignore_errors=True)


class Gui:
    def __init__(self, run, name, process, frame_dir, log):
        self.run = run
        self.name = name
        self.process = process
        self.frame_dir = frame_dir
        self.log = log

    def alive(self):
        return self.process.poll() is None

    def frame(self):
        """Latest (Image, meta), or None before the first frame."""
        path = self.frame_dir / "frame.ppm"
        if not path.exists():
            return None
        return imaging.read_frame(path)

    def wait_frame(self, predicate=lambda meta, image: True, timeout=20, settle=0.4, what="frame"):
        """First frame satisfying `predicate` that stays the latest for `settle`
        seconds (event-driven rendering: no new frame means the state is drawn)."""
        def ready():
            if not self.alive():
                raise AssertionError(f"GUI exited with {self.process.returncode}: {self.read_log()}")
            current = self.frame()
            if not current or not predicate(current[1], current[0]):
                return None
            seq = current[1]["seq"]
            time.sleep(settle)
            again = self.frame()
            if again and again[1]["seq"] == seq:
                return again
            return None
        return eventually(ready, timeout, interval=0.1, what=f"{self.name}: {what}")

    def read_log(self):
        self.log.flush()
        try:
            return (self.run.artifacts / f"{self.name}.log").read_text(errors="replace")[-4000:]
        except OSError:
            return ""

    def wait_exit(self, timeout=10):
        return self.process.wait(timeout=timeout)

    def kill(self):
        if self.alive():
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.log.close()


# ------------------------------------------------------------ grid helpers

def grid_text(screen):
    """Row strings with wide spacers removed (as `capture-pane` prints them)."""
    return ["".join(c["text"] for c in row if not c["wide_spacer"]).rstrip() for row in screen["cells"]]


def locate(screen, label):
    """Row index of the line starting with `label` and that row's cells."""
    for index, row in enumerate(screen["cells"]):
        text = "".join(c["text"] for c in row if not c["wide_spacer"])
        if text.startswith(label + " ") or text.rstrip() == label:
            return index, row
    raise AssertionError(f"line {label!r} not on screen: {grid_text(screen)}")


def column_of(row, token, occurrence=0):
    """Grid column where `token` starts (cluster-aware) and its width in cells."""
    texts = [c["text"] for c in row]
    seen = 0
    for col in range(len(row)):
        if row[col]["wide_spacer"]:
            continue
        joined = ""
        end = col
        while end < len(row) and len(joined) < len(token):
            if not row[end]["wide_spacer"]:
                joined += texts[end]
            end += 1
        if joined == token:
            if seen == occurrence:
                while end < len(row) and row[end]["wide_spacer"]:
                    end += 1
                return col, end - col
            seen += 1
    raise AssertionError(f"{token!r} not found in {''.join(texts)!r}")
