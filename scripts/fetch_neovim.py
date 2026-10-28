#!/usr/bin/env python3
"""Download an official Neovim release archive pinned by SHA-256 and unpack it.

Only the archives listed in PINS are accepted. The digest is checked before the
archive is opened, so a changed upstream asset fails instead of being executed.
Prints the absolute path of the unpacked `nvim` executable on stdout.
"""
import argparse
import hashlib
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tarfile
import tempfile
import urllib.request
import zipfile

# Upstream: https://github.com/neovim/neovim/releases (asset digests as published).
PINS = {
    "0.11.7": {
        "nvim-linux-x86_64.tar.gz": "38a7c6317f94503841096c00e8fde05ef04b9472fc9d7d62b6e033cecd6f7991",
        "nvim-linux-arm64.tar.gz": "99bb3c53604e83ce18fc0b459e34cf1a5e212f4e5fbe2eb136b3c18092ae9905",
        "nvim-macos-arm64.tar.gz": "9c84686ce84bbab725ff3d88ca3ac1d6da5a72128b68d9dd8094e34d4049a126",
        "nvim-macos-x86_64.tar.gz": "014649c0c75e188fd70d40f787ee7cb83e7c2926264cd4e72c300d0198922371",
        "nvim-win64.zip": "7b10e6e08b14bf256422b72cfe706ef0452fd9c0a40e138426e1af09803d697f",
    },
    "0.12.6": {
        "nvim-linux-x86_64.tar.gz": "474430d53e6264f6d6dd18db42d6dc9df3a1b56ca9e88a325bbf860e1a811d87",
        "nvim-linux-arm64.tar.gz": "8f1f64a0bdb97247034038c3823c6cbad5bdf9ecd5751b85494b71c3ee04c815",
        "nvim-macos-arm64.tar.gz": "1dbd148222b051ba6c307c4d17a318ed083df018a4f6a5a0ce46955ffc9ab87b",
        "nvim-macos-x86_64.tar.gz": "481424f1dbc85f637d57e47d27d042aa9c883e5a4fe9ee64fc4ee9573b221998",
        "nvim-win64.zip": "4e12b7103b601ffc8f25954b4da100f339faa73dccb69a5b135e897fa3d893ed",
    },
}
USER_AGENT = "crow-term-ci-fetch/1"


def asset_name() -> str:
    machine = platform.machine().lower()
    if platform.system() == "Windows" and machine in ("amd64", "x86_64"):
        return "nvim-win64.zip"
    system = {"Linux": "linux", "Darwin": "macos"}.get(platform.system())
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "arm64", "aarch64": "arm64"}.get(machine)
    if not system or not arch:
        sys.exit(f"unsupported platform for pinned Neovim: {platform.system()} {machine}")
    return f"nvim-{system}-{arch}.tar.gz"


def safe_extract(archive: Path, destination: Path) -> None:
    if archive.name.endswith(".zip"):
        root = destination.resolve()
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                target = (destination / member.filename).resolve()
                # Zip entries carry no symlinks for extractall; only paths are checked.
                if not target.is_relative_to(root):
                    sys.exit(f"refusing archive member outside destination: {member.filename}")
            bundle.extractall(destination)
        return
    if not hasattr(tarfile, "data_filter"):
        sys.exit("Python with tarfile extraction filters (3.12+, or a 3.8-3.11 security release) is required")
    root = destination.resolve()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            target = (destination / member.name).resolve()
            link = (target.parent / member.linkname).resolve() if member.issym() else root
            if not target.is_relative_to(root) or not link.is_relative_to(root):
                sys.exit(f"refusing archive member outside destination: {member.name}")
            if not (member.isfile() or member.isdir() or member.issym()):
                sys.exit(f"refusing special archive member: {member.name}")
        tar.extractall(destination, filter="data")


def installed(executable: Path, version: str, digest: str) -> bool:
    """A cached tree counts only if it records this archive digest and reports the version."""
    marker = executable.parents[2] / "archive.sha256"
    if not executable.is_file() or not marker.is_file() or marker.read_text().strip() != digest:
        return False
    result = subprocess.run([str(executable), "--version"], capture_output=True, text=True, timeout=30)
    return result.returncode == 0 and result.stdout.splitlines()[:1] == [f"NVIM v{version}"]


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("version", choices=sorted(PINS))
    parser.add_argument("--dest", required=True, type=Path, help="directory that receives the unpacked release")
    args = parser.parse_args()
    name = asset_name()
    expected = PINS[args.version].get(name)
    if not expected:
        sys.exit(f"no pinned digest for {name} {args.version}")
    destination = args.dest.resolve() / f"nvim-{args.version}"
    stem = name.removesuffix(".tar.gz").removesuffix(".zip")
    executable = destination / stem / "bin" / ("nvim.exe" if name.endswith(".zip") else "nvim")
    if installed(executable, args.version, expected):
        print(executable)
        return
    url = f"https://github.com/neovim/neovim/releases/download/v{args.version}/{name}"
    args.dest.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=args.dest) as temp:
        archive = Path(temp) / name
        request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
        digest = hashlib.sha256()
        with urllib.request.urlopen(request, timeout=120) as response, archive.open("wb") as out:
            while chunk := response.read(1 << 20):
                digest.update(chunk)
                out.write(chunk)
        if digest.hexdigest() != expected:
            sys.exit(f"SHA-256 mismatch for {name}: {digest.hexdigest()} != {expected}")
        staging = Path(temp) / "unpacked"
        staging.mkdir()
        safe_extract(archive, staging)
        (staging / "archive.sha256").write_text(expected + "\n")
        if destination.exists():
            shutil.rmtree(destination)
        os.replace(staging, destination)
    if not installed(executable, args.version, expected):
        sys.exit(f"archive did not contain a working Neovim {args.version} at {executable}")
    print(executable)


if __name__ == "__main__":
    main()
