#!/usr/bin/env python3
"""Render the Homebrew formula/cask against fake assets; stdlib unittest only."""
import hashlib
import io
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "scripts"))
import update_tap  # noqa: E402

VERSION = "0.1.0-alpha.1"


class UpdateTapTest(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        base = Path(self.temp.name)
        self.assets, self.tap = base / "assets", base / "tap"
        self.assets.mkdir()
        (self.tap / "Formula").mkdir(parents=True)
        self.digests = {}
        for token, name in update_tap.asset_names(VERSION).items():
            payload = f"fake {name}".encode()
            (self.assets / name).write_bytes(payload)
            self.digests[token] = hashlib.sha256(payload).hexdigest()

    def tearDown(self):
        self.temp.cleanup()

    def run_tool(self, *extra):
        out = io.StringIO()
        changed = update_tap.run(["--version", VERSION, "--tap", str(self.tap), *extra], out=out)
        return changed, out.getvalue()

    def test_renders_formula_and_cask_from_assets_dir(self):
        changed, output = self.run_tool("--assets-dir", str(self.assets))
        self.assertEqual(changed, ["Formula/crow-term.rb", "Casks/crow-term.rb"])
        formula = (self.tap / "Formula/crow-term.rb").read_text()
        cask = (self.tap / "Casks/crow-term.rb").read_text()
        # Formula version comes from the URL (brew audit: an explicit version is redundant).
        self.assertNotIn('version "', formula)
        self.assertIn(f"/download/v{VERSION}/crow-term-{VERSION}-macos-arm64.tar.gz", formula)
        for token in ("SHA256_MACOS_ARM64", "SHA256_MACOS_X86_64", "SHA256_LINUX_ARM64", "SHA256_LINUX_X86_64"):
            self.assertIn(self.digests[token], formula)
        self.assertIn(self.digests["SHA256_MACOS_ARM64_APP"], cask)
        self.assertIn(self.digests["SHA256_MACOS_X86_64_APP"], cask)
        self.assertNotIn("@", formula.replace("#{", ""))
        self.assertIn("caveats <<~EOS", cask)  # not notarized by default
        self.assertTrue(formula.startswith("# Generated"))
        self.assertIn("git -C", output)
        self.assertNotIn("Template rendered", formula + cask)

    def test_notarized_drops_caveat_and_is_idempotent(self):
        self.run_tool("--assets-dir", str(self.assets), "--notarized")
        cask = (self.tap / "Casks/crow-term.rb").read_text()
        self.assertNotIn("caveats", cask)
        self.assertTrue(cask.rstrip().endswith("]\nend"))
        changed, output = self.run_tool("--assets-dir", str(self.assets), "--notarized")
        self.assertEqual(changed, [])
        self.assertIn("unchanged Formula/crow-term.rb", output)

    def test_sha256sums_file_and_dry_run_writes_nothing(self):
        sums = self.assets / "SHA256SUMS"
        sums.write_text("".join(f"{self.digests[t]}  {n}\n" for t, n in update_tap.asset_names(VERSION).items()))
        changed, output = self.run_tool("--sha256sums", str(sums), "--dry-run")
        self.assertEqual(len(changed), 2)
        self.assertIn("+++ b/Casks/crow-term.rb", output)
        self.assertFalse((self.tap / "Formula/crow-term.rb").exists())
        self.assertFalse((self.tap / "Casks").exists())

    def test_refuses_missing_asset(self):
        (self.assets / f"crow-term-{VERSION}-linux-arm64.tar.gz").unlink()
        with self.assertRaisesRegex(update_tap.TapError, "linux-arm64"):
            self.run_tool("--assets-dir", str(self.assets))
        self.assertFalse((self.tap / "Formula/crow-term.rb").exists())

    def cask_only_assets(self):
        for token, name in update_tap.asset_names(VERSION).items():
            if not token.endswith("_APP"):
                (self.assets / name).unlink()

    def test_cask_only_requires_only_app_archives_and_preserves_formula(self):
        self.cask_only_assets()
        formula = self.tap / "Formula/crow-term.rb"
        sentinel = b"# Previous complete release\r\nclass CrowTerm < Formula\r\nend\r\n"
        formula.write_bytes(sentinel)
        changed, output = self.run_tool("--assets-dir", str(self.assets), "--cask-only", "--notarized")
        self.assertEqual(changed, ["Casks/crow-term.rb"])
        self.assertEqual(formula.read_bytes(), sentinel)
        cask = (self.tap / "Casks/crow-term.rb").read_text()
        self.assertIn(self.digests["SHA256_MACOS_ARM64_APP"], cask)
        self.assertIn(self.digests["SHA256_MACOS_X86_64_APP"], cask)
        self.assertNotIn("caveats", cask)
        self.assertNotIn("Formula/crow-term.rb", output)
        changed, _ = self.run_tool("--assets-dir", str(self.assets), "--cask-only", "--notarized")
        self.assertEqual(changed, [])
        self.assertEqual(formula.read_bytes(), sentinel)

    def test_cask_only_sha256sums_dry_run_needs_only_app_archives(self):
        self.cask_only_assets()
        sums = self.assets / "SHA256SUMS"
        sums.write_text("".join(f"{self.digests[t]}  {n}\n" for t, n in update_tap.asset_names(VERSION).items()
                                if t.endswith("_APP")))
        changed, output = self.run_tool("--sha256sums", str(sums), "--cask-only", "--dry-run")
        self.assertEqual(changed, ["Casks/crow-term.rb"])
        self.assertIn("+++ b/Casks/crow-term.rb", output)
        self.assertNotIn("Formula/crow-term.rb", output)
        self.assertFalse((self.tap / "Formula/crow-term.rb").exists())
        self.assertFalse((self.tap / "Casks").exists())

    def test_cask_only_refuses_missing_app_archive(self):
        self.cask_only_assets()
        for arch in ("arm64", "x86_64"):
            with self.subTest(arch=arch):
                name = f"crow-term-{VERSION}-macos-{arch}.app.zip"
                path = self.assets / name
                payload = path.read_bytes()
                path.unlink()
                with self.assertRaisesRegex(update_tap.TapError, name):
                    self.run_tool("--assets-dir", str(self.assets), "--cask-only", "--notarized")
                self.assertFalse((self.tap / "Casks").exists())
                path.write_bytes(payload)

    def test_refuses_mismatched_sidecar(self):
        name = f"crow-term-{VERSION}-macos-arm64.tar.gz"
        (self.assets / f"{name}.sha256").write_text("0" * 64 + f"  {name}\n")
        with self.assertRaisesRegex(update_tap.TapError, "does not match"):
            self.run_tool("--assets-dir", str(self.assets))

    def test_rejects_bad_version_and_plain_http(self):
        # Same grammar as the release workflow's tag check (hyphens allowed in pre-release).
        self.assertTrue(update_tap.VERSION_RE.fullmatch("0.1.0-rc-1"))
        for bad in ("1.0", "latest", "1.0.0;rm", "1.0.0-", "1.0.0-a b"):
            with self.assertRaises(update_tap.TapError):
                update_tap.run(["--version", bad, "--tap", str(self.tap), "--assets-dir", str(self.assets)],
                               out=io.StringIO())
        with self.assertRaisesRegex(update_tap.TapError, "non-HTTPS"):
            self.run_tool("--sha256sums", "http://example.invalid/SHA256SUMS")

    def test_accepts_leading_v(self):
        changed = update_tap.run(["--version", f"v{VERSION}", "--tap", str(self.tap),
                                     "--assets-dir", str(self.assets)], out=io.StringIO())
        self.assertEqual(len(changed), 2)


if __name__ == "__main__":
    unittest.main()
