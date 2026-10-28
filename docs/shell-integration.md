# Fish and zsh: isolated, explicit integration

Crow Term does not modify a user's login shell or startup files. The following
setup is optional; an ordinary configured shell remains supported. Python 3.9+
and the selected shell must already be installed. Setup does not install them.

## Create a profile

From the source checkout, inspect a plan first (spaces in paths are supported):

```sh
python3 shell/setup.py --shell fish --prefix "$HOME/.config/crow-term/profiles/fish-v1" --install-plugins --dry-run
python3 shell/setup.py --shell fish --prefix "$HOME/.config/crow-term/profiles/fish-v1" --install-plugins
python3 "$HOME/.config/crow-term/profiles/fish-v1/launch-fish.py"
```

For zsh, substitute `--shell zsh`, a separate `zsh-v1` prefix, and
`launch-zsh.py`. Without `--install-plugins`, only the profile/launcher are
created; absent plugins are skipped. Adding the flag later fills in the pinned
plugins without rewriting an existing identical profile.

All plugin source and licenses are bundled from immutable official upstream
commits; [the source manifest](../shell/plugins.lock.json) contains SHA256 hashes.
Installation is offline, never executes downloaded code and never calls Fisher.
The plugin code first runs when the user explicitly launches the generated
profile. Executing `fisher install`/`update` yourself is a separate network-capable
operation, outside the setup's offline guarantee. Its `fisher_path` is confined
to the profile's `fish-managed/` directory, not the personal fish config.

## Launch contract

The generated launcher calls the shell with an argument array, without an
intermediate command shell. Absolute profile paths are encoded safely, including
spaces, quotes, dollar signs and semicolons.

- **Fish:** `fish --no-config --init-command 'source <quoted-profile>/fish/config.fish'`.
  Fish's native editing/autosuggestions/completions remain available. The minimal
  optional plugin set is Fisher + `jethrokuan/z`; there is one small built-in
  prompt and Crowquill-inspired colors, not Tide + Shellder + Starship together.
- **zsh:** environment `ZDOTDIR=<prefix>/zsh`, then `zsh -d -i`. The dedicated
  `.zshrc` loads autosuggestions, then syntax-highlighting **last**, without a
  framework. `-d` skips global rc files, but zsh's system `/etc/zshenv` is still
  unavoidable. No personal `.zshrc`, `.zprofile` or `.zshenv` is sourced.
- Both launchers set `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` and
  `XDG_STATE_HOME` beneath the prefix in the child environment. This isolates
  fish universal variables, plugin state and shell history. **Applications
  started from this session also inherit these isolated XDG locations** and
  may not see their usual config. This is intentional opt-in isolation, not a
  sandbox: HOME, filesystem permissions and other inherited environment remain.
  Do not source the generated profile from personal startup files if isolation
  is required; use the launcher. Exit the session to restore the parent environment.

To select it in Crow Term's TOML config (replace the absolute path):

```toml
shell = "/usr/bin/python3" # or your existing Python 3.9+ absolute executable
shell_args = ["/absolute/path/to/profile/launch-fish.py"]
```

No automatic launch or shell configuration is triggered by reading the example
appearance config. The root CLI may forward `shell-setup` arguments to
`shell/setup.py`; the standalone script is the installation contract.

## Filesystem guarantees and recovery

Use a dedicated directory, not HOME or `/`. Control characters, prefix symlinks,
symlink target files/directories and conflicting existing content are rejected.
Existing identical files and their modes/timestamps are retained. Every planned
target is checked before writes. New directories are private (`0700`); data and
profile files are `0600`, launchers `0700`.

New files are written to a temporary sibling, flushed and atomically published
using a no-clobber hard link; a racing existing file is never replaced. There is
no whole-directory transaction: interruption may leave a partial set of complete
files, safely resumable by rerunning. This requires a local filesystem supporting
hard links (normal APFS/ext4/WSL Linux filesystems). Concurrent hostile mutation
of parent directories is not a supported trust model. Setup does not chmod,
delete or merge owner files, and provides no destructive uninstall command.
To abandon the profile, stop launching it; inspect/back up any new history before
manually removing its dedicated directory.

## Platform and rendering boundaries

- **macOS / Unix:** use existing fish or zsh on PATH; neither is installed globally.
- **Windows:** the Python installer's hard-link/private-mode and fish/zsh launcher
  contract is supported inside **WSL's Linux filesystem** (for example `~/.config`,
  not `/mnt/c`). Run the setup and launch within WSL. Native Windows PowerShell
  remains separate; no native fish/zsh support is claimed by this helper.
- Crowquill Mono's four faces and OFL notice are in `assets/fonts/`. The family
  name is preserved. Contextual keyword bold and ligatures require renderer
  shaping; the shell does not implement them. Emoji rendering requires renderer
  fallback and cannot be guaranteed by the programming font alone.
- Theme licensing/provenance and the personal-vs-original palette distinction
  are in `assets/README.md`. No full personal dotfiles, secrets, history, account
  configs, private title helpers, agent hooks or machine paths were imported.
- Existing tmux configuration can be selected independently. This helper neither
  replaces tmux nor copies macOS-only `pbcopy`/`open` behavior to other systems.

## Verification

```sh
python3 tests/shell_setup.py
```

Offline tests use temporary directories only: dry-run, conflict/no-overwrite,
symlink rejection, SHA mismatch, optional plugins, idempotence/permissions,
atomic publish races, quoted launch arguments and isolated environment. Available
fish/zsh binaries additionally parse profiles and load pinned plugins in an
explicit isolated child session. Missing shells are skipped; those platforms,
interactive PTY editing, renderer shaping/emoji and WSL must be tested separately.
