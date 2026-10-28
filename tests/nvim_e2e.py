#!/usr/bin/env python3
"""Real Neovim (no user config) through the daemon PTY: keyboard protocol.

Each chord is encoded by the GUI encoder itself (hidden `debug-encode-key`
command) for the keyboard flags Neovim negotiated, sent to the pane, and the
real editor must decode it as the intended key.
"""
import os
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
        import json
        script = self.runtime / "init.lua"
        script.write_text(init)
        keymap = {name: lhs for name, (_, _, lhs) in CHORDS.items()}
        pane = self.rpc("new-session", "nvim")["Created"]["pane"]
        command = "CROW_KEYMAP=" + shlex.quote(json.dumps(keymap)) + " " + " ".join(
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
