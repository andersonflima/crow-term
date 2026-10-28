#!/usr/bin/env python3
"""Install the pinned LazyVim plugin set for tests/lazyvim_e2e.py (network step).

Plugins go to `<home>/data/nvim/lazy` at exactly the commits recorded in
tests/lazyvim/nvim/lazy-lock.json. `--generate-lock` refreshes the lockfile
instead (maintainers only; review the diff before committing it).
"""
import argparse
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]
FIXTURE = ROOT / "tests" / "lazyvim" / "nvim"
LOCK = FIXTURE / "lazy-lock.json"
LAZY_REPO = "https://github.com/folke/lazy.nvim.git"


def environment(home: Path, config: Path) -> dict:
    env = dict(os.environ)
    env.update(
        XDG_CONFIG_HOME=str(config),
        XDG_DATA_HOME=str(home / "data"),
        XDG_STATE_HOME=str(home / "state"),
        XDG_CACHE_HOME=str(home / "cache"),
        NVIM_APPNAME="nvim",
        CROW_LAZYVIM_BOOTSTRAP="1",
        GIT_TERMINAL_PROMPT="0",
    )
    return env


def git(*args, cwd=None):
    subprocess.run(["git", *args], cwd=cwd, check=True, stdout=subprocess.DEVNULL)


def head(path: Path) -> str:
    return subprocess.run(["git", "rev-parse", "HEAD"], cwd=path, check=True,
                          capture_output=True, text=True).stdout.strip()


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--home", type=Path, required=True, help="plugin cache directory")
    parser.add_argument("--nvim", default=os.environ.get("CROW_TERM_NVIM") or "nvim")
    parser.add_argument("--generate-lock", action="store_true")
    args = parser.parse_args()
    home = args.home.resolve()
    lazy_dir = home / "data" / "nvim" / "lazy"
    lock = {} if args.generate_lock else json.loads(LOCK.read_text())
    lazy_path = lazy_dir / "lazy.nvim"
    if not lazy_path.is_dir():
        lazy_dir.mkdir(parents=True, exist_ok=True)
        git("clone", "--filter=blob:none", LAZY_REPO, str(lazy_path))
    if lock:
        git("fetch", "--quiet", "origin", cwd=lazy_path) if head(lazy_path) != lock["lazy.nvim"]["commit"] else None
        git("checkout", "--quiet", lock["lazy.nvim"]["commit"], cwd=lazy_path)
    with tempfile.TemporaryDirectory(prefix="crow-lazyvim-config-") as temp:
        config = Path(temp)
        shutil.copytree(FIXTURE, config / "nvim")
        if args.generate_lock:
            (config / "nvim" / "lazy-lock.json").unlink(missing_ok=True)
        command = "Lazy! sync" if args.generate_lock else "Lazy! restore"
        result = subprocess.run([args.nvim, "--headless", f"+{command}", "+qa"],
                                env=environment(home, config), capture_output=True, text=True, timeout=1800)
        sys.stderr.write(result.stderr[-4000:])
        if result.returncode:
            sys.exit(f"nvim {command} failed with {result.returncode}")
        if args.generate_lock:
            shutil.copyfile(config / "nvim" / "lazy-lock.json", LOCK)
            lock = json.loads(LOCK.read_text())
    wrong = {name: entry["commit"] for name, entry in lock.items()
             if not (lazy_dir / name).is_dir() or head(lazy_dir / name) != entry["commit"]}
    if wrong:
        sys.exit(f"plugins not at their locked commits: {sorted(wrong)}")
    print(f"{len(lock)} plugins at locked commits in {lazy_dir}")


if __name__ == "__main__":
    main()
