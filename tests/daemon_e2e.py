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


def neovim_binary(test):
    """CROW_TERM_NVIM selects a pinned binary; CROW_TERM_REQUIRE_NVIM turns a skip into a failure."""
    required = os.environ.get("CROW_TERM_REQUIRE_NVIM")
    nvim = os.environ.get("CROW_TERM_NVIM") or (None if required else shutil.which("nvim"))
    if not nvim:
        if required:
            test.fail("CROW_TERM_REQUIRE_NVIM is set but CROW_TERM_NVIM names no binary")
        test.skipTest("nvim not installed")
    expected = os.environ.get("CROW_TERM_NVIM_VERSION")
    if expected:
        version = subprocess.run([nvim, "--version"], capture_output=True, text=True, timeout=20).stdout.splitlines()[0]
        test.assertEqual(version, f"NVIM v{expected}", "pinned Neovim version mismatch")
    return nvim


class DaemonHarness(unittest.TestCase):
    """Isolated runtime + daemon per test. Holds no tests itself, so other
    E2E modules can reuse it without re-running this module's tests."""
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="crow-e2e-")
        self.addCleanup(self.temp.cleanup)
        self.runtime = Path(self.temp.name)
        self.runtime.chmod(0o700)
        self.env = dict(os.environ, CROW_TERM_RUNTIME_DIR=str(self.runtime))
        self.shell = "powershell.exe" if os.name == "nt" else "/bin/sh"
        self.log = (self.runtime / "test-daemon.log").open("w+")
        self.addCleanup(self.log.close)
        self.daemon = subprocess.Popen([str(BIN), *self.daemon_args(), "daemon"], env=self.env, stdout=self.log, stderr=self.log)
        self.addCleanup(self.stop)
        # Wait with `doctor`, which never starts a daemon: any other command
        # autostarts one (with the default config) if it wins the race against
        # this daemon's bind, and the test would then talk to the wrong daemon.
        self.eventually(lambda: "daemon: running" in self.command("doctor").stdout, timeout=15)
        self.assertIsNone(self.daemon.poll(), f"test daemon exited: {self.daemon_log()}")

    def daemon_log(self):
        """The daemon's log so far (the shared handle's offset is at its end)."""
        self.log.flush()
        return (self.runtime / "test-daemon.log").read_text(errors="replace")[-4000:]

    def daemon_args(self):
        """Extra global CLI arguments for the daemon (e.g. --config)."""
        return []

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
        raise AssertionError(f"Condition timed out: {error}; daemon: {self.daemon_log()}; "
                             f"screens:\n{self.all_screens()}")

    def all_screens(self):
        """Every pane's text, for failure messages."""
        try:
            sessions = json.loads(self.command("list-sessions", check=False).stdout or "{}")
            panes = []
            for session in sessions.get("View", {}).get("sessions", []):
                for window in session["windows"]:
                    panes += re.findall(r'"Pane": (\d+)', json.dumps(window["layout"]).replace('"Pane":', '"Pane": '))
            return "\n".join(f"--- pane {p}\n{self.command('capture-pane', p, check=False).stdout}" for p in panes)
        except Exception as error:  # noqa: BLE001 - diagnostics only
            return f"(no screens: {error})"

    def capture(self, pane):
        return self.command("capture-pane", pane).stdout

    def output_line(self,pane,expected):
        # A command's terminal echo is not evidence that the shell executed it.
        return any(line.strip()==expected for line in self.capture(pane).splitlines())

    def send(self, pane, text):
        # A login shell reads its profile first: type once its prompt is drawn,
        # as a person would (input typed earlier may be read by the profile).
        if pane not in getattr(self, "_prompted", set()):
            self._prompted = getattr(self, "_prompted", set()) | {pane}
            until = time.monotonic() + 15
            while time.monotonic() < until and not self.capture(pane).strip():
                time.sleep(0.05)
        self.command("send-keys", pane, text, "--enter")


class DaemonTests(DaemonHarness):
    def test_session_window_split_reattach_resize_and_isolation(self):
        first = self.rpc("new-session", "alpha")["Created"]
        self.assertEqual(self.rpc("new-session", "alpha")["Created"], first)
        pane, window = first["pane"], first["window"]
        self.send(pane, "echo CROW_REAL_PTY")
        # A cold PowerShell start on a CI runner can take several seconds.
        self.eventually(lambda: self.output_line(pane,"CROW_REAL_PTY"), timeout=30)
        # Reattaching with another CLI default must not change a session's shell profile.
        second = self.rpc("--session", "alpha", "--shell", "crow-nonexistent-shell-fixture", "new-window")["Created"]
        self.send(second["pane"],"echo CROW_ORIGINAL_PROFILE")
        self.eventually(lambda: self.output_line(second["pane"],"CROW_ORIGINAL_PROFILE"))
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
        self.send(pane, "printf '\\033[4:3;58:2::1:2:3mCROW_CURL\\033[0m\\n'")
        self.eventually(lambda: self.output_line(pane, "CROW_CURL"))
        cells = [c for row in self.rpc("capture-pane", pane, "--json")["cells"] for c in row]
        curly = [c for c in cells if c["underline_style"] == "Curly"]
        self.assertEqual("".join(c["text"] for c in curly), "CROW_CURL")
        self.assertTrue(all(c["underline_color"] == {"Rgb": [1, 2, 3]} and c["underline"] for c in curly))
        self.command("kill-session", "pid")
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    @unittest.skipIf(os.name == "nt", "Neovim POSIX PTY fixture")
    def test_neovim_alternate_screen_input_and_clean_exit(self):
        nvim = neovim_binary(self)
        import shlex
        pane = self.rpc("new-session", "nvim")["Created"]["pane"]
        self.send(pane, shlex.quote(nvim) + " --clean -n -i NONE")
        self.eventually(lambda: self.rpc("capture-pane", pane, "--json")["modes"]["bracketed_paste"])
        self.command("send-keys", pane, "iCrow terminal é 界 -> != 🚀")
        self.eventually(lambda: "Crow terminal" in self.capture(pane))
        # Escape as the GUI encodes it for the keyboard mode Neovim enabled: a
        # raw ESC followed by ":" is ambiguous (Alt+:) under the kitty protocol.
        flags = self.rpc("capture-pane", pane, "--json")["modes"].get("keyboard", 0)
        escape = subprocess.run([str(BIN), "debug-encode-key", "Escape", "--flags", str(flags)],
                                capture_output=True, check=True, timeout=10).stdout.decode()
        self.command("send-keys", pane, escape)
        self.eventually(lambda: self.rpc("capture-pane", pane, "--json")["cursor"]["shape"] == "block")
        self.command("send-keys", pane, ":qa!\r")
        self.eventually(lambda: not self.rpc("capture-pane", pane, "--json")["modes"]["bracketed_paste"])
        self.send(pane, "echo CROW_NVIM_EXIT_OK")
        self.eventually(lambda: self.output_line(pane,"CROW_NVIM_EXIT_OK"))

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
        self.eventually(lambda: self.output_line(pane,"CROW_TMUX_DETACHED"))


class UnresponsiveDaemonTests(DaemonHarness):
    @unittest.skipIf(os.name == "nt", "POSIX stty/head/wc")
    def test_a_large_paste_into_a_program_that_is_not_reading_is_delivered_whole(self):
        """Repro: ~800 KB pasted while the program sleeps. The daemon used to
        give up after 5 s of PTY back-pressure and drop the rest; now every
        byte arrives, in order, once the program reads."""
        pane = self.rpc("new-session", "paste")["Created"]["pane"]
        total = 40 * 20000
        self.send(pane, f"stty raw -echo; sleep 8; head -c {total} | wc -c; stty sane")
        time.sleep(0.5)
        for index in range(40):
            result = subprocess.run([str(BIN), "send-keys", str(pane), "x" * 20000], env=self.env,
                                    capture_output=True, text=True, timeout=90)
            self.assertEqual(result.returncode, 0, f"chunk {index}: {result.stderr}")
        self.eventually(lambda: any(l.strip() == str(total) for l in self.capture(pane).splitlines()), timeout=60)


    @unittest.skipIf(os.name == "nt", "SIGSTOP")
    def test_a_frozen_daemon_is_never_replaced(self):
        """A live daemon that neither answers nor accepts (SIGSTOP + full accept
        backlog, so connecting is refused) keeps its socket and sessions; a CLI
        call waits and fails instead of starting a second daemon."""
        import signal
        import socket
        pane = self.rpc("new-session", "keep")["Created"]["pane"]
        path = next(self.runtime.glob("daemon-v*.sock"))
        held = []
        os.kill(self.daemon.pid, signal.SIGSTOP)
        try:
            refused = False
            for _ in range(4096):
                client = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
                client.setblocking(False)
                try:
                    client.connect(str(path))
                except BlockingIOError:
                    pass
                except (ConnectionRefusedError, OSError):
                    client.close()
                    refused = True
                    break
                held.append(client)
            result = None if not refused else subprocess.run([str(BIN), "list-sessions"], env=self.env, capture_output=True,
                                    text=True, timeout=60)
        finally:
            for client in held:
                client.close()
            os.kill(self.daemon.pid, signal.SIGCONT)
        if result is None:
            # Linux queues connections to a full Unix-socket backlog (EAGAIN)
            # instead of refusing them: a live daemon is never "unreachable".
            self.skipTest("this kernel does not refuse connections to a live socket")
        self.assertNotEqual(result.returncode, 0, result.stdout)
        # Busy, not gone: the refusal of a live daemon is never "not running".
        self.assertRegex(result.stderr, "not answering|alive but not accepting")
        self.assertNotIn("not running", result.stderr)
        self.assertTrue(path.exists(), "the live daemon's socket was removed")
        sessions = self.rpc("list-sessions")["View"]["sessions"]
        self.assertEqual([s["name"] for s in sessions], ["keep"], "the original daemon still serves")
        self.assertIsNone(self.daemon.poll())
        self.assertTrue(self.capture(pane) is not None)


class DebugEncodeKeyTests(unittest.TestCase):
    def test_key_encoding_aid_never_starts_a_daemon(self):
        with tempfile.TemporaryDirectory(prefix="crow-e2e-") as temp:
            runtime = Path(temp) / "runtime"
            env = dict(os.environ, CROW_TERM_RUNTIME_DIR=str(runtime))
            result = subprocess.run([str(BIN), "debug-encode-key", "/", "--mod", "ctrl", "--flags", "1"],
                                    env=env, capture_output=True, timeout=10)
            self.assertEqual(result.stdout, b"\x1b[47;5u", result.stderr)
            self.assertFalse(runtime.exists() and any(runtime.iterdir()), "a daemon runtime was created")


if __name__ == "__main__":
    unittest.main(verbosity=2)
