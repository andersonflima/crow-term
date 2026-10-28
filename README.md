# Crow Term

A new native terminal in Rust. **Not a fork, WebView, or configuration wrapper.**

Crow Term combines an independent frontend/session daemon with reusable PTY,
VT-emulation and OpenType-shaping libraries. The goal is a small, responsive
terminal for command-line tools and Vim/Neovim/LazyVim.

**Status: early alpha, not a claim of finished terminal parity.** See
[coverage and limitations](docs/delivery-map.md). Native macOS/Windows/Linux
builds are checked in CI; a build is not the same as manual validation of every
platform, keyboard layout, graphics driver or editor setup.

## Install

Prebuilt alpha archives for macOS (arm64, x86_64), Linux (arm64, x86_64) and
Windows (x86_64) are attached to each
[GitHub Release](https://github.com/andersonflima/crow-term/releases), with
`SHA256SUMS` and build provenance (`gh attestation verify <file> --repo andersonflima/crow-term`).

With Homebrew (pick one; both provide `crow-term`):

```sh
brew install andersonflima/tap/crow-term          # command-line install, macOS and Linux
brew install --cask andersonflima/tap/crow-term   # Crow Term.app in /Applications, macOS
```

The macOS app is signed and notarized only when the release says so; Windows
builds are not code-signed yet. See [docs/release.md](docs/release.md).

## Run from source

Install a current stable Rust toolchain and your preferred shell, then:

```sh
cargo build --release --locked
./target/release/crow-term
# Windows: .\target\release\crow-term.exe
```

Fish is selected automatically on Unix when available, otherwise `$SHELL` or
`/bin/sh`. Windows defaults to PowerShell using native ConPTY. Fish, zsh and tmux
on Windows run **inside an installed WSL distribution**, not as bundled Win32
shells:

```sh
crow-term --session linux --shell wsl.exe start -- --exec fish -l
```

No installer runs, no login shell is changed, and no personal dotfile is copied.
The four Crowquill Mono font faces are embedded; system emoji fallback fonts
remain necessary. Default: **Crowquill Ink Light**, ligatures enabled. The dark
personal profile is available in `assets/crowquill-example.toml`.

## Persistent native sessions

```sh
crow-term --session work
crow-term list-sessions
crow-term attach work
crow-term --session work new-window
crow-term --session work split-pane 1 --window 2
crow-term --session work rename-window 2 editor
crow-term kill-session work
crow-term stop              # refuses while sessions exist
crow-term stop --force      # explicitly terminates all sessions
```

IDs are returned by creation commands; `1`/`2` above are examples, not fixed IDs.
Close the GUI or use prefix `d` to detach: **the daemon keeps the same PTYs and
processes alive**. This is not process survival across daemon crash/reboot.
No command history, terminal content or session environment is persisted to disk
by the daemon. Shells can maintain their own histories normally.

The default native prefix is `Ctrl-t`, matching the originating setup:

| After prefix | Action |
|---|---|
| `c` | New window/tab |
| `%` / `"` | Split side-by-side / stacked |
| `n` / `p`, `1`..`9` | Switch window |
| `h`, `j`, `k`, `l` | Focus adjacent pane |
| `x` | Close focused pane and its process |
| `a` | Toggle the crow agent panel |
| `d` | Detach GUI |
| `Ctrl-t` again | Send a literal `Ctrl-t` |

Without the prefix, Vim control chords go to the PTY. Clipboard shortcuts are
`Ctrl-Shift-C/V` and `Command-C/V` on macOS. Scrollback, selection, bracketed paste,
truecolor, alternate screen and application mouse reporting are implemented.

## Crow agent status bar and panel

When the [`crow-skills`](https://github.com/andersonflima/crow-skills) CLI is
installed, the footer shows a thin status line for agent CLIs running on the
machine, and prefix `a` toggles an agent list panel:

```text
▸▸ crow on   claude 2  codex 1   ⚠ 1 blocked   Crow panel 3/7   mem 41%   ⎇ main
```

- crow mode (`on`/`off`, coloured), live sessions per CLI, blocked/error
  sessions highlighted, the active crow-behaviour run `title done/total`
  (preferring the run that contains the focused pane's directory), memory
  pressure when `crow-skills` reports it (0.9.0+), and the git branch of the
  focused pane's working directory (read from `.git/HEAD`, no `git` subprocess).
- The panel lists each running run as `● main  <title>  x/y`, one
  `○ <owner>  <activity>` line per running unit and a `✓ done ✗ failed pending`
  count line, then each session (CLI, state, tool, age; project@branch, model,
  context/tokens, cost, subagents) and a summary line.
- Data comes from `crow-skills events snapshot --json` (`crow-snapshot/1`), run on
  a worker thread with at most one call in flight, only when crow's state files
  change (at most every 5 s while agents work) or the data is stale (15 s while
  agents work, 60 s idle; 4x slower while the window is unfocused), with a 5 s
  timeout that kills the launcher's whole process group, a 2 MiB output cap,
  exponential back-off on failure and bounded row counts. One call costs about
  0.14 s CPU (crow-skills is node + python). Unknown fields and newer minor
  versions are ignored; an unknown major version is shown as `crow ?`. The source
  never contains prompt, message or tool I/O text.
- Without `crow-skills` the crow segments are hidden (the branch remains).

```toml
[crow]
enabled = true            # false: no worker thread, no subprocess
status_bar = true
panel = false             # visible at startup; prefix `a` toggles it
panel_position = "right"  # or "bottom"
command = "crow-skills"   # name or absolute path; Homebrew/~/.local/bin are searched too
interval_ms = 2000        # 1000..60000
```

## Reuse an existing tmux configuration exactly

```sh
crow-term --session tmux-work --tmux work
```

This runs **real tmux**, which reads its existing config/plugins/helpers itself.
Crow Term disables its native prefix in this mode so it cannot steal tmux's
`Ctrl-t`. Windows uses `wsl.exe --exec tmux ...` and reads the configuration in
that WSL distribution. OS-specific tmux commands such as `pbcopy` still need
their normal platform adaptations.

The native mux does **not** interpret arbitrary `.tmux.conf`, TPM plugins,
`run-shell`, or control mode. Running real tmux is the compatibility path, not a
lossy importer disguised as equivalent functionality. Use a separate Crow
session name for tmux and native profiles: reattaching intentionally preserves
the original process instead of replacing it with a different shell.

## Configuration and shell profiles

```sh
crow-term config --path
crow-term config
crow-term --config /absolute/path/config.toml
crow-term doctor
```

The config is TOML with validated fields, explicit colors, font size/family,
ligature toggle, scrollback, shell and argument array. It is never written
automatically. Config changes for newly created PTYs require restarting the
daemon after safely closing/detaching existing sessions; do not use `stop --force`
to reload if you want to keep those processes.

[Isolated shell setup](docs/shell-integration.md) supplies Fisher + `z`, and zsh
autosuggestions + syntax-highlighting from pinned, licensed, vendored sources:

```sh
python3 shell/setup.py --shell fish --prefix /dedicated/profile --install-plugins --dry-run
python3 shell/setup.py --shell fish --prefix /dedicated/profile --install-plugins
```

The setup is offline, opt-in and refuses to overwrite conflicting files. It does
not install fish/zsh/Python globally. The generated isolated shell profile also
isolates XDG paths for child applications; ordinary default shells keep the
user's existing Vim/LazyVim configuration. Read that trade-off before selecting
the isolated profile.

## Verify

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
python3 tests/shell_setup.py
python3 tests/daemon_e2e.py
```

The E2E suite uses an isolated daemon/runtime and real PTYs. Available Neovim and
tmux binaries receive separate integration tests; missing optional tools are
reported as skipped, not silently counted as verified. See the delivery map for
observed platform coverage and remaining work.

## Rendering and performance

The initial renderer uses a **native software framebuffer** (`softbuffer`), not
GPU text rendering. OpenType shaping and fallback use COSMIC Text; rows and glyphs
are cached, redraws are event-driven, output updates are coalesced to at most
60 Hz, queues and scrollback are bounded, and release builds use thin LTO.
“Optimized to the maximum” is a target to measure, not an honest certification
for a new terminal. GPU rendering and comparative latency/throughput benchmarks
remain explicit follow-up work.

## Licensing and upstream components

Application: MIT. Crowquill Mono: SIL OFL 1.1, with original family name and
notices preserved. Crowquill Ink: MIT. Plugin manifests contain exact upstream
commits, hashes and licenses. See `assets/`, `shell/plugins.lock.json`, and
[`NOTICE.md`](NOTICE.md). No personal full dotfiles or credentials are bundled.
