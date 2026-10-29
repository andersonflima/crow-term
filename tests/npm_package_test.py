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
        for notice in npm_package.ASSET_NOTICES:
            files[f"{top}/{notice}"] = b"asset notice\n"
        if npm_os == "darwin":
            files[f"{top}/{npm_package.APP}/Contents/MacOS/crow-term"] = FAKE.encode()
            files[f"{top}/{npm_package.APP}/Contents/Resources/shell/setup.py"] = b"# setup\n"
            files[f"{top}/{npm_package.APP}/Contents/Resources/THIRD_PARTY_LICENSES.txt"] = b"third\n"
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


def fake_status(directory):
    directory.mkdir()
    for target, (npm_os, _, _) in npm_package.TARGETS.items():
        status = {"target": target, "signed": npm_os == "darwin", "notarized": npm_os == "darwin"}
        if npm_os == "linux":
            status["glibc"] = "2.35"
        (directory / f"{target}.json").write_text(json.dumps(status))


class NpmPackages(unittest.TestCase):
    def setUp(self):
        self.temp = Path(tempfile.mkdtemp(prefix="crow-npm-"))
        self.addCleanup(shutil.rmtree, self.temp, True)
        self.dist = self.temp / "dist"
        self.dist.mkdir()
        fake_archives(self.dist)
        self.status = self.temp / "status"
        fake_status(self.status)
        self.out = self.temp / "out"
        npm_package.main(["--dist", str(self.dist), "--out", str(self.out), "--version", VERSION,
                          "--status", str(self.status)])

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

    def test_platform_packages_ship_the_exact_binary_notices_and_libc(self):
        expected = {
            "darwin": "bin/Crow Term.app/Contents/MacOS/crow-term",
            "linux": "bin/crow-term",
            "win32": "bin/crow-term.exe",
        }
        for npm_os, cpu, _ in npm_package.TARGETS.values():
            root = self.out / f"crow-term-{npm_os}-{cpu}"
            platform = self.manifest(f"crow-term-{npm_os}-{cpu}")
            self.assertEqual(npm_package.binary_path(npm_os), expected[npm_os])
            binary = root / expected[npm_os]
            self.assertTrue(binary.is_file(), binary)
            # Windows file systems keep no exec bits; packages are built on Linux.
            if npm_os != "win32" and os.name != "nt":
                self.assertTrue(binary.stat().st_mode & 0o111, f"{binary} is not executable")
            # Everything `files` names exists, and every shipped notice is listed.
            for entry in platform["files"]:
                self.assertTrue((root / entry).exists(), f"{npm_os}-{cpu}: {entry}")
            for notice in npm_package.ASSET_NOTICES:
                self.assertIn(notice, platform["files"])
                self.assertTrue((root / notice).is_file())
            if npm_os == "darwin":
                # One copy of the third-party texts: the one inside the bundle.
                self.assertNotIn("THIRD_PARTY_LICENSES.txt", platform["files"])
                self.assertFalse((root / "THIRD_PARTY_LICENSES.txt").exists())
                self.assertTrue((root / npm_package.BUNDLED_THIRD_PARTY).is_file())
            else:
                self.assertIn("THIRD_PARTY_LICENSES.txt", platform["files"])
            if npm_os == "linux":
                self.assertEqual(platform["libc"], ["glibc"])
                self.assertEqual(platform["crowTerm"], {"glibc": "2.35"})
                self.assertIn("glibc 2.35", (root / "README.md").read_text())
            else:
                self.assertNotIn("libc", platform)
        main = self.manifest("crow-term")
        for notice in npm_package.ASSET_NOTICES:
            self.assertIn(notice, main["files"])
            self.assertTrue((self.out / "crow-term" / notice).is_file())

    @unittest.skipUnless(shutil.which("npm"), "npm not installed")
    def test_npm_pack_lists_the_binary_with_its_exec_bit(self):
        npm = shutil.which("npm")
        for npm_os, cpu, _ in npm_package.TARGETS.values():
            root = self.out / f"crow-term-{npm_os}-{cpu}"
            result = subprocess.run([npm, "pack", "--dry-run", "--json"], cwd=root,
                                    capture_output=True, text=True, check=True)
            files = {entry["path"]: entry["mode"] for entry in json.loads(result.stdout)[0]["files"]}
            binary = npm_package.binary_path(npm_os)
            self.assertIn(binary, files, f"{npm_os}-{cpu}: {sorted(files)}")
            if npm_os != "win32" and os.name != "nt":
                self.assertTrue(files[binary] & 0o111, f"{npm_os}-{cpu}: mode {oct(files[binary])}")
            for notice in npm_package.ASSET_NOTICES:
                self.assertIn(notice, files)

    def test_linux_packages_need_the_recorded_glibc(self):
        (self.status / "linux-arm64.json").write_text(json.dumps({"target": "linux-arm64"}))
        with self.assertRaises(SystemExit):
            npm_package.main(["--dist", str(self.dist), "--out", str(self.temp / "o3"), "--version", VERSION,
                              "--status", str(self.status)])

    def test_missing_archive_fails_closed(self):
        (self.dist / f"crow-term-{VERSION}-linux-arm64.tar.gz").unlink()
        with self.assertRaises(SystemExit):
            npm_package.main(["--dist", str(self.dist), "--out", str(self.temp / "o2"), "--version", VERSION,
                              "--status", str(self.status)])

    @unittest.skipUnless(shutil.which("node"), "node not installed")
    def test_launcher_mapping_and_glibc_check_on_any_host(self):
        launcher = self.out / "crow-term/bin/crow-term.js"
        script = """
const l = require(process.argv[1]);
const out = {};
for (const [p, a] of [["darwin","arm64"],["darwin","x64"],["linux","arm64"],["linux","x64"],["win32","x64"],["linux","ia32"],["freebsd","x64"]]) {
  const t = l.target(p, a);
  out[p + " " + a] = t instanceof Error ? "error: " + t.message : t;
}
out.glibc = [
  l.glibcProblem("2.35", "2.39"), l.glibcProblem("2.35", "2.35"), l.glibcProblem("2.35", "2.9"),
  l.glibcProblem("2.35", "2.31"), l.glibcProblem("2.35", undefined), l.glibcProblem(undefined, "2.31"),
];
process.stdout.write(JSON.stringify(out));
"""
        result = subprocess.run([shutil.which("node"), "-e", script, str(launcher)],
                                capture_output=True, text=True, check=True)
        out = json.loads(result.stdout)
        for npm_os, cpu, _ in npm_package.TARGETS.values():
            self.assertEqual(out[f"{npm_os} {cpu}"],
                             [f"@andespindola/crow-term-{npm_os}-{cpu}", npm_package.binary_path(npm_os)])
        self.assertTrue(out["linux ia32"].startswith("error: Crow Term has no prebuilt binary for linux ia32"))
        self.assertTrue(out["freebsd x64"].startswith("error:"))
        ok, same, old_minor, older, musl, unknown = out["glibc"]
        self.assertIsNone(ok)
        self.assertIsNone(same)
        # Numeric comparison: 2.9 is older than 2.35.
        self.assertIn("needs glibc >= 2.35; this system has glibc 2.9", old_minor)
        self.assertIn("needs glibc >= 2.35; this system has glibc 2.31", older)
        self.assertIn("needs glibc >= 2.35, and this system has no glibc", musl)
        self.assertIsNone(unknown)

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
