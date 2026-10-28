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
import sys
import tarfile
import tempfile
import urllib.request

# Upstream: https://github.com/neovim/neovim/releases (asset digests as published).
PINS = {
    "0.11.7": {
        "nvim-linux-x86_64.tar.gz": "38a7c6317f94503841096c00e8fde05ef04b9472fc9d7d62b6e033cecd6f7991",
        "nvim-linux-arm64.tar.gz": "99bb3c53604e83ce18fc0b459e34cf1a5e212f4e5fbe2eb136b3c18092ae9905",
        "nvim-macos-arm64.tar.gz": "9c84686ce84bbab725ff3d88ca3ac1d6da5a72128b68d9dd8094e34d4049a126",
        "nvim-macos-x86_64.tar.gz": "014649c0c75e188fd70d40f787ee7cb83e7c2926264cd4e72c300d0198922371",
    },
    "0.12.6": {
        "nvim-linux-x86_64.tar.gz": "474430d53e6264f6d6dd18db42d6dc9df3a1b56ca9e88a325bbf860e1a811d87",
        "nvim-linux-arm64.tar.gz": "8f1f64a0bdb97247034038c3823c6cbad5bdf9ecd5751b85494b71c3ee04c815",
        "nvim-macos-arm64.tar.gz": "1dbd148222b051ba6c307c4d17a318ed083df018a4f6a5a0ce46955ffc9ab87b",
        "nvim-macos-x86_64.tar.gz": "481424f1dbc85f637d57e47d27d042aa9c883e5a4fe9ee64fc4ee9573b221998",
    },
}
USER_AGENT = "crow-term-ci-fetch/1"


def asset_name() -> str:
    system = {"Linux": "linux", "Darwin": "macos"}.get(platform.system())
    machine = platform.machine().lower()
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "arm64": "arm64", "aarch64": "arm64"}.get(machine)
    if not system or not arch:
        sys.exit(f"unsupported platform for pinned Neovim: {platform.system()} {machine}")
    return f"nvim-{system}-{arch}.tar.gz"


def safe_extract(archive: Path, destination: Path) -> None:
    root = destination.resolve()
    with tarfile.open(archive, "r:gz") as tar:
        for member in tar.getmembers():
            target = (destination / member.name).resolve()
            if not target.is_relative_to(root) or member.issym() and Path(member.linkname).is_absolute():
                sys.exit(f"refusing archive member outside destination: {member.name}")
            if not (member.isfile() or member.isdir() or member.issym()):
                sys.exit(f"refusing special archive member: {member.name}")
        tar.extractall(destination, filter="data") if hasattr(tarfile, "data_filter") else tar.extractall(destination)


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
    executable = destination / name.removesuffix(".tar.gz") / "bin" / "nvim"
    if executable.is_file():
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
        if destination.exists():
            shutil.rmtree(destination)
        os.replace(staging, destination)
    if not executable.is_file():
        sys.exit(f"archive did not contain {executable}")
    print(executable)


if __name__ == "__main__":
    main()
