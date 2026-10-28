#!/usr/bin/env python3
"""Offline shell-profile regression tests; all mutations use TemporaryDirectory."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
spec = importlib.util.spec_from_file_location("crow_shell_setup", ROOT / "shell/setup.py")
setup = importlib.util.module_from_spec(spec)
spec.loader.exec_module(setup)


class ShellSetupTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(prefix="crow-shell-test-")
        self.addCleanup(self.temp.cleanup)
        self.base = Path(self.temp.name).resolve()
        self.prefix = self.base / "profile space 'quote; dollar$"

    def run_setup(self, shell="fish", *extra):
        with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
            return setup.main(["--shell", shell, "--prefix", str(self.prefix), *extra])

    def test_dry_run_writes_nothing_even_with_plugins(self):
        self.assertEqual(self.run_setup("fish", "--install-plugins", "--dry-run"), 0)
        self.assertFalse(self.prefix.exists())
        self.assertEqual(list(self.base.iterdir()), [])

    def test_idempotent_and_no_mode_or_content_overwrite(self):
        self.assertEqual(self.run_setup("fish", "--install-plugins"), 0)
        file = self.prefix / "fish/config.fish"
        file.chmod(0o640)
        before = {str(p): (p.read_bytes(), p.stat().st_mtime_ns, stat.S_IMODE(p.stat().st_mode))
                  for p in self.prefix.rglob("*") if p.is_file()}
        self.assertEqual(self.run_setup("fish", "--install-plugins"), 0)
        after = {str(p): (p.read_bytes(), p.stat().st_mtime_ns, stat.S_IMODE(p.stat().st_mode))
                 for p in self.prefix.rglob("*") if p.is_file()}
        self.assertEqual(before, after)
        self.assertEqual(stat.S_IMODE((self.prefix / "launch-fish.py").stat().st_mode), 0o700)
        self.assertEqual(stat.S_IMODE(self.prefix.stat().st_mode), 0o700)

    def test_conflict_preflight_does_not_write_any_other_file(self):
        existing = self.prefix / "fish/config.fish"
        existing.parent.mkdir(parents=True)
        existing.write_text("# owner's file\n")
        self.assertEqual(self.run_setup("fish", "--install-plugins"), 1)
        self.assertEqual(existing.read_text(), "# owner's file\n")
        self.assertFalse((self.prefix / "launch-fish.py").exists())
        self.assertFalse((self.prefix / "plugins").exists())

    def test_reject_file_parent_and_prefix_symlinks(self):
        outside = self.base / "outside"
        outside.mkdir()
        self.prefix.symlink_to(outside, target_is_directory=True)
        self.assertEqual(self.run_setup(), 1)
        self.prefix.unlink()
        self.prefix.mkdir()
        (self.prefix / "fish").symlink_to(outside, target_is_directory=True)
        self.assertEqual(self.run_setup(), 1)
        (self.prefix / "fish").unlink()
        (self.prefix / "fish").mkdir()
        target = outside / "original"
        target.write_text("untouched")
        (self.prefix / "fish/config.fish").symlink_to(target)
        self.assertEqual(self.run_setup(), 1)
        self.assertEqual(target.read_text(), "untouched")
        self.assertEqual(sorted(p.name for p in outside.iterdir()), ["original"])

    def test_plugins_are_explicit_and_shell_specific(self):
        self.assertEqual(self.run_setup(), 0)
        self.assertFalse((self.prefix / "plugins").exists())
        self.assertEqual(self.run_setup("fish", "--install-plugins"), 0)
        self.assertTrue((self.prefix / "plugins/fisher/functions/fisher.fish").exists())
        self.assertTrue((self.prefix / "plugins/z/functions/__z.fish").exists())
        self.assertFalse((self.prefix / "plugins/zsh-autosuggestions").exists())
        self.assertEqual(self.run_setup("zsh", "--install-plugins"), 0)
        self.assertTrue((self.prefix / "plugins/zsh-autosuggestions/LICENSE").exists())
        self.assertTrue((self.prefix / "plugins/zsh-syntax-highlighting/COPYING.md").exists())

    def test_tampered_vendor_hash_fails_closed(self):
        fake = self.base / "bad-vendor"
        (fake / "vendor/fisher/functions").mkdir(parents=True)
        (fake / "vendor/fisher/functions/fisher.fish").write_text("tampered")
        (fake / "plugins.lock.json").write_text(json.dumps({"schema": 1, "plugins": [{
            "name": "fisher", "files": {"functions/fisher.fish": "0" * 64}}]}))
        with patch.object(setup, "HERE", fake):
            self.assertEqual(self.run_setup("fish", "--install-plugins"), 1)
        self.assertFalse(self.prefix.exists())

    def test_atomic_publish_never_replaces_a_racing_file(self):
        self.prefix.mkdir()
        real_link = os.link

        def race(source, dest):
            Path(dest).write_text("concurrent owner")
            return real_link(source, dest)

        with patch.object(setup.os, "link", race):
            with self.assertRaises(ValueError):
                setup.write_new(self.prefix, "racing", b"new", 0o600)
        self.assertEqual((self.prefix / "racing").read_text(), "concurrent owner")
        self.assertFalse(list(self.prefix.glob(".crow-term-*")))

    def test_launcher_uses_argument_array_and_isolated_environment(self):
        self.assertEqual(self.run_setup(), 0)
        bin_dir = self.base / "bin space"
        bin_dir.mkdir()
        fake_fish = bin_dir / "fish"
        output = self.base / "result.json"
        fake_fish.write_text("#!" + sys.executable + "\nimport json, os, sys\n"
                             "open(os.environ['CROW_TEST_OUTPUT'], 'w').write(json.dumps({'args': sys.argv[1:], 'config': os.environ['XDG_CONFIG_HOME']}))\n")
        fake_fish.chmod(0o700)
        env = dict(os.environ, PATH=str(bin_dir) + os.pathsep + os.environ.get("PATH", ""), CROW_TEST_OUTPUT=str(output))
        subprocess.run([sys.executable, str(self.prefix / "launch-fish.py"), "--version"], env=env, check=True)
        result = json.loads(output.read_text())
        self.assertEqual(result["args"], ["--no-config", "--init-command", "source " + setup.quote_fish(self.prefix / "fish/config.fish"), "--version"])
        self.assertEqual(result["config"], str(self.prefix / "config"))

    def test_generated_profiles_parse_in_available_shells(self):
        count = 0
        for shell, relative in [("fish", "fish/config.fish"), ("zsh", "zsh/.zshrc")]:
            program = shutil.which(shell)
            if not program:
                continue
            count += 1
            self.assertEqual(self.run_setup(shell, "--install-plugins"), 0)
            flags = ["--no-config", "-n"] if shell == "fish" else ["-d", "-f", "-n"]
            result = subprocess.run([program, *flags, str(self.prefix / relative)], capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
        if not count:
            self.skipTest("neither fish nor zsh installed; syntax check not run")

    def test_pinned_plugins_load_only_in_explicit_isolated_launch(self):
        count = 0
        checks = {
            "fish": "functions -q fisher; and functions -q z",
            "zsh": "(( $+functions[_zsh_autosuggest_start] )) && (( $+functions[_zsh_highlight] ))",
        }
        clean_home = self.base / "clean home"
        clean_home.mkdir()
        for shell, command in checks.items():
            if not shutil.which(shell):
                continue
            count += 1
            self.assertEqual(self.run_setup(shell, "--install-plugins"), 0)
            result = subprocess.run([sys.executable, str(self.prefix / f"launch-{shell}.py"), "-c", command],
                                    env=dict(os.environ, HOME=str(clean_home)), capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertNotIn("not found", result.stderr)
        self.assertEqual(list(clean_home.iterdir()), [])
        if not count:
            self.skipTest("neither fish nor zsh installed; runtime check not run")


if __name__ == "__main__":
    unittest.main(verbosity=2)
