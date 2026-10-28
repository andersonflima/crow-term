#!/usr/bin/env python3
"""Install the pinned Symbols Nerd Font for the GUI journeys (CI only).

The archive digest is checked before it is opened; only the single Mono TTF is
extracted, into the per-user font directory the renderer's font database scans.
Colour emoji and CJK come from the OS (Linux: fonts-noto-color-emoji and
fonts-noto-cjk from the runner's apt archive; Windows/macOS: system fonts).
"""
import hashlib
import os
from pathlib import Path
import platform
import sys
import tarfile
import tempfile
import urllib.request

# https://github.com/ryanoasis/nerd-fonts/releases/tag/v3.4.0 (MIT / OFL, see LICENSE in the archive)
URL = "https://github.com/ryanoasis/nerd-fonts/releases/download/v3.4.0/NerdFontsSymbolsOnly.tar.xz"
SHA256 = "7f8c090da3b0eaa7108646bf34cbbb6ed13d5358a72460522108b06c7ecd716a"
MEMBER = "SymbolsNerdFontMono-Regular.ttf"


def font_dir():
    system = platform.system()
    if system == "Darwin":
        return Path.home() / "Library/Fonts"
    if system == "Windows":
        return Path(os.environ["LOCALAPPDATA"]) / "Microsoft/Windows/Fonts"
    return Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "fonts"


def main():
    with tempfile.TemporaryDirectory() as temp:
        archive = Path(temp) / "symbols.tar.xz"
        request = urllib.request.Request(URL, headers={"User-Agent": "crow-term-ci-fonts/1"})
        with urllib.request.urlopen(request, timeout=120) as response:
            archive.write_bytes(response.read())
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        if digest != SHA256:
            sys.exit(f"Nerd Font archive digest {digest} != pinned {SHA256}")
        target = font_dir()
        target.mkdir(parents=True, exist_ok=True)
        with tarfile.open(archive, "r:xz") as tar:
            member = tar.getmember(MEMBER)
            if not member.isfile():
                sys.exit(f"{MEMBER} is not a regular file")
            data = tar.extractfile(member).read()
        (target / MEMBER).write_bytes(data)
        print(target / MEMBER)


if __name__ == "__main__":
    main()
