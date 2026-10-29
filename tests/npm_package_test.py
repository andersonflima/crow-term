#!/usr/bin/env python3
"""npm distribution: packages built from (fake) release archives, and the
launcher run through a node_modules layout like the one npm creates."""
import io
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tarfile
import tempfile
import unittest
import zipfile

ROOT = Path(__file__).resolve().parents[1]
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / "scripts"))
import npm_package  # noqa: E402

VERSION = "9.8.7-test.1"
FAKE = "#!/bin/sh\necho \"fake crow-term $*\"\nexit 7\n"
HOST = {("darwin", "arm64"), ("darwin", "x64"), ("linux", "x64"), ("linux", "arm64")}


def add(tar, name, data, mode=0o644):
    info = tarfile.TarInfo(name)
    info.size = len(data)
    info.mode = mode
    tar.addfile(info, io.BytesIO(data))


def fake_archives(dist):
    for target, (npm_os, _, extension) in npm_package.TARGETS.items():
        top = f"crow-term-{VERSION}-{target}"
        files = {f"{top}/{notice}": b"notice\n" for notice in npm_package.NOTICES}
        if npm_os == "darwin":
            files[f"{top}/{npm_package.APP}/Contents/MacOS/crow-term"] = FAKE.encode()
            files[f"{top}/{npm_package.APP}/Contents/Resources/shell/setup.py"] = b"# setup\n"
        else:
            files[f"{top}/crow-term" + (".exe" if npm_os == "win32" else "")] = FAKE.encode()
            files[f"{top}/shell/setup.py"] = b"# setup\n"
        path = dist / f"{top}.{extension}"
        if extension == "zip":
            with zipfile.ZipFile(path, "w") as bundle:
                for name, data in files.items():
                    bundle.writestr(name, data)
        else:
            with tarfile.open(path, "w:gz") as tar:
                for name, data in files.items():
                    add(tar, name, data, 0o755 if name.endswith("/crow-term") else 0o644)


class NpmPackages(unittest.TestCase):
    def setUp(self):
        self.temp = Path(tempfile.mkdtemp(prefix="crow-npm-"))
        self.addCleanup(shutil.rmtree, self.temp, True)
        self.dist = self.temp / "dist"
        self.dist.mkdir()
        fake_archives(self.dist)
        self.out = self.temp / "out"
        npm_package.main(["--dist", str(self.dist), "--out", str(self.out), "--version", VERSION])

    def manifest(self, name):
        return json.loads((self.out / name / "package.json").read_text())

    def test_manifests_pin_every_platform_package_exactly(self):
        main = self.manifest("crow-term")
        self.assertEqual(main["name"], "@andespindola/crow-term")
        self.assertEqual(main["bin"], {"crow-term": "bin/crow-term.js"})
        self.assertNotIn("scripts", main, "no install scripts")
        self.assertEqual(sorted(main["optionalDependencies"]), sorted(
            f"@andespindola/crow-term-{o}-{c}" for o, c, _ in npm_package.TARGETS.values()))
        self.assertEqual(set(main["optionalDependencies"].values()), {VERSION})
        for npm_os, cpu, _ in npm_package.TARGETS.values():
            platform = self.manifest(f"crow-term-{npm_os}-{cpu}")
            self.assertEqual((platform["os"], platform["cpu"], platform["version"]), ([npm_os], [cpu], VERSION))
            self.assertNotIn("scripts", platform)
            root = self.out / f"crow-term-{npm_os}-{cpu}"
            self.assertTrue((root / "LICENSE").is_file())
            if npm_os == "darwin":
                self.assertTrue((root / "bin" / npm_package.APP / "Contents/Resources/shell/setup.py").is_file())
            else:
                self.assertTrue((root / "bin/shell/setup.py").is_file())
        # Every launcher target names a package that is built.
        launcher = (self.out / "crow-term/bin/crow-term.js").read_text()
        for npm_os, cpu, _ in npm_package.TARGETS.values():
            self.assertIn(f'"@andespindola/crow-term-{npm_os}-{cpu}"', launcher)

    def test_missing_archive_fails_closed(self):
        (self.dist / f"crow-term-{VERSION}-linux-arm64.tar.gz").unlink()
        with self.assertRaises(SystemExit):
            npm_package.main(["--dist", str(self.dist), "--out", str(self.temp / "o2"), "--version", VERSION])

    @unittest.skipUnless(shutil.which("node"), "node not installed")
    def test_launcher_runs_the_platform_binary_with_arguments_and_exit_code(self):
        node = shutil.which("node")
        host = subprocess.run([node, "-p", "process.platform + ' ' + process.arch"],
                              capture_output=True, text=True, check=True).stdout.split()
        if tuple(host) not in HOST:
            self.skipTest(f"fake binary is a shell script; host {host}")
        modules = self.temp / "prefix/node_modules/@andespindola"
        modules.mkdir(parents=True)
        shutil.copytree(self.out / "crow-term", modules / "crow-term")
        shutil.copytree(self.out / f"crow-term-{host[0]}-{host[1]}", modules / f"crow-term-{host[0]}-{host[1]}")
        launcher = modules / "crow-term/bin/crow-term.js"
        result = subprocess.run([node, str(launcher), "--version", "a b"], capture_output=True, text=True)
        self.assertEqual(result.stdout.strip(), "fake crow-term --version a b")
        self.assertEqual(result.returncode, 7)
        # A signal sent to the launcher alone reaches the native process.
        binary = modules / f"crow-term-{host[0]}-{host[1]}" / (
            f"bin/{npm_package.APP}/Contents/MacOS/crow-term" if host[0] == "darwin" else "bin/crow-term")
        marker = self.temp / "got-term"
        # Bounded loop: a regressed launcher must fail the test, not hang it.
        binary.write_text(
            "#!/bin/sh\ntrap 'echo term > \"$CROW_MARK\"; exit 9' TERM\necho ready\n"
            "i=0; while [ $i -lt 200 ]; do sleep 0.05; i=$((i+1)); done\n")
        child = subprocess.Popen([node, str(launcher)], stdout=subprocess.PIPE, stdin=subprocess.DEVNULL,
                                 text=True, start_new_session=True, env=dict(os.environ, CROW_MARK=str(marker)))
        def reap():
            try:
                os.killpg(child.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            child.wait()
            child.stdout.close()
        self.addCleanup(reap)
        self.assertEqual(child.stdout.readline().strip(), "ready")
        child.send_signal(signal.SIGTERM)
        self.assertEqual(child.wait(timeout=10), 9)
        self.assertEqual(marker.read_text().strip(), "term")
        # Without the platform package the launcher explains instead of crashing.
        shutil.rmtree(modules / f"crow-term-{host[0]}-{host[1]}")
        result = subprocess.run([node, str(launcher)], capture_output=True, text=True)
        self.assertEqual(result.returncode, 1)
        self.assertIn("platform package", result.stderr)


if __name__ == "__main__":
    unittest.main(verbosity=2)
