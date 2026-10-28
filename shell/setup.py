#!/usr/bin/env python3
"""Create an opt-in Crow Term shell profile; never edit a personal dotfile.

Only Python's standard library is required. Plugin installation is offline and
copies an allowlist of vendored, pinned, SHA256-verified upstream files.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import stat
import sys
import tempfile

HERE = Path(__file__).resolve().parent


def quote_fish(value):
    return "'" + str(value).replace("\\", "\\\\").replace("'", "\\'") + "'"


def quote_zsh(value):
    return "'" + str(value).replace("'", "'\\''") + "'"


def checked_prefix(value):
    path = Path(os.path.abspath(os.path.expanduser(value)))
    if any(ord(c) < 32 or ord(c) == 127 for c in str(path)):
        raise ValueError("prefix must not contain control characters")
    if path.is_symlink():
        raise ValueError("prefix must not be a symlink")
    path = path.resolve()
    if path in (Path(path.anchor), Path.home().resolve()):
        raise ValueError("choose a dedicated profile directory, not root or HOME")
    if path.exists() and not path.is_dir():
        raise ValueError("prefix is not a directory")
    return path


def fish_profile(prefix):
    q = quote_fish
    return f"""# Crow Term isolated profile. Generated; no personal dotfiles are sourced.
set -g fisher_path {q(prefix / 'fish-managed')}
set -g fish_function_path {q(prefix / 'fish-managed/functions')} {q(prefix / 'plugins/fisher/functions')} {q(prefix / 'plugins/z/functions')} $fish_function_path
set -g fish_complete_path {q(prefix / 'plugins/fisher/completions')} $fish_complete_path
set -gx Z_DATA_DIR {q(prefix / 'data/z')}
set -gx Z_DATA {q(prefix / 'data/z/data')}
if test -f {q(prefix / 'plugins/z/conf.d/z.fish')}
    source {q(prefix / 'plugins/z/conf.d/z.fish')}
end
set -g fish_color_command FFFFFF --bold
set -g fish_color_keyword FFFFFF --bold
set -g fish_color_quote 8C8C8C
set -g fish_color_comment 6A6A6A --italics
set -g fish_color_param A8A8A8
set -g fish_color_operator FFFFFF
set -g fish_color_redirection C6C6C6
set -g fish_color_end FFFFFF
set -g fish_color_error FFFFFF --bold
set -g fish_color_autosuggestion 4A4A4A
set -g fish_color_selection --background=2A2A2A
set -g fish_color_search_match --background=2A2A2A
function fish_prompt
    set -l last_status $status
    set_color white
    printf '%s' (prompt_pwd)
    if test $last_status -ne 0
        printf ' [%s]' $last_status
    end
    printf ' > '
    set_color normal
end
"""


def zsh_profile(prefix):
    q = quote_zsh
    return f"""# Crow Term isolated profile; no personal dotfiles or framework.
HISTFILE={q(prefix / 'data/zsh-history')}
HISTSIZE=10000
SAVEHIST=10000
setopt HIST_IGNORE_SPACE
PROMPT='%F{{white}}%~%f > '
ZSH_AUTOSUGGEST_HIGHLIGHT_STYLE='fg=#4A4A4A'
if [[ -f {q(prefix / 'plugins/zsh-autosuggestions/zsh-autosuggestions.zsh')} ]]; then
    source {q(prefix / 'plugins/zsh-autosuggestions/zsh-autosuggestions.zsh')}
fi
typeset -gA ZSH_HIGHLIGHT_STYLES
ZSH_HIGHLIGHT_STYLES[command]='fg=#FFFFFF,bold'
ZSH_HIGHLIGHT_STYLES[builtin]='fg=#FFFFFF,bold'
ZSH_HIGHLIGHT_STYLES[reserved-word]='fg=#FFFFFF,bold'
ZSH_HIGHLIGHT_STYLES[comment]='fg=#6A6A6A,italic'
ZSH_HIGHLIGHT_STYLES[single-quoted-argument]='fg=#8C8C8C'
ZSH_HIGHLIGHT_STYLES[double-quoted-argument]='fg=#8C8C8C'
# Must remain the final sourced plugin (upstream requirement).
if [[ -f {q(prefix / 'plugins/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh')} ]]; then
    source {q(prefix / 'plugins/zsh-syntax-highlighting/zsh-syntax-highlighting.zsh')}
fi
"""


def launcher(prefix, shell):
    # Embed data as Python literals, not shell substitutions. execvpe receives an
    # argument array, and profile path quoting is only for fish's -C expression.
    args = (["--no-config", "--init-command", "source " + quote_fish(prefix / "fish/config.fish")]
            if shell == "fish" else ["-d", "-i"])
    return f"""#!/usr/bin/env python3
import os
from pathlib import Path
import shutil
import sys
prefix = Path({str(prefix)!r})
env = os.environ.copy()
# Isolate shell universal variables, history, plugin state and completion cache.
# Child applications inherit these XDG directories too; this is opt-in.
for key, rel in {{'XDG_CONFIG_HOME': 'config', 'XDG_DATA_HOME': 'data', 'XDG_CACHE_HOME': 'cache', 'XDG_STATE_HOME': 'state'}}.items():
    folder = prefix / rel
    folder.mkdir(mode=0o700, parents=True, exist_ok=True)
    env[key] = str(folder)
env['ZDOTDIR'] = str(prefix / 'zsh')
program = shutil.which({shell!r})
if not program:
    sys.exit('Required shell not found on PATH: {shell}')
os.execvpe(program, [program] + {args!r} + sys.argv[1:], env)
"""


def plan_files(prefix, shell, install_plugins=False):
    profile = ("fish/config.fish", fish_profile(prefix)) if shell == "fish" else ("zsh/.zshrc", zsh_profile(prefix))
    plan = {profile[0]: (profile[1].encode(), 0o600),
            f"launch-{shell}.py": (launcher(prefix, shell).encode(), 0o700)}
    lock_data = (HERE / "plugins.lock.json").read_bytes()
    lock = json.loads(lock_data)
    if lock.get("schema") != 1:
        raise ValueError("unsupported plugin lock schema")
    if install_plugins:
        selected = {"fisher", "z"} if shell == "fish" else {"zsh-autosuggestions", "zsh-syntax-highlighting"}
        for plugin in lock["plugins"]:
            if plugin["name"] not in selected:
                continue
            for rel, digest in plugin["files"].items():
                path = PurePosixPath(rel)
                if path.is_absolute() or ".." in path.parts:
                    raise ValueError("invalid path in plugin lock")
                source = HERE / "vendor" / plugin["name"] / rel
                if source.is_symlink() or not source.is_file():
                    raise ValueError(f"missing/nonregular vendored file: {source}")
                content = source.read_bytes()
                if hashlib.sha256(content).hexdigest() != digest:
                    raise ValueError(f"SHA256 mismatch: {source}")
                plan[f"plugins/{plugin['name']}/{rel}"] = (content, 0o600)
        plan["plugins.lock.json"] = (lock_data, 0o600)
        if shell == "fish":
            pins = [p['repository'].removeprefix('https://github.com/') + '@' + p['commit']
                    for p in lock['plugins'] if p['name'] in selected]
            plan["fish-managed/fish_plugins"] = (("\n".join(pins) + "\n").encode(), 0o600)
    return plan


def check_destination(prefix, rel, data):
    dest = prefix / rel
    for current in [prefix] + list(dest.relative_to(prefix).parents)[::-1]:
        # Relative parents are handled below; only inspect paths beneath prefix.
        if not current.is_absolute():
            current = prefix / current
        if current.is_symlink():
            raise ValueError(f"refusing symlink directory: {current}")
        if current.exists() and not current.is_dir():
            raise ValueError(f"not a directory: {current}")
    if dest.is_symlink():
        raise ValueError(f"refusing symlink file: {dest}")
    if dest.exists():
        if not stat.S_ISREG(dest.stat().st_mode) or dest.read_bytes() != data:
            raise ValueError(f"existing file differs; refusing overwrite: {dest}")
        return False
    return True


def create_directory(path):
    missing = []
    while not path.exists():
        missing.append(path)
        path = path.parent
    if path.is_symlink() or not path.is_dir():
        raise ValueError(f"unsafe directory: {path}")
    for directory in reversed(missing):
        directory.mkdir(mode=0o700)


def write_new(prefix, rel, data, mode):
    if not check_destination(prefix, rel, data):
        return
    dest = prefix / rel
    create_directory(dest.parent)
    temporary = None
    try:
        with tempfile.NamedTemporaryFile(dir=dest.parent, prefix=".crow-term-", delete=False) as file:
            temporary = Path(file.name)
            os.fchmod(file.fileno(), mode)
            file.write(data)
            file.flush()
            os.fsync(file.fileno())
        # Atomic no-clobber publish. Unlike replace()/rename(), link() cannot
        # overwrite an existing destination, even if another setup races us.
        try:
            os.link(temporary, dest)
        except FileExistsError:
            if check_destination(prefix, rel, data):
                raise
    finally:
        if temporary is not None:
            temporary.unlink(missing_ok=True)


def main(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--shell", required=True, choices=("fish", "zsh"))
    parser.add_argument("--prefix", required=True, help="dedicated Crow Term profile directory")
    parser.add_argument("--install-plugins", action="store_true", help="copy verified vendored plugins (offline)")
    parser.add_argument("--dry-run", action="store_true", help="validate and print plan without writing")
    args = parser.parse_args(argv)
    try:
        prefix = checked_prefix(args.prefix)
        plan = plan_files(prefix, args.shell, args.install_plugins)
        # Preflight every target before the first write. Interrupted execution
        # may leave a partial set of complete files; rerun safely resumes it.
        changes = {rel: check_destination(prefix, rel, data) for rel, (data, _) in plan.items()}
        for rel, (data, mode) in plan.items():
            print(("create " if changes[rel] else "keep   ") + str(prefix / rel))
            if not args.dry_run:
                write_new(prefix, rel, data, mode)
        print(("Would launch: " if args.dry_run else "Launch explicitly: ") + f"python3 {str(prefix / ('launch-' + args.shell + '.py'))!r}")
        return 0
    except (OSError, ValueError, KeyError, TypeError) as error:
        print(f"shell setup: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
