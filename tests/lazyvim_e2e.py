#!/usr/bin/env python3
"""LazyVim (pinned, tests/lazyvim) through the daemon PTY. Offline.

Requires a plugin directory from tests/lazyvim_bootstrap.py
(`CROW_LAZYVIM_HOME`) and Neovim >= 0.11.2 (`CROW_TERM_NVIM`). Any HTTP(S)
access is pointed at a closed local port, so an accidental download fails.
Keys are encoded by the GUI encoder for the keyboard flags LazyVim negotiated.
"""
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import sys
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import daemon_e2e  # noqa: E402
from nvim_e2e import BEFORE, encode, leaked_replies  # noqa: E402

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests" / "lazyvim"
DEAD_PROXY = "http://127.0.0.1:9"


def lazyvim_home(test):
    home = os.environ.get("CROW_LAZYVIM_HOME")
    if not home or not (Path(home) / "data" / "nvim" / "lazy" / "LazyVim").is_dir():
        if os.environ.get("CROW_TERM_REQUIRE_NVIM"):
            test.fail("CROW_LAZYVIM_HOME does not contain a bootstrapped plugin set")
        test.skipTest("run tests/lazyvim_bootstrap.py and set CROW_LAZYVIM_HOME")
    return Path(home).resolve()


@unittest.skipIf(os.name == "nt", "POSIX PTY LazyVim fixture")
class LazyVimTests(daemon_e2e.DaemonHarness):
    def setUp(self):
        self.nvim = daemon_e2e.neovim_binary(self)
        self.home = lazyvim_home(self)
        # Every locked plugin must be present at its commit (local git only).
        lazy = self.home / "data" / "nvim" / "lazy"
        for name, entry in json.loads((FIXTURE / "nvim" / "lazy-lock.json").read_text()).items():
            head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=lazy / name, capture_output=True, text=True)
            self.assertEqual(head.stdout.strip(), entry["commit"], f"{name} is not at its locked commit")
        super().setUp()
        self.project = self.runtime / "project"
        self.project.mkdir()
        (self.project / "alpha.lua").write_text(
            "local crowcompletionword = 1\nlocal function greet(name)\n  return 'hi ' .. name\nend\nreturn greet\n")
        (self.project / "beta.md").write_text("# Beta\n\ntext\n")
        git = ["git", "-c", "user.name=crow-e2e", "-c", "user.email=e2e@invalid", "-c", "commit.gpgsign=false"]
        subprocess.run([*git, "init", "-q"], cwd=self.project, check=True)
        subprocess.run([*git, "add", "."], cwd=self.project, check=True)
        subprocess.run([*git, "commit", "-qm", "fixture"], cwd=self.project, check=True)
        # One modified line for gitsigns.
        (self.project / "alpha.lua").write_text(
            "local crowcompletionword = 2\nlocal function greet(name)\n  return 'hi ' .. name\nend\nreturn greet\n")

    def start(self, *files):
        config = self.runtime / "config"
        shutil.copytree(FIXTURE / "nvim", config / "nvim")
        env = {
            "XDG_CONFIG_HOME": config,
            "XDG_DATA_HOME": self.home / "data",
            "XDG_STATE_HOME": self.runtime / "state",
            "XDG_CACHE_HOME": self.runtime / "cache",
            "NVIM_APPNAME": "nvim",
            "http_proxy": DEAD_PROXY,
            "https_proxy": DEAD_PROXY,
            "HTTP_PROXY": DEAD_PROXY,
            "HTTPS_PROXY": DEAD_PROXY,
            "ALL_PROXY": DEAD_PROXY,
            "no_proxy": "",
            "NO_PROXY": "",
            # Ignore user/system git config (e.g. url.insteadOf rewriting to
            # SSH) and allow only local repositories.
            "GIT_CONFIG_GLOBAL": "/dev/null",
            "GIT_CONFIG_NOSYSTEM": "1",
            "GIT_ALLOW_PROTOCOL": "file",
        }
        pane = self.rpc("new-session", "lazyvim")["Created"]["pane"]
        self.command("resize-pane", pane, "--rows", 40, "--cols", 140)
        command = " ".join(f"{key}={shlex.quote(str(value))}" for key, value in env.items())
        command += " " + " ".join(map(shlex.quote, [self.nvim, *files]))
        self.send(pane, f"cd {shlex.quote(str(self.project))} && echo {BEFORE} && {command}")
        self.pane = pane
        self.flags = self.eventually(lambda: self.modes()["keyboard"], timeout=30)
        # Runs before the daemon stops: let Neovim exit and finish writing its
        # state into the temporary directory before that directory is removed.
        self.addCleanup(self.quit_editor)
        return pane

    def quit_editor(self):
        try:
            if self.modes()["keyboard"] == 0:
                return
            self.key("Escape")
            self.ex("qa!")
            self.eventually(lambda: self.modes()["keyboard"] == 0, timeout=15)
            self.send(self.pane, "echo CROW_EDITOR_EXITED")
            self.eventually(lambda: self.output_line(self.pane, "CROW_EDITOR_EXITED"), timeout=10)
        except AssertionError:
            pass

    def modes(self):
        return self.rpc("capture-pane", self.pane, "--json")["modes"]

    def key(self, key, *mods):
        self.command("send-keys", self.pane, encode(key, list(mods), self.flags))

    def keys(self, text):
        for character in text:
            self.key(character)

    def ex(self, command):
        self.key(":")
        self.keys(command)
        self.key("Enter")

    def screen_has(self, *needles, timeout=15):
        last = [""]
        def found():
            last[0] = self.capture(self.pane)
            return all(needle in last[0] for needle in needles) and last[0]
        try:
            return self.eventually(found, timeout=timeout)
        except AssertionError:
            raise AssertionError(f"screen lacks {needles}:\n{last[0]}") from None

    def screen_lacks(self, needle, timeout=10):
        return self.eventually(lambda: needle not in self.capture(self.pane), timeout=timeout)

    def report(self, expression):
        """Evaluate Lua in the editor and return its JSON-encoded result."""
        out = self.runtime / "report.json"
        out.unlink(missing_ok=True)
        lua = f"vim.fn.writefile({{vim.json.encode({expression})}}, {json.dumps(str(out))})"
        self.ex("lua " + lua)
        self.eventually(lambda: out.exists(), timeout=10)
        return json.loads(out.read_text())

    def test_lazyvim_ui_scenarios(self):
        self.start()
        with self.subTest("dashboard"):
            self.screen_has("Find File", "Restore Session", "Neovim loaded", timeout=30)
        with self.subTest("which-key"):
            # which-key loads on VeryLazy, after the dashboard is drawn: retry
            # the leader press until its popup is up.
            def popup():
                self.key("Space")
                try:
                    return self.screen_has("+buffer", "+git", "+search", timeout=4)
                except AssertionError:
                    self.key("Escape")
                    return False
            self.eventually(popup, timeout=30)
            self.key("Escape")
            self.screen_lacks("+buffer")
        with self.subTest("noice cmdline"):
            self.key(":")
            self.screen_has("Cmdline")
            self.key("Escape")
            self.screen_lacks("Cmdline")
        with self.subTest("picker"):
            self.key("Space")
            self.key("Space")
            self.screen_has("Files", "alpha.lua", "beta.md")
            self.keys("beta")
            self.key("Enter")
            self.screen_has("# Beta")
        with self.subTest("bufferline and lualine"):
            self.ex("edit alpha.lua")
            text = self.screen_has("NORMAL", "alpha.lua", "beta.md")
            tabline = text.splitlines()[0]
            self.assertIn("alpha.lua", tabline)
            self.assertIn("beta.md", tabline)
        with self.subTest("gitsigns sign"):
            last = [""]
            def signed():
                screen = self.rpc("capture-pane", self.pane, "--json")
                last[0] = "\n".join("".join(cell["text"] for cell in row) for row in screen["cells"])
                for row in screen["cells"]:
                    line = "".join(cell["text"] for cell in row)
                    if "crowcompletionword = 2" in line:
                        # Sign column sits before the text, after the line number.
                        # (An empty cell text is not a sign: "" is in every string.)
                        return any(cell["text"] and cell["text"] in "▎┃▍" for cell in row[:8]) and line
                return False
            # Oracle: the terminal must show what Neovim itself drew. Read
            # Neovim's own screen grid (screenstring) for the modified line's
            # sign column, then require the same sign in the captured pane.
            grid = (
                "(function() local p = vim.fn.screenpos(0, 1, 1) if p.row == 0 then return '' end"
                " local s = '' for c = 1, 8 do s = s .. vim.fn.screenstring(p.row, c) end return s end)()")
            def drawn_by_neovim():
                cells = self.report(grid)
                return next((sign for sign in "▎┃▍" if sign in cells), None)
            def terminal_shows(sign):
                screen = self.rpc("capture-pane", self.pane, "--json")
                last[0] = "\n".join("".join(cell["text"] for cell in row) for row in screen["cells"])
                row = next((r for r in screen["cells"]
                            if "crowcompletionword = 2" in "".join(c["text"] for c in r)), None)
                return row is not None and any(cell["text"] == sign for cell in row[:8])
            try:
                self.eventually(signed, timeout=10)
            except AssertionError:
                sign = drawn_by_neovim()
                if sign:
                    # Neovim had already drawn it while the terminal showed no
                    # sign for 10 s: a terminal defect. No retry, so the redraw
                    # caused by the report command cannot repair it.
                    if not terminal_shows(sign):
                        raise AssertionError(
                            f"Neovim drew {sign!r} in the sign column but the terminal shows:\n{last[0]}") from None
                else:
                    # gitsigns has not drawn anything: detached (buffer read before
                    # the plugin loaded on a busy runner) or still diffing. Editor
                    # timing, not the terminal: attach and let Neovim draw.
                    self.ex("Gitsigns attach")
                    try:
                        sign = self.eventually(drawn_by_neovim, timeout=30)
                    except AssertionError:
                        raise AssertionError(f"Neovim never drew a gitsigns sign:\n{last[0]}") from None
                    print("gitsigns: attached explicitly", file=sys.stderr)
                    try:
                        self.eventually(lambda: terminal_shows(sign), timeout=5)
                    except AssertionError:
                        raise AssertionError(
                            f"Neovim drew {sign!r} in the sign column but the terminal shows:\n{last[0]}") from None
        with self.subTest("treesitter highlight"):
            active = self.report("vim.treesitter.highlighter.active[vim.api.nvim_get_current_buf()] ~= nil")
            self.assertTrue(active)
            def greet_row():
                screen = self.rpc("capture-pane", self.pane, "--json")
                return next((r for r in screen["cells"] if "local function greet" in "".join(c["text"] for c in r)), None)
            row = self.eventually(greet_row, timeout=10)
            text = "".join(c["text"] for c in row)
            keyword = row[text.index("local function")]
            name = row[text.index("greet")]
            self.assertNotEqual(keyword["fg"], name["fg"], "keyword and function name share a color")
        with self.subTest("blink menu"):
            self.key("G")
            self.key("o")
            self.keys("crowcomp")
            def menu():
                lines = [line for line in self.capture(self.pane).splitlines() if "crowcompletionword" in line]
                return len(lines) >= 2
            self.eventually(menu, timeout=15)
            self.key("Escape")
            self.key("u")
        with self.subTest("termguicolors"):
            # LazyVim's tokyonight sets its own background; the clean-Neovim
            # test covers background detection from OSC 11.
            self.assertTrue(self.report("vim.o.termguicolors"))
        with self.subTest("clean exit without leaked replies"):
            self.key("Escape")
            self.ex("qa!")
            self.eventually(lambda: self.modes()["keyboard"] == 0, timeout=15)
            self.send(self.pane, "echo CROW_AFTER_LAZYVIM")
            self.eventually(lambda: self.output_line(self.pane, "CROW_AFTER_LAZYVIM"), timeout=10)
            self.assertEqual(leaked_replies(self, self.pane, "CROW_AFTER_LAZYVIM"), [],
                             self.capture(self.pane))

    def test_checkhealth_errors_are_allowlisted(self):
        self.start("alpha.lua")
        self.screen_has("alpha.lua", timeout=30)
        out = self.runtime / "health.txt"
        self.ex("checkhealth")
        self.screen_has("health", timeout=60)
        self.eventually(lambda: "ERROR" in self.capture(self.pane) or "OK" in self.capture(self.pane), timeout=60)
        self.ex(f"silent write! {out}")
        self.eventually(lambda: out.exists() and out.stat().st_size > 0, timeout=30)
        report = out.read_text()
        # A real report, not an empty or partial buffer.
        for section in ("lazy", "snacks", "noice", "which-key", "vim.health"):
            self.assertIn(section, report.lower(), report[:2000])
        errors = sorted({line.strip() for line in out.read_text().splitlines() if "ERROR" in line})
        allow = [re.compile(line) for line in (FIXTURE / "checkhealth-allowlist.txt").read_text().splitlines()
                 if line.strip() and not line.startswith("#")]
        unexpected = [line for line in errors if not any(pattern.search(line) for pattern in allow)]
        self.assertEqual(unexpected, [], "\n".join(errors))


if __name__ == "__main__":
    unittest.main(verbosity=2)
