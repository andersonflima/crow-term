#!/usr/bin/env python3
"""Exercise the actual CLI, daemon, PTYs and VT snapshots in an isolated runtime."""
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

ROOT = Path(__file__).resolve().parents[1]
BIN = Path(os.environ.get("CROW_TERM_BIN", ROOT / "target/debug" / ("crow-term.exe" if os.name == "nt" else "crow-term")))


class DaemonTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="crow-e2e-")
        self.addCleanup(self.temp.cleanup)
        self.runtime = Path(self.temp.name)
        self.runtime.chmod(0o700)
        self.env = dict(os.environ, CROW_TERM_RUNTIME_DIR=str(self.runtime))
        self.shell = "powershell.exe" if os.name == "nt" else "/bin/sh"
        self.log = (self.runtime / "test-daemon.log").open("w+")
        self.addCleanup(self.log.close)
        self.daemon = subprocess.Popen([str(BIN), "daemon"], env=self.env, stdout=self.log, stderr=self.log)
        self.addCleanup(self.stop)
        self.eventually(lambda: self.command("list-sessions"), timeout=8)

    def stop(self):
        if self.daemon.poll() is None:
            subprocess.run([str(BIN), "stop", "--force"], env=self.env, capture_output=True, timeout=10)
            try:
                self.daemon.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.daemon.kill()
                self.daemon.wait(timeout=5)

    def command(self, *args, check=True):
        shell_options=[] if "--shell" in args else ["--shell",self.shell]
        result = subprocess.run([str(BIN), *shell_options, *map(str, args)], env=self.env,
                                capture_output=True, text=True, timeout=12)
        if check and result.returncode:
            raise AssertionError(f"{args}: {result.stderr}")
        return result

    def rpc(self, *args):
        return json.loads(self.command(*args).stdout)

    def eventually(self, callback, timeout=5):
        until = time.monotonic() + timeout
        error = None
        while time.monotonic() < until:
            try:
                result = callback()
                if result:
                    return result
            except (AssertionError, KeyError, ValueError) as exc:
                error = exc
            time.sleep(0.03)
        self.log.flush()
        self.log.seek(0)
        raise AssertionError(f"Condition timed out: {error}; daemon: {self.log.read()}")

    def capture(self, pane):
        return self.command("capture-pane", pane).stdout

    def send(self, pane, text):
        self.command("send-keys", pane, text, "--enter")

    def test_session_window_split_reattach_resize_and_isolation(self):
        first = self.rpc("new-session", "alpha")["Created"]
        self.assertEqual(self.rpc("new-session", "alpha")["Created"], first)
        pane, window = first["pane"], first["window"]
        self.send(pane, "echo CROW_REAL_PTY")
        self.eventually(lambda: "CROW_REAL_PTY" in self.capture(pane))
        # Reattaching with another CLI default must not change a session's shell profile.
        second = self.rpc("--session", "alpha", "--shell", "crow-nonexistent-shell-fixture", "new-window")["Created"]
        self.send(second["pane"],"echo CROW_ORIGINAL_PROFILE")
        self.eventually(lambda: "CROW_ORIGINAL_PROFILE" in self.capture(second["pane"]))
        split = self.rpc("--session", "alpha", "split-pane", pane, "--window", window, "--horizontal")["Created"]
        sessions = self.rpc("list-sessions")["View"]["sessions"]
        self.assertEqual(len(sessions), 1)
        self.assertEqual(len(sessions[0]["windows"]), 2)
        self.assertEqual(sessions[0]["windows"][0]["layout"]["Split"]["axis"], "Horizontal")
        self.command("resize-pane", pane, "--rows", 18, "--cols", 63)
        screen = self.rpc("capture-pane", pane, "--json")
        self.assertEqual(screen["size"], {"rows": 18, "cols": 63})
        self.assertNotEqual(self.command("resize-pane", pane, "--rows", 0, "--cols", 80, check=False).returncode, 0)
        self.assertNotEqual(self.command("stop", check=False).returncode, 0)
        beta = self.rpc("new-session", "beta")["Created"]
        self.command("--session", "alpha", "close-pane", split["pane"], "--window", window)
        self.command("kill-session", "alpha")
        sessions = self.rpc("list-sessions")["View"]["sessions"]
        self.assertEqual([s["name"] for s in sessions], ["beta"])
        self.assertEqual(self.rpc("new-session", "beta")["Created"], beta)
        self.command("kill-session", "beta")
        self.assertEqual(self.rpc("list-sessions")["View"]["sessions"], [])
        self.assertNotEqual(second["pane"], pane)

    def test_four_large_panes_fit_the_wire_protocol(self):
        created=self.rpc("new-session","large")["Created"]
        panes=[created["pane"]]
        for _ in range(3):
            panes.append(self.rpc("--session","large","split-pane",panes[0],"--window",created["window"])["Created"]["pane"])
        for pane in panes:
            self.command("resize-pane",pane,"--rows",200,"--cols",200)
        self.assertEqual(self.rpc("capture-pane",panes[0],"--json")["size"],{"rows":200,"cols":200})

    def test_shell_setup_never_executes_a_working_directory_helper(self):
        untrusted=self.runtime/"cwd"
        (untrusted/"shell").mkdir(parents=True)
        (untrusted/"shell/setup.py").write_text('print("UNTRUSTED_CWD_SCRIPT_EXECUTED")\n')
        result=subprocess.run([str(BIN),"shell-setup","--dry-run"],cwd=untrusted,env=self.env,capture_output=True,text=True,timeout=10)
        self.assertNotEqual(result.returncode,0)
        self.assertNotIn("UNTRUSTED_CWD_SCRIPT_EXECUTED",result.stdout)
        # Forwarded --shell belongs to setup.py, not the terminal's root shell override.
        result=subprocess.run([str(BIN),"shell-setup","--source",str(ROOT),"--shell","fish","--prefix",str(self.runtime/"profile"),"--dry-run"],env=self.env,capture_output=True,text=True,timeout=10)
        self.assertEqual(result.returncode,0,result.stderr)
        self.assertFalse((self.runtime/"profile").exists())

    @unittest.skipIf(os.name == "nt", "POSIX shell fixture; Windows uses separate PowerShell smoke")
    def test_pid_survives_clients_and_vt_truecolor_unicode(self):
        pane = self.rpc("new-session", "pid")["Created"]["pane"]
        self.send(pane, "printf 'CROW_PID_%s\\n' \"$$\"")
        output = self.eventually(lambda: (m.group(1) if (m := re.search(r"CROW_PID_(\d+)", self.capture(pane))) else None))
        pid = int(output)
        os.kill(pid, 0)
        self.assertEqual(self.rpc("new-session", "pid")["Created"]["pane"], pane)
        self.send(pane, "printf '\\033[38;2;1;2;3mCROW_RGB\\033[0m 界 é 🚀\\n'")
        self.eventually(lambda: "CROW_RGB" in self.capture(pane))
        screen = self.rpc("capture-pane", pane, "--json")
        self.assertTrue(any(c["fg"] == {"Rgb": [1, 2, 3]} for row in screen["cells"] for c in row))
        self.assertTrue(any(c["wide_spacer"] for row in screen["cells"] for c in row))
        self.assertIn("界", self.capture(pane))
        self.command("kill-session", "pid")
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    @unittest.skipIf(os.name == "nt", "Neovim POSIX PTY fixture")
    def test_neovim_alternate_screen_input_and_clean_exit(self):
        nvim = shutil.which("nvim")
        if not nvim:
            self.skipTest("nvim not installed")
        import shlex
        pane = self.rpc("new-session", "nvim")["Created"]["pane"]
        self.send(pane, shlex.quote(nvim) + " --clean -n -i NONE")
        self.eventually(lambda: self.rpc("capture-pane", pane, "--json")["modes"]["bracketed_paste"])
        self.command("send-keys", pane, "iCrow terminal é 界 -> != 🚀\x1b")
        self.eventually(lambda: "Crow terminal" in self.capture(pane))
        self.command("send-keys", pane, ":qa!\r")
        self.eventually(lambda: not self.rpc("capture-pane", pane, "--json")["modes"]["bracketed_paste"])
        self.send(pane, "echo CROW_NVIM_EXIT_OK")
        self.eventually(lambda: "CROW_NVIM_EXIT_OK" in self.capture(pane))

    @unittest.skipIf(os.name == "nt", "tmux supported here on Unix; WSL must be tested separately")
    def test_real_tmux_reads_unmodified_config(self):
        tmux = shutil.which("tmux")
        if not tmux:
            self.skipTest("tmux not installed")
        import shlex
        conf = self.runtime / "tmux.conf"
        conf.write_text("set -g prefix C-t\nset -g @crow-marker exact-config\nset -g status off\n")
        socket = "crow-e2e-" + str(os.getpid()) + "-" + str(time.monotonic_ns())
        self.addCleanup(lambda: subprocess.run([tmux, "-L", socket, "kill-server"], capture_output=True))
        pane = self.rpc("new-session", "tmux")["Created"]["pane"]
        command = " ".join(map(shlex.quote, [tmux, "-L", socket, "-f", str(conf), "new-session", "-s", "test"]))
        self.send(pane, command)
        def marker():
            result = subprocess.run([tmux, "-L", socket, "show-option", "-gv", "@crow-marker"], capture_output=True, text=True)
            return result.stdout.strip() == "exact-config"
        self.eventually(marker)
        self.assertEqual(conf.read_text(), "set -g prefix C-t\nset -g @crow-marker exact-config\nset -g status off\n")
        self.command("send-keys", pane, "\x14d")
        self.send(pane, "echo CROW_TMUX_DETACHED")
        self.eventually(lambda: "CROW_TMUX_DETACHED" in self.capture(pane))


if __name__ == "__main__":
    unittest.main(verbosity=2)
