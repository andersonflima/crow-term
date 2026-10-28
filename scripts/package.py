#!/usr/bin/env python3
"""Assemble a portable build with licenses and the offline shell setup; no install.

Usage (stdlib only, Python 3.9+):

    python scripts/package.py                 # stage + archive (default, used by CI)
    python scripts/package.py stage           # build the directory (and the macOS .app)
    python scripts/package.py archive         # archive an already staged (e.g. signed) tree

The release workflow runs ``stage``, signs/notarizes the macOS bundle in place,
then runs ``archive``. Asset names are stable:

    crow-term-<version>-<os>-<arch>.tar.gz   (macOS, Linux)
    crow-term-<version>-windows-<arch>.zip   (Windows)
    crow-term-<version>-macos-<arch>.app.zip (macOS, only the .app, for the cask)

each with a ``<asset>.sha256`` file. ``os`` is macos/linux/windows and ``arch``
is arm64/x86_64.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import platform
import plistlib
import re
import shutil
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
APP_NAME = "Crow Term.app"
BUNDLE_ID = "dev.crow.crow-term"
ARCHES = {"arm64": "arm64", "aarch64": "arm64", "x86_64": "x86_64", "amd64": "x86_64"}
SYSTEMS = {"Darwin": "macos", "Linux": "linux", "Windows": "windows"}
SHIPPED_DIRECTORIES = ["shell", "assets", "docs"]
SHIPPED_FILES = ["LICENSE", "NOTICE.md", "THIRD_PARTY_LICENSES.txt", "README.md", "Cargo.lock"]
LICENSE_FILES = ["LICENSE", "NOTICE.md", "THIRD_PARTY_LICENSES.txt"]
IGNORED = shutil.ignore_patterns("__pycache__", "*.pyc", ".DS_Store")


def cargo_version(root=ROOT):
    """Read [package].version from Cargo.toml without third-party parsers."""
    text = (root / "Cargo.toml").read_text(encoding="utf-8")
    section = None
    for line in text.splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            section = stripped
            continue
        match = re.match(r'version\s*=\s*"([^"]+)"', stripped)
        if section == "[package]" and match:
            return match.group(1)
    raise SystemExit("Cargo.toml has no [package] version")


def short_version(version):
    """CFBundleShortVersionString accepts only up to three integers (0.1.0-alpha.1 -> 0.1.0)."""
    match = re.match(r"(\d+)\.(\d+)\.(\d+)", version)
    if not match:
        raise SystemExit(f"Version is not semver: {version}")
    return ".".join(match.groups())


def detect_target():
    system = SYSTEMS.get(platform.system())
    arch = ARCHES.get(platform.machine().lower())
    if not system or not arch:
        raise SystemExit(f"Unsupported platform {platform.system()} {platform.machine()}")
    return system, arch


def package_name(version, system, arch):
    return f"crow-term-{version}-{system}-{arch}"


def info_plist(version, build_number, icon_file):
    plist = {
        "CFBundleName": "Crow Term",
        "CFBundleDisplayName": "Crow Term",
        "CFBundleIdentifier": BUNDLE_ID,
        "CFBundleExecutable": "crow-term",
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": short_version(version),
        "CFBundleVersion": str(build_number),
        "CFBundleInfoDictionaryVersion": "6.0",
        "LSApplicationCategoryType": "public.app-category.developer-tools",
        "NSHighResolutionCapable": True,
        "LSMinimumSystemVersion": "11.0",
    }
    if icon_file:
        plist["CFBundleIconFile"] = icon_file
    return plist


def find_icon(root):
    icons = sorted((root / "assets").rglob("*.icns"))
    if len(icons) > 1:
        raise SystemExit(f"Ambiguous app icon, expected at most one .icns: {icons}")
    return icons[0] if icons else None


def build_app(dest, binary_path, version, build_number, root):
    """Self-contained bundle: nothing inside it points outside it (required by codesign)."""
    contents = dest / APP_NAME / "Contents"
    (contents / "MacOS").mkdir(parents=True)
    resources = contents / "Resources"
    resources.mkdir()
    executable = contents / "MacOS/crow-term"
    shutil.copy2(binary_path, executable)
    # Data lives in Resources, never in MacOS: codesign treats MacOS entries as code.
    # The binary finds Contents/Resources/shell/setup.py (see bundled_shell_setup_candidates).
    shutil.copytree(root / "shell", resources / "shell", ignore=IGNORED)
    for file in LICENSE_FILES:
        shutil.copy2(root / file, resources / file)
    icon = find_icon(root)
    if icon:
        shutil.copy2(icon, resources / icon.name)
    with (contents / "Info.plist").open("wb") as file:
        plistlib.dump(info_plist(version, build_number, icon.name if icon else None), file)
    # Seal the bundle ad hoc: the linker-only signature of an arm64 binary leaves the
    # bundle invalid ("code has no resources"), which macOS reports as damaged. The
    # release workflow replaces this with a Developer ID signature when configured.
    if shutil.which("codesign"):
        subprocess.run(["codesign", "--force", "--sign", "-", str(dest / APP_NAME)], check=True)
    # Portable-archive entry point outside the bundle; a relative link into it is fine.
    (dest / "crow-term").symlink_to(f"{APP_NAME}/Contents/MacOS/crow-term")
    return dest / APP_NAME


def stage(args):
    system, arch = detect_target()
    dest = args.dist / package_name(args.version, system, arch)
    args.dist.mkdir(parents=True, exist_ok=True)
    dest.mkdir(exist_ok=False)  # fail closed rather than merging with old packaging files
    binary = "crow-term.exe" if system == "windows" else "crow-term"
    binary_path = args.target_dir / binary
    if not binary_path.is_file():
        raise SystemExit(f"Missing release binary {binary_path}; run cargo build --release --locked")
    for directory in SHIPPED_DIRECTORIES:
        shutil.copytree(args.root / directory, dest / directory, ignore=IGNORED)
    for file in SHIPPED_FILES:
        shutil.copy2(args.root / file, dest / file)
    if system == "macos":
        app = build_app(dest, binary_path, args.version, args.build_number, args.root)
        print(f"staged {dest}\napp {app}")
    else:
        shutil.copy2(binary_path, dest / binary)
        print(f"staged {dest}")
    return dest


def write_checksum(path):
    digest = hashlib.sha256(path.read_bytes()).hexdigest()
    Path(f"{path}.sha256").write_text(f"{digest}  {path.name}\n", encoding="utf-8")
    return digest


def archive(args):
    system, arch = detect_target()
    name = package_name(args.version, system, arch)
    dest = args.dist / name
    if not dest.is_dir():
        raise SystemExit(f"Nothing staged at {dest}; run the stage step first")
    outputs = []
    kind = "zip" if system == "windows" else "gztar"
    outputs.append(Path(shutil.make_archive(str(args.dist / name), kind, root_dir=args.dist, base_dir=name)))
    if system == "macos":
        # ditto keeps the bundle layout, extended attributes and the stapled ticket.
        app_zip = args.dist / f"{name}.app.zip"
        subprocess.run(["ditto", "-c", "-k", "--sequesterRsrc", "--keepParent",
                        str(dest / APP_NAME), str(app_zip)], check=True)
        outputs.append(app_zip)
    result = [{"file": out.name, "sha256": write_checksum(out)} for out in outputs]
    print(json.dumps(result, indent=2))
    return result


def parse(argv):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("step", nargs="?", choices=("all", "stage", "archive"), default="all")
    parser.add_argument("--version", default=None, help="defaults to Cargo.toml [package].version")
    parser.add_argument("--build-number", default=os.environ.get("CROW_BUILD_NUMBER", "1"),
                        help="CFBundleVersion (default: $CROW_BUILD_NUMBER or 1)")
    parser.add_argument("--dist", type=Path, default=ROOT / "dist")
    parser.add_argument("--target-dir", type=Path, default=ROOT / "target/release")
    parser.add_argument("--root", type=Path, default=ROOT, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)
    args.version = args.version or cargo_version(args.root)
    if not re.fullmatch(r"\d+", str(args.build_number)):
        parser.error("--build-number must be a positive integer")
    short_version(args.version)
    return args


def main(argv=None):
    args = parse(sys.argv[1:] if argv is None else argv)
    if args.step in ("all", "stage"):
        stage(args)
    if args.step in ("all", "archive"):
        archive(args)


if __name__ == "__main__":
    main()
