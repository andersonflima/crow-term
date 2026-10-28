#!/usr/bin/env python3
"""Real Neovim (no user config) through the daemon PTY: keyboard protocol.

The byte sequences below are the ones `src/keyboard.rs` produces for the
keyboard flags Neovim requests (asserted by its reference-table unit tests;
press events are identical under flags 1 and 3); here the real editor must
decode each one as the intended key.
"""
import os
from pathlib import Path
import shlex
import sys
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import daemon_e2e  # noqa: E402

# key name -> (bytes sent for kitty flag 1 "disambiguate", Neovim lhs)
KITTY_DISAMBIGUATE = {
    "C_SLASH": ("\x1b[47;5u", "<C-/>"),
    "C_DOT": ("\x1b[46;5u", "<C-.>"),
    "C_SEMI": ("\x1b[59;5u", "<C-;>"),
    "C_COMMA": ("\x1b[44;5u", "<C-,>"),
    "C_I": ("\x1b[105;5u", "<C-i>"),
    "TAB": ("\t", "<Tab>"),
    "C_M": ("\x1b[109;5u", "<C-m>"),
    "CR": ("\r", "<CR>"),
    "S_CR": ("\x1b[13;2u", "<S-CR>"),
    "C_CR": ("\x1b[13;5u", "<C-CR>"),
    "C_TAB": ("\x1b[9;5u", "<C-Tab>"),
    "C_BS": ("\x1b[127;5u", "<C-BS>"),
    # Report-all-keys (flag 8) form; Neovim decodes it even though it only
    # requests flag 1, under which Shift+Space is plain text (kitty semantics).
    "S_SPACE": ("\x1b[32;2u", "<S-Space>"),
}

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
        keymap = {name: lhs for name, (_, lhs) in KITTY_DISAMBIGUATE.items()}
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
        for name, (sequence, lhs) in KITTY_DISAMBIGUATE.items():
            with self.subTest(key=lhs):
                self.command("send-keys", pane, sequence)
                self.eventually(lambda: self.output_line(pane, "KEY_" + name), timeout=5)
        if flags & 2:
            # Release events (what the encoder adds under flag 2) insert nothing.
            self.command("send-keys", pane, "ix\x1b[120;1:3uy\x1b[121;1:3u\x1b[27u\x1b[27;1:3u")
            self.eventually(lambda: self.output_line(pane, "xyKEY_S_SPACE"), timeout=5)
        # Leaving Neovim pops its keyboard mode: the shell gets legacy keys again.
        self.command("send-keys", pane, "\x1b[27u:qa!\r")
        self.eventually(lambda: self.modes(pane)["keyboard"] == 0, timeout=10)


if __name__ == "__main__":
    unittest.main(verbosity=2)
