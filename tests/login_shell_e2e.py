#!/usr/bin/env python3
"""Login shells, the way the user's own terminal starts them.

For every shell installed here (bash, zsh, fish, pwsh, sh, dash, ksh, mksh,
nu, xonsh), a daemon started with a stripped environment (launchd-like: no
TERM, no SHELL, PATH=/usr/bin:/bin) runs the configured shell as a login shell:
the pane sees the PATH entry and marker its login profile adds, and the
interactive rc's function/alias. A second test leaves the shell unset and checks
the user-database shell plus the daemon adopting the login environment.
"""
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

sys.dont_write_bytecode = True
sys.path.insert(0, str(Path(__file__).resolve().parent))
import daemon_e2e  # noqa: E402

BIN = daemon_e2e.BIN
POSIX_PROFILE = 'export PATH="$HOME/crow-login-bin:$PATH"\nexport CROW_RC_MARKER=login-profile\n'

# shell -> (rc files relative to HOME, probe command, login check output or None)
SHELLS = {
    "bash": ({".bash_profile": POSIX_PROFILE + '. "$HOME/.bashrc"\n',
              ".bashrc": "crowfn() { echo CROW_FN_OK; }\n"},
             'shopt -q login_shell && echo IS_LOGIN; crowfn; echo "M=$CROW_RC_MARKER P=$PATH"', "IS_LOGIN"),
    "zsh": ({".zprofile": POSIX_PROFILE, ".zshrc": "crowfn() { echo CROW_FN_OK; }\n"},
            '[[ -o login ]] && echo IS_LOGIN; crowfn; echo "M=$CROW_RC_MARKER P=$PATH"', "IS_LOGIN"),
    "fish": ({".config/fish/config.fish":
              "if status is-login\n  set -gx PATH $HOME/crow-login-bin $PATH\n  set -gx CROW_RC_MARKER login-profile\nend\n"
              "function crowfn; echo CROW_FN_OK; end\n"},
             'status is-login; and echo IS_LOGIN; crowfn; echo "M=$CROW_RC_MARKER P=$PATH"', "IS_LOGIN"),
    "pwsh": ({".config/powershell/Microsoft.PowerShell_profile.ps1":
              '$env:PATH = "$HOME/crow-login-bin:" + $env:PATH; $env:CROW_RC_MARKER = "login-profile"\n'
              "function crowfn { 'CROW_FN_OK' }\n"},
             'crowfn; echo "M=$env:CROW_RC_MARKER P=$env:PATH"', None),
    "sh": ({".profile": POSIX_PROFILE}, 'echo "M=$CROW_RC_MARKER P=$PATH"', None),
    "dash": ({".profile": POSIX_PROFILE}, 'echo "M=$CROW_RC_MARKER P=$PATH"', None),
    "ksh": ({".profile": POSIX_PROFILE}, 'echo "M=$CROW_RC_MARKER P=$PATH"', None),
    "mksh": ({".profile": POSIX_PROFILE}, 'echo "M=$CROW_RC_MARKER P=$PATH"', None),
}


def stripped_env(home, runtime):
    """What a GUI app gets from launchd: no TERM, no SHELL, a minimal PATH."""
    env = {"HOME": str(home), "PATH": "/usr/bin:/bin", "CROW_TERM_RUNTIME_DIR": str(runtime)}
    for key in ("USER", "LOGNAME", "TMPDIR", "LANG"):
        if key in os.environ:
            env[key] = os.environ[key]
    return env


@unittest.skipIf(os.name == "nt", "Unix login shells (Windows PowerShell loads profiles itself)")
class LoginShells(unittest.TestCase):
    def setUp(self):
        self.temp = Path(tempfile.mkdtemp(prefix="crow-login-"))
        self.addCleanup(shutil.rmtree, self.temp, True)

    def daemon(self, home, config_text, autostart=False):
        runtime = home.parent / f"rt-{home.name}"
        runtime.mkdir(mode=0o700)
        config = home.parent / f"{home.name}.toml"
        config.write_text(config_text)
        env = stripped_env(home, runtime)
        log = (home.parent / f"{home.name}-daemon.log").open("w+")
        self.addCleanup(log.close)
        daemon = None if autostart else subprocess.Popen(
            [str(BIN), "--config", str(config), "daemon"], env=env, stdout=log, stderr=log)

        def stop():
            subprocess.run([str(BIN), "stop", "--force"], env=env, capture_output=True, timeout=10)
            if daemon is None:
                return
            try:
                daemon.wait(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
        self.addCleanup(stop)

        def cli(*args):
            result = subprocess.run([str(BIN), "--config", str(config), *map(str, args)], env=env,
                                    capture_output=True, text=True, timeout=20)
            if result.returncode:
                raise AssertionError(f"{args}: {result.stderr}")
            return result.stdout
        if autostart:
            # Like the GUI: a client command starts the daemon (ensure_daemon).
            cli("list-sessions")
        deadline = time.monotonic() + 15
        while "daemon: running" not in cli("doctor"):
            if time.monotonic() > deadline:
                raise AssertionError("daemon did not start")
            time.sleep(0.1)
        return cli, runtime

    def wait_until_ready(self, cli, pane, prompt=None, quiet=0.8, timeout=40):
        deadline = time.monotonic() + timeout
        last, since = None, time.monotonic()
        while time.monotonic() < deadline:
            screen = cli("capture-pane", pane)
            if screen != last:
                last, since = screen, time.monotonic()
            elif screen.strip() and (prompt is None or prompt in screen) and time.monotonic() - since >= quiet:
                return screen
            time.sleep(0.1)
        raise AssertionError(f"shell never became ready (prompt {prompt!r}):\n{last}")

    def wait_for(self, cli, pane, needle, timeout=30):
        deadline = time.monotonic() + timeout
        screen = ""
        while time.monotonic() < deadline:
            screen = cli("capture-pane", pane)
            if needle in screen:
                return screen
            time.sleep(0.2)
        raise AssertionError(f"{needle!r} not on screen:\n{screen}")

    def test_each_installed_shell_starts_as_a_login_shell(self):
        found = []
        for name, (files, probe, login_mark) in SHELLS.items():
            program = shutil.which(name)
            if not program:
                continue
            found.append(name)
            with self.subTest(shell=name):
                home = self.temp / name
                (home / "crow-login-bin").mkdir(parents=True)
                for relative, text in files.items():
                    (home / relative).parent.mkdir(parents=True, exist_ok=True)
                    (home / relative).write_text(text)
                cli, _ = self.daemon(home, f'shell = "{program}"\n')
                doctor = cli("doctor")
                self.assertIn("; login shell)", doctor)
                pane = json.loads(cli("new-session", name))["Created"]["pane"]
                # Type only once the shell is ready: its prompt is drawn and the
                # screen has stopped changing (pwsh's PSReadLine on Linux
                # re-displays early typeahead without running it).
                self.wait_until_ready(cli, pane, prompt="PS " if name == "pwsh" else None)
                cli("send-keys", pane, probe, "--enter")
                screen = self.wait_for(cli, pane, "M=login-profile")
                # The PATH line wraps at 80 columns: compare the unwrapped text.
                flat = "".join(screen.splitlines())
                self.assertIn(str(home / "crow-login-bin"), flat[flat.index("M=login-profile"):],
                              f"{name}: login PATH entry missing")
                if "crowfn" in probe:
                    self.assertIn("CROW_FN_OK", screen, f"{name}: rc function missing")
                if login_mark:
                    self.assertIn(login_mark, screen, f"{name}: not a login shell")
        print(f"login shells exercised: {found}", file=sys.stderr)
        self.assertIn("sh", found)
        if os.environ.get("CROW_REQUIRE_SHELLS"):
            missing = set(os.environ["CROW_REQUIRE_SHELLS"].split(",")) - set(found)
            self.assertFalse(missing, f"required shells not installed: {missing}")

    def test_default_is_the_user_database_shell_and_the_daemon_adopts_its_login_env(self):
        home = self.temp / "default"
        (home / "crow-login-bin").mkdir(parents=True)
        for files, _, _ in (SHELLS["bash"], SHELLS["zsh"], SHELLS["fish"], SHELLS["sh"]):
            for relative, text in files.items():
                (home / relative).parent.mkdir(parents=True, exist_ok=True)
                (home / relative).write_text(text)
        cli, runtime = self.daemon(home, "", autostart=True)
        doctor = cli("doctor")
        self.assertIn("user database (login shell); login shell)", doctor)
        started = (runtime / "daemon.log").read_text()
        count = int(started.split("daemon starts with ")[1].split(" variable")[0]) if "daemon starts with" in started else 0
        self.assertGreater(count, 0, f"a daemon started without TERM must get the login environment: {started}")
        # A program started without a shell inherits the adopted environment.
        printenv = shutil.which("printenv", path="/usr/bin:/bin")
        pane = json.loads(cli("--shell", printenv, "new-session", "env"))["Created"]["pane"]
        screen = self.wait_for(cli, pane, "PATH=")
        self.assertIn(str(home / "crow-login-bin"), screen.replace("\n", ""))


if __name__ == "__main__":
    unittest.main(verbosity=2)
