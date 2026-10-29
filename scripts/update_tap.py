#!/usr/bin/env python3
"""Render the crow-term Homebrew formula and cask into a local tap checkout.

Stdlib only, Python 3.9+. Never commits or pushes: it prints the git commands.

    python3 scripts/update_tap.py --version 0.1.0-alpha.1 \\
        --tap "$(brew --repository andersonflima/tap)" \\
        --sha256sums https://github.com/andersonflima/crow-term/releases/download/v0.1.0-alpha.1/SHA256SUMS \\
        [--notarized] [--dry-run] [--cask-only]

``--assets-dir DIR`` computes digests from local release archives instead.
Without ``--notarized`` the cask gets a caveat about the unsigned/unnotarized app.
``--cask-only`` requires only macOS app archives and leaves the formula untouched.
"""
import argparse
import difflib
import hashlib
from pathlib import Path
import re
import shlex
import sys
import urllib.parse
import urllib.request

ROOT = Path(__file__).resolve().parents[1]
TEMPLATES = ROOT / "packaging" / "homebrew"
# Keep in sync with the tag check in .github/workflows/release.yml.
VERSION_RE = re.compile(r"[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?")
SHA_RE = re.compile(r"[0-9a-f]{64}")
# Placeholder token -> asset suffix after "crow-term-<version>-".
ASSETS = {
    "SHA256_MACOS_ARM64": "macos-arm64.tar.gz",
    "SHA256_MACOS_X86_64": "macos-x86_64.tar.gz",
    "SHA256_LINUX_ARM64": "linux-arm64.tar.gz",
    "SHA256_LINUX_X86_64": "linux-x86_64.tar.gz",
    "SHA256_MACOS_ARM64_APP": "macos-arm64.app.zip",
    "SHA256_MACOS_X86_64_APP": "macos-x86_64.app.zip",
}
OUTPUTS = {"crow-term.rb": "Formula/crow-term.rb", "cask/crow-term.rb": "Casks/crow-term.rb"}
HEADER = "# Generated from crow-term packaging/homebrew by scripts/update_tap.py; do not edit by hand.\n"
UNSIGNED_CAVEATS = '''
  caveats <<~EOS
    This build of Crow Term is not notarized by Apple (it is only ad-hoc
    signed). If macOS refuses to open it, remove the download quarantine:
      xattr -dr com.apple.quarantine "#{appdir}/Crow Term.app"
  EOS
'''


class TapError(Exception):
    pass


def asset_names(version):
    return {token: f"crow-term-{version}-{suffix}" for token, suffix in ASSETS.items()}


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open("rb") as file:
        for chunk in iter(lambda: file.read(1 << 20), b""):
            digest.update(chunk)
    return digest.hexdigest()


def parse_sums(text):
    sums = {}
    for number, line in enumerate(text.splitlines(), 1):
        if not line.strip():
            continue
        parts = line.split()
        if len(parts) != 2 or not SHA_RE.fullmatch(parts[0].lower()):
            raise TapError(f"SHA256SUMS line {number} is malformed")
        sums[parts[1].lstrip("*")] = parts[0].lower()
    return sums


def read_sums(source):
    if re.match(r"https?://", source):
        if urllib.parse.urlparse(source).scheme != "https":
            raise TapError("Refusing a non-HTTPS SHA256SUMS URL")
        with urllib.request.urlopen(source, timeout=60) as response:  # noqa: S310 (https only)
            return parse_sums(response.read().decode("utf-8"))
    return parse_sums(Path(source).read_text(encoding="utf-8"))


def digests_from_dir(directory, names):
    result, missing = {}, []
    for token, name in names.items():
        path = directory / name
        if not path.is_file():
            missing.append(name)
            continue
        digest = sha256_file(path)
        sidecar = directory / f"{name}.sha256"
        if sidecar.is_file() and sidecar.read_text(encoding="utf-8").split()[0].lower() != digest:
            raise TapError(f"{sidecar.name} does not match {name}")
        result[token] = digest
    if missing:
        raise TapError("Missing release assets: " + ", ".join(missing))
    return result


def digests_from_sums(sums, names):
    missing = [name for name in names.values() if name not in sums]
    if missing:
        raise TapError("SHA256SUMS lacks: " + ", ".join(missing))
    return {token: sums[name] for token, name in names.items()}


def render(template, version, digests, notarized):
    body = template.splitlines(keepends=True)
    while body and body[0].startswith("#"):
        body.pop(0)  # template-only header comments
    text = HEADER + "".join(body)
    caveats = "" if notarized else UNSIGNED_CAVEATS
    text = re.sub(r"\n[ \t]*@CASK_CAVEATS@[ \t]*\n", lambda _: "\n" + caveats if caveats else "\n", text)
    text = text.replace("@VERSION@", version)
    for token, digest in digests.items():
        text = text.replace(f"@{token}@", digest)
    leftover = sorted(set(re.findall(r"@[A-Z0-9_]+@", text)))
    if leftover:
        raise TapError("Unrendered placeholders: " + ", ".join(leftover))
    return text


def run(argv=None, out=sys.stdout):
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--version", required=True, help="release version without the leading v")
    parser.add_argument("--tap", required=True, type=Path, help="local tap checkout (e.g. brew --repository andersonflima/tap)")
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--assets-dir", type=Path, help="directory with the release archives")
    source.add_argument("--sha256sums", help="SHA256SUMS file path or https URL")
    parser.add_argument("--notarized", action="store_true", help="macOS app is signed and notarized (drops the cask caveat)")
    parser.add_argument("--dry-run", action="store_true", help="print a diff, write nothing")
    parser.add_argument("--cask-only", action="store_true", help="update only the cask; preserve the existing formula")
    parser.add_argument("--templates", type=Path, default=TEMPLATES, help=argparse.SUPPRESS)
    args = parser.parse_args(argv)

    version = args.version[1:] if args.version.startswith("v") else args.version
    if not VERSION_RE.fullmatch(version):
        raise TapError(f"Invalid version {args.version!r}; expected semver like 0.1.0 or 0.1.0-alpha.1")
    tap = args.tap.expanduser()
    if not tap.is_dir():
        raise TapError(f"Tap directory not found: {tap}")
    names = asset_names(version)
    outputs = OUTPUTS
    if args.cask_only:
        names = {token: name for token, name in names.items() if token.endswith("_APP")}
        outputs = {"cask/crow-term.rb": OUTPUTS["cask/crow-term.rb"]}
    if args.assets_dir:
        digests = digests_from_dir(args.assets_dir, names)
    else:
        digests = digests_from_sums(read_sums(args.sha256sums), names)

    changed = []
    for template_name, relative in outputs.items():
        template = (args.templates / template_name).read_text(encoding="utf-8")
        rendered = render(template, version, digests, args.notarized)
        target = tap / relative
        current = target.read_text(encoding="utf-8") if target.is_file() else ""
        if current == rendered:
            print(f"unchanged {relative}", file=out)
            continue
        changed.append(relative)
        if args.dry_run:
            out.writelines(difflib.unified_diff(current.splitlines(keepends=True), rendered.splitlines(keepends=True),
                                                f"a/{relative}", f"b/{relative}"))
        else:
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text(rendered, encoding="utf-8")
            print(f"wrote {relative}", file=out)

    if changed and not args.dry_run:
        quoted = shlex.quote(str(tap))
        files = " ".join(changed)
        print("\nReview, audit and publish the tap yourself:", file=out)
        ruby_files = " ".join(f"{quoted}/{relative}" for relative in outputs.values())
        print(f"  ruby -c {ruby_files}", file=out)
        print("  brew style andersonflima/tap", file=out)
        print(f"  git -C {quoted} add {files}", file=out)
        print(f"  git -C {quoted} commit -m 'feat(crow-term): {version}'", file=out)
        print(f"  git -C {quoted} push", file=out)
    return changed


def main():
    try:
        run()
    except (TapError, OSError) as error:
        sys.exit(f"update_tap: {error}")


if __name__ == "__main__":
    main()
