#!/usr/bin/env python3
"""Build the npm packages of Crow Term from the release archives.

    python scripts/npm_package.py --dist dist --out npm-dist [--version X.Y.Z]

Reads `crow-term-<version>-<target>.tar.gz|zip` (the signed/notarized output of
release.yml) and writes one directory per npm package:

    npm-dist/crow-term/                 @andespindola/crow-term (JS launcher)
    npm-dist/crow-term-<os>-<cpu>/      @andespindola/crow-term-<os>-<cpu> (binary)

The main package lists every platform package as an exact-version
optionalDependency; npm installs only the one whose `os`/`cpu` match, and the
launcher (npm/crow-term/bin/crow-term.js) runs its binary. No install scripts,
no downloads at install or run time. The version comes from Cargo.toml.
"""
import argparse
import json
from pathlib import Path
import shutil
import sys
import tarfile
import tempfile
import zipfile

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "scripts"))
import package  # noqa: E402

SCOPE = "@andespindola"
# release target -> (npm os, npm cpu, archive extension)
TARGETS = {
    "macos-arm64": ("darwin", "arm64", "tar.gz"),
    "macos-x86_64": ("darwin", "x64", "tar.gz"),
    "linux-x86_64": ("linux", "x64", "tar.gz"),
    "linux-arm64": ("linux", "arm64", "tar.gz"),
    "windows-x86_64": ("win32", "x64", "zip"),
}
NOTICES = ["LICENSE", "NOTICE.md", "THIRD_PARTY_LICENSES.txt"]
APP = "Crow Term.app"
COMMON = {
    "license": "MIT",
    "homepage": "https://github.com/andersonflima/crow-term",
    "repository": {"type": "git", "url": "git+https://github.com/andersonflima/crow-term.git"},
    "bugs": {"url": "https://github.com/andersonflima/crow-term/issues"},
}


def platform_name(npm_os, cpu):
    return f"{SCOPE}/crow-term-{npm_os}-{cpu}"


def safe_extract(archive, destination):
    root = destination.resolve()
    if archive.name.endswith(".zip"):
        with zipfile.ZipFile(archive) as bundle:
            for member in bundle.infolist():
                if not (destination / member.filename).resolve().is_relative_to(root):
                    sys.exit(f"refusing archive member outside destination: {member.filename}")
            bundle.extractall(destination)
        return
    with tarfile.open(archive, "r:gz") as tar:
        tar.extractall(destination, filter="data")


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def build_platform(target, archive, version, out):
    npm_os, cpu, _ = TARGETS[target]
    directory = out / f"crow-term-{npm_os}-{cpu}"
    shutil.rmtree(directory, ignore_errors=True)
    (directory / "bin").mkdir(parents=True)
    with tempfile.TemporaryDirectory() as temp:
        safe_extract(archive, Path(temp))
        tree = Path(temp) / f"crow-term-{version}-{target}"
        if not tree.is_dir():
            sys.exit(f"{archive.name} does not contain {tree.name}/")
        if npm_os == "darwin":
            # The signed, notarized bundle as a whole; the binary inside finds
            # Contents/Resources/shell itself. The archive's symlink is not shipped.
            shutil.copytree(tree / APP, directory / "bin" / APP, symlinks=True)
            binary = directory / "bin" / APP / "Contents/MacOS/crow-term"
        else:
            name = "crow-term.exe" if npm_os == "win32" else "crow-term"
            shutil.copy2(tree / name, directory / "bin" / name)
            # shell/setup.py beside the binary (bundled shell-setup lookup).
            shutil.copytree(tree / "shell", directory / "bin" / "shell")
            binary = directory / "bin" / name
        if not binary.is_file():
            sys.exit(f"{archive.name}: binary missing at {binary}")
        if npm_os != "win32":
            binary.chmod(0o755)
        for notice in NOTICES:
            shutil.copy2(tree / notice, directory / notice)
    write_json(directory / "package.json", {
        "name": platform_name(npm_os, cpu),
        "version": version,
        "description": f"Crow Term native binary for {npm_os}-{cpu} (installed by @andespindola/crow-term)",
        **COMMON,
        "os": [npm_os],
        "cpu": [cpu],
        "files": ["bin", *NOTICES],
        "preferUnplugged": True,
    })
    (directory / "README.md").write_text(
        f"# {platform_name(npm_os, cpu)}\n\nThe `{npm_os}-{cpu}` binary of Crow Term. Install "
        f"[`@andespindola/crow-term`](https://www.npmjs.com/package/@andespindola/crow-term) instead.\n",
        encoding="utf-8")
    return directory


def build_main(version, out):
    directory = out / "crow-term"
    shutil.rmtree(directory, ignore_errors=True)
    shutil.copytree(ROOT / "npm" / "crow-term", directory)
    for notice in NOTICES:
        shutil.copy2(ROOT / notice, directory / notice)
    write_json(directory / "package.json", {
        "name": f"{SCOPE}/crow-term",
        "version": version,
        "description": "Crow Term: a native terminal for command-line tools and Vim/Neovim/LazyVim, with persistent sessions",
        **COMMON,
        "keywords": ["terminal", "terminal-emulator", "neovim", "tmux", "cli"],
        "bin": {"crow-term": "bin/crow-term.js"},
        "files": ["bin", "README.md", *NOTICES],
        "engines": {"node": ">=18"},
        "optionalDependencies": {
            platform_name(npm_os, cpu): version for npm_os, cpu, _ in TARGETS.values()
        },
    })
    return directory


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dist", type=Path, required=True, help="directory with the release archives")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--version", default=None, help="default: Cargo.toml version")
    parser.add_argument("--allow-missing", action="store_true", help="skip absent targets (tests only)")
    args = parser.parse_args(argv)
    version = args.version or package.cargo_version()
    args.out.mkdir(parents=True, exist_ok=True)
    built = []
    for target, (_, _, extension) in TARGETS.items():
        archive = args.dist / f"crow-term-{version}-{target}.{extension}"
        if not archive.is_file():
            if args.allow_missing:
                continue
            sys.exit(f"missing release archive {archive}")
        built.append(build_platform(target, archive, version, args.out))
    built.append(build_main(version, args.out))
    for directory in built:
        print(directory)


if __name__ == "__main__":
    main()
