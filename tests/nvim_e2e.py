#!/usr/bin/env python3
"""Real Neovim (no user config) through the daemon PTY: keyboard protocol.

Each chord is encoded by the GUI encoder itself (hidden `debug-encode-key`
command) for the keyboard flags Neovim negotiated, sent to the pane, and the
real editor must decode it as the intended key.
"""
import json
import os
import re
from pathlib import Path
import shlex
import subprocess
import sys
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import daemon_e2e  # noqa: E402

# marker -> (key, modifiers, Neovim lhs)
CHORDS = {
    "C_SLASH": ("/", ["ctrl"], "<C-/>"),
    "C_DOT": (".", ["ctrl"], "<C-.>"),
    "C_SEMI": (";", ["ctrl"], "<C-;>"),
    "C_COMMA": (",", ["ctrl"], "<C-,>"),
    "C_I": ("i", ["ctrl"], "<C-i>"),
    "TAB": ("Tab", [], "<Tab>"),
    "C_M": ("m", ["ctrl"], "<C-m>"),
    "CR": ("Enter", [], "<CR>"),
    "S_CR": ("Enter", ["shift"], "<S-CR>"),
    "C_CR": ("Enter", ["ctrl"], "<C-CR>"),
    "C_TAB": ("Tab", ["ctrl"], "<C-Tab>"),
    "C_BS": ("Backspace", ["ctrl"], "<C-BS>"),
    "A_J": ("j", ["alt"], "<M-j>"),
}


BEFORE = "CROW_BEFORE_NVIM"
# Query answers that would show up as text if they reached the shell.
LEAK_PATTERNS = [re.compile(pattern) for pattern in (
    r"\[\?[\d;]*[cuy]", r"\[>[\d;]*[cmq]", r"\[[\d;:]+u", r"rgb:[0-9a-f]",
    r"P[01][+$]r", r">\|", r"\[\d+;\d+R", r"\[[468];\d+;\d+t", r"\]1[01];",
)]


def leaked_replies(test, pane, after):
    """Answer-like text between the editor start marker and the shell's
    `after` marker (the region a leaked reply would be typed or printed in)."""
    lines = test.capture(pane).splitlines()
    start = max(index for index, line in enumerate(lines) if line.strip() == BEFORE)
    end = max(index for index, line in enumerate(lines) if line.strip() == after)
    region = "\n".join(lines[start + 1:end]).replace("echo " + after, "")
    return [pattern.pattern for pattern in LEAK_PATTERNS if pattern.search(region)]


def encode(key, mods, flags, release=False):
    args = [str(daemon_e2e.BIN), "debug-encode-key", key, "--flags", str(flags)]
    for mod in mods:
        args += ["--mod", mod]
    if release:
        args.append("--release")
    return subprocess.run(args, capture_output=True, check=True, timeout=10).stdout.decode()


INIT = """
vim.opt.shortmess:append('I')
for name, lhs in pairs(vim.json.decode(vim.env.CROW_KEYMAP)) do
  vim.keymap.set('n', lhs, function()
    vim.api.nvim_buf_set_lines(0, 0, -1, false, { 'KEY_' .. name })
  end)
end
"""


class NeovimKeyboardTests(daemon_e2e.DaemonHarness):
    def setUp(self):
        if os.name == "nt":
            self.skipTest("POSIX PTY Neovim fixture")
        self.nvim = daemon_e2e.neovim_binary(self)
        super().setUp()

    def start_nvim(self, init):
        script = self.runtime / "init.lua"
        script.write_text(init)
        keymap = {name: lhs for name, (_, _, lhs) in CHORDS.items()}
        pane = self.rpc("new-session", "nvim")["Created"]["pane"]
        command = f"echo {BEFORE}; CROW_KEYMAP=" + shlex.quote(json.dumps(keymap)) + " " + " ".join(
            map(shlex.quote, [self.nvim, "--clean", "-n", "-i", "NONE", "-u", str(script)]))
        self.send(pane, command)
        return pane

    def modes(self, pane):
        return self.rpc("capture-pane", pane, "--json")["modes"]

    def test_neovim_negotiates_kitty_keyboard_and_decodes_each_chord(self):
        pane = self.start_nvim(INIT)
        flags = self.eventually(lambda: self.modes(pane)["keyboard"], timeout=10)
        # Neovim 0.11 requests disambiguate (1); 0.12 adds event types (2).
        self.assertIn(flags, (1, 3))
        for name, (key, mods, lhs) in CHORDS.items():
            with self.subTest(key=lhs):
                self.command("send-keys", pane, encode(key, mods, flags))
                self.eventually(lambda: self.output_line(pane, "KEY_" + name), timeout=5)
        # Shift+Space types a plain space under flags 1/3 (kitty semantics):
        # Neovim sees <Space>, so a <S-Space> mapping cannot fire.
        self.assertEqual(encode("Space", ["shift"], flags), " ")
        if flags & 2:
            # Release events (sent under flag 2) insert nothing.
            keys = "i" + encode("x", [], flags) + encode("x", [], flags, release=True)
            keys += encode("y", [], flags) + encode("y", [], flags, release=True)
            keys += encode("Escape", [], flags) + encode("Escape", [], flags, release=True)
            self.command("send-keys", pane, keys)
            self.eventually(lambda: self.output_line(pane, "xyKEY_A_J"), timeout=5)
        # Leaving Neovim pops its keyboard mode: the shell gets legacy keys again.
        self.command("send-keys", pane, encode("Escape", [], flags) + ":qa!\r")
        self.eventually(lambda: self.modes(pane)["keyboard"] == 0, timeout=10)

    def test_clean_neovim_detects_terminal_features_without_leaking_replies(self):
        report = self.runtime / "report.json"
        init = f"""
vim.opt.shortmess:append('I')
vim.api.nvim_buf_set_lines(0, 0, -1, false, {{ 'crowtypo here' }})
vim.fn.matchadd('SpellBad', 'crowtypo')
vim.api.nvim_create_user_command('CrowReport', function()
  local report = {{
    termguicolors = vim.o.termguicolors,
    background = vim.o.background,
  }}
  vim.fn.writefile({{ vim.json.encode(report) }}, {json_quote(str(report))})
end, {{}})
"""
        pane = self.start_nvim(init)
        self.eventually(lambda: self.output_line(pane, "crowtypo here"), timeout=10)
        # Detection is asynchronous (OSC 11, XTGETTCAP); give it a moment.
        def reported():
            self.command("send-keys", pane, ":CrowReport\r")
            return report.exists() and json.loads(report.read_text())
        state = self.eventually(lambda: (r := reported()) and r["background"] == "light" and r, timeout=10)
        self.assertTrue(state["termguicolors"], state)
        # SpellBad: undercurl with a special (underline) color. Neovim detects
        # undercurl from the DECRQSS SGR answer (`4:3`); protocol_probe.py
        # covers the XTGETTCAP Smulx/Setulc answers separately.
        def curly():
            cells = [c for c in self.rpc("capture-pane", pane, "--json")["cells"][0]]
            marked = cells[:8]
            return all(c["underline_style"] == "Curly" for c in marked) and marked
        marked = self.eventually(curly, timeout=10)
        self.assertTrue(all(c["underline_color"] is not None for c in marked), marked[0])
        self.assertNotEqual(self.rpc("capture-pane", pane, "--json")["cells"][0][9]["underline_style"], "Curly")
        # Cursor shape follows 'guicursor': block in Normal, beam in Insert.
        cursor = lambda: self.rpc("capture-pane", pane, "--json")["cursor"]["shape"]
        self.eventually(lambda: cursor() == "block", timeout=5)
        self.command("send-keys", pane, "i")
        self.eventually(lambda: cursor() == "beam", timeout=5)
        flags = self.modes(pane)["keyboard"]
        self.command("send-keys", pane, encode("Escape", [], flags))
        self.eventually(lambda: cursor() == "block", timeout=5)
        self.command("send-keys", pane, ":qa!\r")
        self.eventually(lambda: self.modes(pane)["keyboard"] == 0, timeout=10)
        self.send(pane, "echo CROW_AFTER_NVIM")
        self.eventually(lambda: self.output_line(pane, "CROW_AFTER_NVIM"), timeout=10)
        # No query answer may surface as text in the shell afterwards.
        self.assertEqual(leaked_replies(self, pane, "CROW_AFTER_NVIM"), [], self.capture(pane))


def json_quote(text):
    return json.dumps(text)


if __name__ == "__main__":
    unittest.main(verbosity=2)
