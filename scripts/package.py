#!/usr/bin/env python3
"""Assemble a portable build with licenses and the offline shell setup; no install."""
import hashlib
from pathlib import Path
import platform
import plistlib
import shutil

root = Path(__file__).resolve().parents[1]
system = {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}[platform.system()]
arch = platform.machine().lower()
name = f"crow-term-{system}-{arch}"
dist = root / "dist"
dest = dist / name
dest.mkdir(parents=True, exist_ok=False) # fail closed rather than merging with old packaging files
binary = "crow-term.exe" if system == "windows" else "crow-term"
shutil.copy2(root / "target/release" / binary, dest / binary)
for directory in ["shell", "assets", "docs"]:
    shutil.copytree(root / directory, dest / directory, dirs_exist_ok=True,
                    ignore=shutil.ignore_patterns("__pycache__", "*.pyc"))
for file in ["LICENSE", "NOTICE.md", "README.md", "Cargo.lock"]:
    shutil.copy2(root / file, dest / file)
if system == "macos":
    contents = dest / "Crow Term.app/Contents"
    (contents / "MacOS").mkdir(parents=True, exist_ok=True)
    executable = contents / "MacOS/crow-term"
    # A real executable inside the bundle is required for native macOS app discovery.
    shutil.move(str(dest / binary), executable)
    (dest / binary).symlink_to("Crow Term.app/Contents/MacOS/crow-term")
    (contents / "MacOS/shell").symlink_to("../../../shell", target_is_directory=True)
    with (contents / "Info.plist").open("wb") as file:
        plistlib.dump({"CFBundleName": "Crow Term", "CFBundleDisplayName": "Crow Term",
                      "CFBundleIdentifier": "dev.crow.crow-term", "CFBundleExecutable": "crow-term",
                      "CFBundlePackageType": "APPL", "CFBundleShortVersionString": "0.1.0",
                      "CFBundleVersion": "1", "NSHighResolutionCapable": True,
                      "LSMinimumSystemVersion": "11.0"}, file)
archive = Path(shutil.make_archive(str(dist / name), "zip" if system == "windows" else "gztar",
                                   root_dir=dist, base_dir=name))
digest = hashlib.sha256(archive.read_bytes()).hexdigest()
archive.with_suffix(archive.suffix + ".sha256").write_text(f"{digest}  {archive.name}\n")
print(archive.name, digest)
