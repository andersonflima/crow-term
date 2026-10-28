#!/usr/bin/env python3
"""Graphical journeys against the real GUI, daemon and PTYs.

    python tests/gui/journeys.py --driver x11 --artifacts out/ [--goldens linux-x11]
        [--update-goldens] [--expect-pass J1,J2,...] [--only J1,J2]

Evidence per journey (results.json + PNGs in the artifacts directory):
* frame dumps from the renderer itself (deterministic; compared with committed
  goldens under tests/gui/goldens/<tag>/ with a tolerant diff, and checked by
  golden-free rules: ink in expected cells, ligature, colour emoji, CJK width,
  icon vs missing-glyph box);
* an OS screenshot proving the window reached the screen, when the driver can
  capture one;
* the mux state (`capture-pane`, files written by Neovim) for input journeys.

`--update-goldens` writes candidate goldens into the artifacts directory for
human review; it never writes into the repository.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import sys
import time
import traceback

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.dont_write_bytecode = True
import drivers  # noqa: E402
import harness  # noqa: E402
from harness import column_of, eventually, locate  # noqa: E402
import imaging  # noqa: E402

BACKGROUND = (255, 255, 255)  # Crowquill Ink Light default background
GOLDENS = HERE / "goldens"


class Skip(Exception):
    pass


class Context:
    def __init__(self, args, driver):
        self.args = args
        self.driver = driver
        self.artifacts = Path(args.artifacts).resolve()
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.tag = args.goldens or driver.name
        self.results = []
        self.info = {}
        self.degraded = []
        self.run = harness.Run(self.artifacts)
        self.fixture_gui = None
        self.nvim_gui = None
        self.nvim_state = {}

    def degrade(self, reason):
        """The journey ran, but through a fallback that skips part of its claim
        (e.g. daemon input instead of native keys): reported as `degraded`."""
        self.degraded.append(reason)

    def need(self, *caps):
        for cap in caps:
            if cap not in self.driver.caps:
                raise Skip(self.driver.why(cap))

    def save_frame(self, name, frame):
        image, meta = frame
        imaging.write_png(self.artifacts / f"{name}.png", image)
        (self.artifacts / f"{name}.json").write_text(json.dumps(meta, indent=2, ensure_ascii=False))
        return f"{name}.png"

    def screenshot(self, name, frame=None):
        """OS screenshot; with `frame`, also prove the frame's header is on screen."""
        if "screenshot" not in self.driver.caps:
            return None, self.driver.why("screenshot")
        path = self.artifacts / f"{name}-screen.png"
        if not self.driver.screenshot(path):
            return None, "screenshot command failed"
        if frame is None:
            return path.name, "captured"
        image, meta = frame
        screen = imaging.read_png(path)
        # Header strip with the session label: distinctive and free of the cursor.
        patch = image.crop(0, 0, min(image.width, int(220 * meta["scale"])), int(40 * meta["scale"]))
        found = imaging.find_patch(screen, patch, tolerance=self.args.screen_tolerance)
        if found is None:
            raise AssertionError(f"window header from the frame dump not found in {path.name}")
        return path.name, f"frame header found on screen at {found}"

    def golden(self, name, frame):
        """Compare against tests/gui/goldens/<tag>/<name>.png. Missing golden is a
        note (candidates are uploaded), a mismatch is a failure."""
        image, meta = frame
        if self.args.update_goldens:
            target = self.artifacts / "goldens" / self.tag
            target.mkdir(parents=True, exist_ok=True)
            imaging.write_png(target / f"{name}.png", image)
        path = GOLDENS / self.tag / f"{name}.png"
        if not path.exists():
            if self.args.require_goldens and not self.args.update_goldens:
                raise AssertionError(f"golden {self.tag}/{name}.png is missing (--require-goldens)")
            return f"no committed golden {self.tag}/{name}.png (candidate in artifacts)"
        golden = imaging.read_png(path)
        ok, report, diff = imaging.compare(image, golden, cell=(meta["cell_width"], meta["cell_height"]))
        if diff is not None:
            imaging.write_png(self.artifacts / f"{name}-diff.png", diff)
        if not ok:
            if self.args.update_goldens:
                # Regenerating on purpose: the candidate is the evidence, not a failure.
                return f"golden {self.tag}/{name}.png differs, candidate written for review: {json.dumps(report)}"
            raise AssertionError(f"golden {self.tag}/{name}.png mismatch: {json.dumps(report)}")
        return f"golden {self.tag}/{name}.png matches ({report['fraction']:.4%} px differ)"


def journey(ident, title):
    def wrap(function):
        function.ident = ident
        function.title = title
        return function
    return wrap


# ------------------------------------------------------------------ helpers

def first_pane(ctx, session):
    layout = ctx.run.session(session)["windows"][0]["layout"]
    while "Split" in layout:
        layout = layout["Split"]["first"]
    return layout["Pane"]


def panes_of(layout):
    if "Pane" in layout:
        return [layout["Pane"]]
    split = layout["Split"]
    return panes_of(split["first"]) + panes_of(split["second"])


def cell_center(meta, pane_index, row, col):
    x, y, w, h = imaging.cell_box(meta, pane_index, row, col)
    return x + w // 2, y + h // 2


def find_pane(meta, pane_id):
    for index, pane in enumerate(meta["panes"]):
        if pane["id"] == pane_id:
            return index
    raise AssertionError(f"pane {pane_id} not in frame {meta['panes']}")


# ----------------------------------------------------------------- journeys

@journey("J1", "launch and first frame")
def j1_launch(ctx):
    python = sys.executable
    gui = ctx.run.launch("fixture", python, HERE / "fixture.py", name="fixture")
    ctx.fixture_gui = gui
    pane = eventually(lambda: first_pane(ctx, "fixture"), 15, what="fixture session")
    eventually(lambda: "END" in ctx.run.capture(pane), 20, what="fixture output in the PTY")
    revision = ctx.run.revision()
    frame = gui.wait_frame(lambda meta, image: meta["revision"] >= revision and meta["panes"], what="first frame with content")
    ctx.driver.attach(gui)
    ctx.info["frame_size"] = [frame[1]["width"], frame[1]["height"], frame[1]["scale"]]
    evidence = [ctx.save_frame("j1-first-frame", frame)]
    shot, note = ctx.screenshot("j1", frame)
    if shot:
        evidence.append(shot)
    return f"first frame {frame[1]['width']}x{frame[1]['height']}@{frame[1]['scale']}; {note}", evidence


@journey("J2", "fixture text: ligatures, accents, CJK, emoji, icon, ZWJ")
def j2_text(ctx):
    gui = ctx.fixture_gui
    if gui is None:
        raise Skip("J1 did not start the fixture GUI")
    pane = first_pane(ctx, "fixture")
    revision = ctx.run.revision()
    frame = gui.wait_frame(lambda meta, image: meta["revision"] >= revision, what="fixture frame")
    screen = ctx.run.capture_json(pane)
    (ctx.artifacts / "j2-capture.txt").write_text("\n".join(harness.grid_text(screen)), encoding="utf-8")
    image, meta = frame
    index = find_pane(meta, pane)
    checks = []

    def box(label, token, occurrence=0):
        row_index, row = locate(screen, label)
        col, span = column_of(row, token, occurrence)
        return imaging.cell_box(meta, index, row_index, col, span), span

    # Every printed glyph has ink; cells past the text stay blank.
    for label in ("LIGA", "ACCENT", "END"):
        row_index, row = locate(screen, label)
        text_cells = [c for c, cell in enumerate(row) if cell["text"].strip() and not cell["wide_spacer"]]
        for col in text_cells:
            if imaging.ink(image, imaging.cell_box(meta, index, row_index, col), BACKGROUND) < 0.01:
                raise AssertionError(f"no ink for {row[col]['text']!r} at row {row_index} col {col}")
        last = max(text_cells)
        blank = imaging.cell_box(meta, index, row_index, last + 3, 4)
        if label != "END" and imaging.ink(image, blank, BACKGROUND) > 0:
            raise AssertionError(f"ink after the text of {label}")
    checks.append("ink in every text cell, none after")

    # Ligature: `->` drawn together differs from `-` and `>` drawn alone.
    pair, _ = box("LIGA", "->")
    minus, _ = box("SOLO", "-")
    greater, _ = box("SOLO", ">")
    together = image.crop(*pair)
    apart = imaging.hstack(image.crop(*minus), image.crop(*greater))
    delta = imaging.mean_delta(together, apart)
    imaging.write_png(ctx.artifacts / "j2-ligature.png", imaging.hstack(together, apart))
    if delta < 2.0:
        raise AssertionError(f"'->' renders like '-' + '>' (mean delta {delta:.2f}); ligatures not applied")
    checks.append(f"ligature -> substituted (mean delta {delta:.1f})")

    for token in ("é", "ç", "ã", "ñ", "ü"):
        cell, _ = box("ACCENT", token)
        if imaging.ink(image, cell, BACKGROUND) < 0.02:
            raise AssertionError(f"accent {token} has no ink")
    checks.append("accents inked")

    cjk, span = box("CJK", "界")
    if span != 2:
        raise AssertionError(f"界 spans {span} cells in the grid, expected 2")
    x, y, w, h = cjk
    halves = [imaging.ink(image, (x, y, w // 2, h), BACKGROUND), imaging.ink(image, (x + w // 2, y, w // 2, h), BACKGROUND)]
    if min(halves) < 0.02:
        raise AssertionError(f"界 ink per half {halves}: glyph does not cover both cells")
    checks.append("CJK spans two inked cells")

    rocket, span = box("EMOJI", "🚀")
    colour = imaging.colorful(image, rocket)
    if colour < 0.05:
        raise AssertionError(f"🚀 is not drawn in colour (saturated fraction {colour:.3f})")
    checks.append(f"emoji in colour ({colour:.0%} saturated, {span} cells)")

    tofu_box, _ = box("TOFU", harness_tofu())
    tofu = image.crop(*tofu_box)
    for icon in ("\uf113", "\ue702"):
        icon_box, span = box("NERD", icon)
        glyph = image.crop(*icon_box)
        if imaging.ink(image, icon_box, BACKGROUND) < 0.02:
            raise AssertionError(f"icon U+{ord(icon):04X} has no ink")
        if (glyph.width, glyph.height) == (tofu.width, tofu.height) and imaging.mean_delta(glyph, tofu) < 4:
            raise AssertionError(f"icon U+{ord(icon):04X} renders as the missing-glyph box")
    checks.append("Nerd icons differ from the missing-glyph box")

    # ZWJ sequences are a known gap of the cell model: record, do not fail.
    row_index, row = locate(screen, "ZWJ")
    zwj_cells = [c for c in range(4, min(len(row), 12)) if imaging.ink(image, imaging.cell_box(meta, index, row_index, c), BACKGROUND) > 0.01]
    ctx.info["zwj_cells_with_ink"] = zwj_cells
    ctx.info["zwj_grid"] = [c["text"] for c in row[4:10]]
    checks.append(f"ZWJ 👩‍💻 inks cells {zwj_cells} (informational)")

    evidence = [ctx.save_frame("j2-fixture", frame), "j2-ligature.png"]
    checks.append(ctx.golden("fixture", frame))
    return "; ".join(checks), evidence


def harness_tofu():
    sys.path.insert(0, str(HERE))
    import fixture
    return fixture.TOFU


@journey("J3", "split panes with Ctrl-t % and Ctrl-t \" and navigate")
def j3_split(ctx):
    gui = ctx.fixture_gui
    if gui is None:
        raise Skip("J1 did not start the fixture GUI")
    window = ctx.run.session("fixture")["windows"][0]
    first = panes_of(window["layout"])[0]
    if "keys" in ctx.driver.caps:
        ctx.driver.key(gui, "ctrl+t")
        ctx.driver.type(gui, "%")
        eventually(lambda: len(panes_of(ctx.run.session("fixture")["windows"][0]["layout"])) == 2, 10, what="vertical split")
        # `%` puts the panes side by side (tmux semantics), `"` stacks them.
        _, meta = gui.wait_frame(lambda m, i: len(m["panes"]) == 2, what="two-pane frame")
        a, b = meta["panes"]
        if not (a["y"] == b["y"] and a["x"] != b["x"]):
            raise AssertionError(f"Ctrl-t % did not split side by side: {meta['panes']}")
        ctx.driver.key(gui, "ctrl+t")
        ctx.driver.type(gui, '"')
        eventually(lambda: len(panes_of(ctx.run.session("fixture")["windows"][0]["layout"])) == 3, 10, what="horizontal split")
        _, meta = gui.wait_frame(lambda m, i: len(m["panes"]) == 3, what="three-pane frame")
        new = meta["panes"][find_pane(meta, meta["pane"])]
        stacked = [p for p in meta["panes"] if p["id"] != new["id"] and p["x"] == new["x"] and p["y"] != new["y"]]
        if not stacked:
            raise AssertionError(f'Ctrl-t " did not stack the focused pane: {meta["panes"]}')
        how = "native keys (% side by side, \" stacked)"
    else:
        ctx.run.cli("--session", "fixture", "split-pane", first, "--window", window["id"], check=True)
        how = "daemon CLI split (no native key automation)"
        ctx.degrade("splits made through the daemon CLI, not Ctrl-t keys")
    layout = ctx.run.session("fixture")["windows"][0]["layout"]
    panes = panes_of(layout)
    for pane in panes:
        eventually(lambda: "END" in ctx.run.capture(pane), 15, what=f"fixture in pane {pane}")
    revision = ctx.run.revision()
    frame = gui.wait_frame(lambda meta, image: len(meta["panes"]) == len(panes) and meta["revision"] >= revision, what="split frame with every pane drawn")
    evidence = [ctx.save_frame("j3-split", frame)]
    # Golden on the frame right after the splits: the newest pane has focus.
    notes = [f"{len(panes)} panes via {how}", ctx.golden("split", frame)]
    if "keys" in ctx.driver.caps:
        # Layout: left | (top-right / bottom-right); the bottom-right pane is focused.
        focused = frame[1]["pane"]

        def x_of(meta, pane):
            return meta["panes"][find_pane(meta, pane)]["x"]
        ctx.driver.key(gui, "ctrl+t")
        ctx.driver.type(gui, "h")
        moved = gui.wait_frame(lambda meta, image: x_of(meta, meta["pane"]) < x_of(meta, focused), what="pane focus moved left")
        left = moved[1]["pane"]
        ctx.driver.key(gui, "ctrl+t")
        ctx.driver.type(gui, "l")
        back = gui.wait_frame(lambda meta, image: x_of(meta, meta["pane"]) > x_of(meta, left), what="pane focus moved right")
        notes.append(f"focus {focused} -h-> {left} -l-> {back[1]['pane']}")
    return "; ".join(notes), evidence


@journey("J4", "window resize reaches the PTY size")
def j4_resize(ctx):
    ctx.need("resize")
    gui = ctx.fixture_gui
    if gui is None:
        raise Skip("J1 did not start the fixture GUI")
    pane = first_pane(ctx, "fixture")
    before = ctx.run.capture_json(pane)["size"]
    _, meta = gui.wait_frame(what="frame before resize")
    width, height = int(meta["width"] * 0.75), int(meta["height"] * 0.8)
    ctx.driver.resize(gui, width, height)
    frame = gui.wait_frame(lambda m, i: abs(m["width"] - width) <= 2 and abs(m["height"] - height) <= 2, what=f"frame {width}x{height}")
    after = eventually(lambda: (s := ctx.run.capture_json(pane)["size"]) != before and s, 10, what="PTY size change")
    if not after["cols"] < before["cols"]:
        raise AssertionError(f"PTY size {before} -> {after}: columns did not shrink")
    return f"window {meta['width']}x{meta['height']} -> {frame[1]['width']}x{frame[1]['height']}; PTY {before} -> {after}", [ctx.save_frame("j4-resized", frame)]


NVIM_INIT = r"""
vim.opt.shortmess:append('I')
vim.o.mouse = 'a'
vim.o.swapfile = false
vim.o.laststatus = 2
local log = {log}
local function mark(text) vim.fn.writefile({{ text }}, log, 'a') end
vim.keymap.set({{ 'n', 'i' }}, '<C-/>', function() mark('KEY_C_SLASH') end)
vim.keymap.set({{ 'n', 'i' }}, '<C-_>', function() mark('KEY_C_UNDERSCORE_LEGACY') end)
vim.keymap.set({{ 'n', 'i' }}, '<C-CR>', function() mark('KEY_C_CR') end)
vim.api.nvim_create_autocmd('FocusLost', {{ callback = function() mark('FOCUS_LOST') end }})
vim.api.nvim_create_autocmd('FocusGained', {{ callback = function() mark('FOCUS_GAINED') end }})
local paste = vim.paste
vim.paste = function(lines, phase)
  mark('PASTE ' .. phase .. ' ' .. #lines)
  return paste(lines, phase)
end
mark('READY')
"""


def nvim_binary():
    return os.environ.get("CROW_TERM_NVIM") or shutil.which("nvim")


def start_nvim(ctx):
    if ctx.nvim_gui is not None:
        return ctx.nvim_gui
    nvim = nvim_binary()
    if not nvim:
        if os.environ.get("CROW_TERM_REQUIRE_NVIM"):
            raise AssertionError("CROW_TERM_REQUIRE_NVIM is set but no nvim binary was found")
        raise Skip("nvim not installed")
    work = ctx.run.work
    log = work / "nvim-events.log"
    init = work / "init.lua"
    init.write_text(NVIM_INIT.format(log=json.dumps(str(log))))
    target = work / "edit.txt"
    lines = "\n".join(f"line {n:02d} abcdefghij" for n in range(1, 11)) + "\n"
    target.write_text(lines)
    gui = ctx.run.launch("nvim", nvim, "--clean", "-n", "-i", "NONE", "-u", init, target, name="nvim")
    pane = eventually(lambda: first_pane(ctx, "nvim"), 15, what="nvim session")
    eventually(lambda: log.exists() and "READY" in log.read_text(), 20, what="nvim init")
    eventually(lambda: "line 01" in ctx.run.capture(pane), 20, what="nvim buffer on screen")
    gui.wait_frame(lambda meta, image: meta["revision"] > 0, what="nvim frame")
    ctx.driver.attach(gui)
    ctx.nvim_gui = gui
    ctx.nvim_state = {"pane": pane, "log": log, "file": target}
    return gui


def normal_mode(ctx, gui):
    """Each Neovim journey starts from Normal mode, whatever the previous one left."""
    if "keys" in ctx.driver.caps:
        ctx.driver.key(gui, "Escape")
    else:
        ctx.run.send_keys(ctx.nvim_state["pane"], "\x1b")
    time.sleep(0.2)


def events(ctx):
    log = ctx.nvim_state["log"]
    return log.read_text().splitlines() if log.exists() else []


def wait_event(ctx, marker, timeout=10):
    return eventually(lambda: marker in events(ctx), timeout, what=f"Neovim event {marker} (events: {events(ctx)[-6:]})")


@journey("J5", "Neovim edit and :w through native keys")
def j5_nvim_edit(ctx):
    gui = start_nvim(ctx)
    normal_mode(ctx, gui)
    target = ctx.nvim_state["file"]
    if "keys" in ctx.driver.caps:
        ctx.driver.type(gui, "ggOcrow typed")
        ctx.driver.key(gui, "Escape")
        ctx.driver.type(gui, ":w")
        ctx.driver.key(gui, "Return")
        how = "native keys"
    else:
        ctx.run.send_keys(ctx.nvim_state["pane"], "ggOcrow typed\x1b:w\r")
        how = "daemon send-keys (no native key automation; GUI key path not exercised)"
        ctx.degrade("keys sent through the daemon, not the GUI")
    eventually(lambda: target.read_text().splitlines()[0] == "crow typed", 10, what="file written by :w")
    frame = gui.wait_frame(lambda m, i: True, what="nvim frame after edit")
    return f"buffer saved with first line 'crow typed' via {how}", [ctx.save_frame("j5-nvim", frame)]


@journey("J6", "clipboard paste arrives as a bracketed paste")
def j6_paste(ctx):
    ctx.need("keys", "clipboard")
    gui = start_nvim(ctx)
    normal_mode(ctx, gui)
    ctx.driver.set_clipboard("pasted one\npasted two")
    ready = getattr(ctx.driver, "clipboard_ready", None)
    if ready is not None:
        ctx.info.setdefault("clipboard_ready_seconds", []).append(ready)
    # Insert mode on a new last line: the paste becomes exactly two lines.
    ctx.driver.type(gui, "Go")
    ctx.driver.key(gui, ctx.driver.paste_chord)
    wait_event(ctx, "PASTE -1 2")
    ctx.driver.key(gui, "Escape")
    ctx.driver.type(gui, ":w")
    ctx.driver.key(gui, "Return")
    def saved():
        lines = [line.rstrip("\r") for line in ctx.nvim_state["file"].read_text().splitlines()]
        return lines[-2:] == ["pasted one", "pasted two"] and lines
    lines = eventually(saved, 10, what="pasted lines saved")
    frame = gui.wait_frame(what="frame after paste")
    paste_events = [e for e in events(ctx) if e.startswith("PASTE")]
    return f"Neovim vim.paste saw {paste_events} (bracketed); file ends {lines[-2:]}; clipboard owned after {ready}s", [ctx.save_frame("j6-paste", frame)]


@journey("J7", "mouse click moves the Neovim cursor")
def j7_mouse(ctx):
    ctx.need("mouse")
    gui = start_nvim(ctx)
    normal_mode(ctx, gui)
    pane = ctx.nvim_state["pane"]
    _, meta = gui.wait_frame(what="frame before click")
    index = find_pane(meta, pane)
    row, col = 4, 7
    x, y = cell_center(meta, index, row, col)
    ctx.driver.click(gui, x, y)
    cursor = eventually(lambda: (c := ctx.run.capture_json(pane)["cursor"]) and (c["row"], c["col"]) == (row, col) and c, 10,
                        what=f"cursor at row {row} col {col}")
    frame = gui.wait_frame(what="frame after click")
    return f"click at {(x, y)} px -> cursor {cursor['row']},{cursor['col']}", [ctx.save_frame("j7-click", frame)]


@journey("J8", "focus out/in reach Neovim (DEC 1004)")
def j8_focus(ctx):
    ctx.need("focus")
    gui = start_nvim(ctx)
    normal_mode(ctx, gui)
    before = len(events(ctx))
    ctx.driver.focus_away(gui)
    gui.wait_frame(lambda m, i: not m["focused"], what="unfocused frame")
    eventually(lambda: "FOCUS_LOST" in events(ctx)[before:], 10, what="FocusLost in Neovim")
    ctx.driver.focus_back(gui)
    gui.wait_frame(lambda m, i: m["focused"], what="focused frame")
    eventually(lambda: "FOCUS_GAINED" in events(ctx)[before:], 10, what="FocusGained in Neovim")
    return f"events {events(ctx)[before:]}", []


@journey("J10", "Ctrl+/ and Ctrl+Return reach Neovim through the key encoder")
def j10_chords(ctx):
    ctx.need("keys")
    gui = start_nvim(ctx)
    normal_mode(ctx, gui)
    before = len(events(ctx))
    ctx.driver.key(gui, "ctrl+slash")
    ctx.driver.key(gui, "ctrl+Return")
    eventually(lambda: {"KEY_C_SLASH", "KEY_C_CR"} <= set(events(ctx)[before:]), 10,
               what=f"chord markers (got {events(ctx)[before:]})")
    keyboard = ctx.run.capture_json(ctx.nvim_state["pane"])["modes"]["keyboard"]
    return f"markers {events(ctx)[before:]} with kitty keyboard flags {keyboard}", []


@journey("J9", "close the GUI, reattach, unsaved buffer survives")
def j9_reattach(ctx):
    gui = start_nvim(ctx)
    normal_mode(ctx, gui)
    pane = ctx.nvim_state["pane"]
    if "keys" in ctx.driver.caps:
        ctx.driver.type(gui, "Gounsaved crow line")
        ctx.driver.key(gui, "Escape")
    else:
        ctx.run.send_keys(pane, "Gounsaved crow line\x1b")
        ctx.degrade("unsaved text typed through the daemon")
    eventually(lambda: "unsaved crow line" in ctx.run.capture(pane), 10, what="unsaved text on screen")
    if "close" in ctx.driver.caps:
        ctx.driver.close(gui)
        how = "window-manager close"
    else:
        gui.process.terminate()
        how = "GUI process terminated (no window-close automation)"
        ctx.degrade("GUI terminated by signal, not closed as a window")
    code = gui.wait_exit(timeout=15)
    if "close" in ctx.driver.caps and code != 0:
        raise AssertionError(f"GUI exited with {code} after a window close: {gui.read_log()}")
    if "unsaved crow line" in ctx.nvim_state["file"].read_text():
        raise AssertionError("buffer was written; the journey needs an unsaved change")
    if "unsaved crow line" not in ctx.run.capture(pane):
        raise AssertionError("pane lost its content after the GUI closed")
    ctx.nvim_gui = None
    again = ctx.run.launch("nvim", attach=True, name="nvim-reattached")
    frame = again.wait_frame(lambda m, i: m["revision"] > 0 and m["pane"] == pane, what="reattached frame")
    screen = ctx.run.capture_json(pane)
    row_index, row = locate(screen, "unsaved")
    box = imaging.cell_box(frame[1], find_pane(frame[1], pane), row_index, 0, 8)
    if imaging.ink(frame[0], box, BACKGROUND) < 0.02:
        raise AssertionError("reattached frame does not show the unsaved line")
    ctx.nvim_gui = again
    return f"{how} (exit {code}); reattached GUI renders the unsaved line from the same Neovim", [ctx.save_frame("j9-reattached", frame)]


JOURNEYS = [j1_launch, j2_text, j3_split, j4_resize, j5_nvim_edit, j6_paste, j7_mouse, j8_focus, j10_chords, j9_reattach]


def summary_markdown(ctx):
    lines = [f"### GUI journeys — {ctx.driver.name} ({platform.system()} {platform.release()})", "",
             "| Journey | Status | Detail |", "|---|---|---|"]
    for result in ctx.results:
        detail = result["detail"].replace("|", "\\|").replace("\n", " ")[:400]
        lines.append(f"| {result['id']} {result['title']} | {result['status']} | {detail} |")
    lines.append("")
    return "\n".join(lines)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--driver", default="none")
    parser.add_argument("--artifacts", required=True)
    parser.add_argument("--goldens", help="golden tag (directory under tests/gui/goldens); default: driver name")
    parser.add_argument("--update-goldens", action="store_true")
    parser.add_argument("--only", default="")
    parser.add_argument("--expect-pass", default="", help="journeys that must pass (skip or degraded counts as failure)")
    parser.add_argument("--require-goldens", action="store_true", help="a missing committed golden is a failure")
    parser.add_argument("--screen-tolerance", type=int, default=0, help="per-channel tolerance when locating the window in a screenshot")
    args = parser.parse_args(argv)
    if not harness.BIN.exists():
        sys.exit(f"{harness.BIN} missing: cargo build --features frame-dump")
    driver = drivers.load(args.driver)
    ctx = None
    only = {item for item in args.only.split(",") if item}
    try:
        ctx = Context(args, driver)
        ctx.info["driver"] = driver.environment()
        for function in JOURNEYS:
            if only and function.ident not in only:
                continue
            started = time.monotonic()
            result = {"id": function.ident, "title": function.title, "evidence": []}
            ctx.degraded = []
            try:
                detail, evidence = function(ctx)
                if ctx.degraded:
                    result.update(status="degraded", detail=f"{detail} [degraded: {'; '.join(ctx.degraded)}]", evidence=evidence)
                else:
                    result.update(status="pass", detail=detail, evidence=evidence)
            except Skip as reason:
                result.update(status="skip", detail=str(reason))
            except Exception as error:  # noqa: BLE001 - every failure is reported per journey
                result.update(status="fail", detail=f"{type(error).__name__}: {error}")
                (ctx.artifacts / f"{function.ident}-traceback.txt").write_text(traceback.format_exc())
                gui = ctx.nvim_gui or ctx.fixture_gui
                if gui and gui.alive():
                    frame = gui.frame()
                    if frame:
                        ctx.save_frame(f"{function.ident}-failure-frame", frame)
                if "screenshot" in driver.caps:
                    try:
                        driver.screenshot(ctx.artifacts / f"{function.ident}-failure-screen.png")
                    except Exception:  # noqa: BLE001
                        pass
            result["seconds"] = round(time.monotonic() - started, 2)
            ctx.results.append(result)
            print(f"{result['id']:>3} {result['status']:<8} {result['title']}: {result['detail']}", flush=True)
    finally:
        driver.shutdown()
        if ctx is not None:
            ctx.run.close()
    (ctx.artifacts / "results.json").write_text(json.dumps({"driver": driver.name, "tag": ctx.tag, "info": ctx.info, "results": ctx.results}, indent=2, ensure_ascii=False))
    markdown = summary_markdown(ctx)
    (ctx.artifacts / "summary.md").write_text(markdown)
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with open(os.environ["GITHUB_STEP_SUMMARY"], "a", encoding="utf-8") as handle:
            handle.write(markdown + "\n")
    expected = {item for item in args.expect_pass.split(",") if item}
    failed = [r["id"] for r in ctx.results if r["status"] == "fail" or (r["id"] in expected and r["status"] != "pass")]
    missing = expected - {r["id"] for r in ctx.results}
    if failed or missing:
        print(f"failed: {failed} missing: {sorted(missing)}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
