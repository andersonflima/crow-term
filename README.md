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

### Links, copy and paste

- **Links:** `Command`-click (macOS) or `Ctrl`-click (Linux, Windows) opens the
  link under the pointer: an OSC 8 hyperlink, or a URL in the text, also when
  it wraps across lines. Holding the modifier underlines the link, shows a hand
  pointer and puts the real target in the status line (`scheme://host:port`
  always in full, the host in punycode, invisible characters escaped; only the
  path is shortened). The click is the terminal's; the program never gets it.
  - `http`/`https` open directly, unless the URL has a user name before the
    host or an OSC 8 text names a different host anywhere in it: then a second,
    deliberate click (at least 0.5 s later, button released in between, within
    5 s) is required. URLs with a backslash, spaces or control characters are
    refused (browsers read `\` as `/`).
  - `mailto` opens with only `to`, `cc`, `bcc`, `subject` and `body` kept.
  - Local `file:` links ask first, then resolve symlinks and `..`, and open
    only a regular, non-executable document: `txt`, `md`, `markdown`, `pdf`,
    `png`, `jpg`, `jpeg`, `gif`, `webp`, `csv`, `tsv`, `log`, `json`. Anything
    else (folders, `.app` bundles and paths inside them, HTML, SVG, source code,
    scripts, `.webloc`, `.lnk`, `.exe`…) is shown in Finder/Explorer, never
    opened. Remote and UNC `file:` links are refused.
  - Every other scheme is refused. Links open through the OS opener (`open`,
    `xdg-open`, the Windows URL handler) with the URL as one argument, never a
    shell.
- **Paste** (`Ctrl-Shift-V`, `Command-V`) takes, in this order: copied
  **files** (pasted as paths), an **image** alone on the clipboard (written as a
  PNG to a private `pasted-images` cache directory, mode 0700, files older than
  24 h removed, and its path pasted), else **text** (bracketed paste; every ESC
  in it is removed so it cannot end the paste). Files dropped on the window
  paste their paths the same way. A path is pasted as `'<path>'`, a form every
  shell (sh, bash, zsh, dash, ksh, fish, tcsh, PowerShell) reads verbatim; a
  name containing `'`, `\`, `!`, `$`, a backtick or a control character (on
  Windows also `& | ^ % ( ) < > "`) is not pasted, and the status line says
  why. Plain `Ctrl-V` and `Ctrl-C` are never taken: they reach the program as
  keys (agent CLIs read clipboard images on `Ctrl-V`).
- **Copy** (`Ctrl-Shift-C`, `Command-C`) copies the selection as plain text:
  soft-wrapped lines are joined, wide characters and emoji kept, trailing
  blanks of each line trimmed. OSC 52 is unchanged (below).

## Tab context: status bar and tab labels

The footer describes the **focused pane**, most important first; when the
window is narrow, whole low-priority parts are dropped first, and a part that
still has to be shortened keeps its distinctive end (`crow-te…/feature/x`) or
ends at a word. Click a shortened part to read it whole.

```text
claude ● working · fix the login flow · 3/8 · 37% ▰▰▰▱▱▱▱▱ · opus-5.5 · ctx 57% · crow-term@main      crow on · 2 active
nvim main.rs · ~/w/crow-term                                                                           crow on
```

- With an agent CLI (claude, codex, gemini, agy, devin) in the foreground of
  the pane: the agent and its state, its task (the crow-behaviour run of its
  directory) or tool, the run's progress, model and context use. A crow
  session belongs to the pane when its process is the pane's foreground job
  or one of its descendants (whatever it is called: `node`, `bun`, a
  wrapper). Otherwise the foreground must be that agent CLI, by name or by
  the script a runtime runs (`node …/bin/gemini`, `@anthropic-ai/claude-code`;
  the command line is read by the daemon and never sent to the GUI), and the
  session is the one on the pane's terminal, else the only one of that CLI in
  the pane's directory (two there: nothing is guessed). Sessions whose process
  is gone are ignored.
- Otherwise: the foreground program (with one argument only for editors and
  build tools, and only if it cannot be a secret) and the directory.
- `repo@branch`, read from the `.git` files of the pane's directory. No `git`
  command is ever run for it, so nothing a repository configures (hooks,
  filters, `core.fsmonitor`) can run because a pane entered it; uncommitted
  and ahead/behind marks are therefore not shown.
- The session name only when more than one session exists.
- On the right, one global part: crow on/off, the number of agent sessions
  working or blocked, memory only under pressure.
- Help for the prefix keys is shown while the prefix is armed, not all the time.

Tab labels follow what runs in each tab: a name you gave the window
(rename) wins, then an agent CLI in the foreground and its task, a title the
foreground program set (OSC 0/2; not a shell's prompt title, a path,
`user@host: dir`, or the program's own name, and never ConPTY's executable
path), the foreground program, `repo@branch`, the shell. Text from programs
and agents is shown without control, bidi or zero-width characters. A label changes only after
holding for 0.6 s (a quick `ls` does not flash it), is shortened in the middle,
and hovering a shortened tab shows it whole. `tab_labels = false` keeps the
window names. The daemon refreshes each pane's summary (foreground program,
title, directory) on its own thread once a second, never while answering a
request, and wakes the GUI only when one changes.

## Crow agent panel

When the [`crow-skills`](https://github.com/andersonflima/crow-skills) CLI is
installed, the status bar above includes agent sessions and runs, and prefix
`a` toggles an agent list panel:

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
- Without `crow-skills` the agent parts are hidden (program, directory and
  repository remain).

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

### Readable text on any background

Programs that hard-code colors for one kind of background (a dark fish or
Neovim theme in truecolor, say) would draw pale text on the light Crowquill Ink
background. `minimum_contrast` (default `4.5`, WCAG contrast ratio; `1.0`
turns it off, up to `21.0`) darkens or lightens such text, keeping its hue,
until it reaches the ratio against its cell background (after reverse video,
dim and selection). Text that already meets it is drawn unchanged, and so are
color emoji, box drawing, block elements, legacy-computing shapes and Powerline
separators (U+E0B0–E0D7; Powerline's branch/line/padlock icons are adjusted
like text). Both
renderers apply it identically. `4.5` is WCAG AA for normal-size text, which
terminal text is. Deliberately quiet colors (comments, autosuggestions) of
some themes come out a little darker or lighter; set `minimum_contrast = 3.0`
(the WCAG minimum for large text) for a subtler look, or `1.0` to turn it off.
On a mid-grey background, where neither black nor white reaches 4.5 by much,
the adjustment can move text all the way to the other end: white on `#777777`
becomes near-black (`#060606`). That is by design (it is the readable choice);
`3.0` avoids it for such themes.

Programs can also ask instead of guessing: OSC 10/11 queries answer with the
configured foreground/background, `CSI ? 996 n` answers `CSI ? 997 ; 2 n`
(light) or `; 1 n` (dark), and mode 2031 (`CSI ? 2031 h`) can be enabled and
queried with DECRQM. Light means black text contrasts more with the
background than white text does; the window chrome uses the same rule. Flip
notifications under mode 2031 are only sent when a pane's theme changes, and
crow-term does not change themes of running panes yet (the theme is applied
when a pane starts), so in practice none are sent today. fish 4.x uses the
queries to set `$fish_terminal_color_theme`; themes saved with light and dark
variants then pick the right one when fish starts. Colors fixed in universal variables (`set -U
fish_color_normal ...`) do not change, which is what `minimum_contrast` covers.

### Clipboard from programs (OSC 52)

Programs in the terminal (Neovim, tmux, remote shells over SSH) may **copy** to
the system clipboard with OSC 52. This is on by default because editors rely on
it to yank across SSH. Reading the clipboard back is never allowed, in any mode,
and a single copy is capped at 1 MiB. The trade-off: any program whose output
reaches the terminal (including `cat` of an untrusted file) can replace your
clipboard contents, though it cannot read them. To turn it off:

```toml
[terminal]
osc52 = "off"   # "copy" (default) or "off"; any other value is rejected
```

The capability is advertised to applications (XTGETTCAP `Ms`) only in `copy` mode.

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
python3 tests/protocol_probe.py
python3 tests/nvim_e2e.py   # optional: CROW_TERM_NVIM=/path/to/nvim (>= 0.11)
# LazyVim (pinned): one network bootstrap, then offline tests
python3 tests/lazyvim_bootstrap.py --home /tmp/crow-lazyvim
CROW_LAZYVIM_HOME=/tmp/crow-lazyvim python3 tests/lazyvim_e2e.py
```

The E2E suite uses an isolated daemon/runtime and real PTYs. Available Neovim and
tmux binaries receive separate integration tests; missing optional tools are
reported as skipped, not silently counted as verified. See the delivery map for
observed platform coverage and remaining work.

## Rendering and performance

Two renderers draw the same frame description (`src/render/`): OpenType shaping
and fallback use COSMIC Text, every shaped cluster is anchored to its cell, and
rows are cached by content, so scrolled rows are not shaped again.

| `renderer` | What it does |
| --- | --- |
| `software` (default) | Native CPU framebuffer (`softbuffer`). |
| `gpu` | wgpu: Metal on macOS, DX12 on Windows, Vulkan or GL on Linux. Failing to start is an error. |
| `auto` | GPU when a hardware adapter works; otherwise the software renderer, with the reason in the status line and on stderr. CPU rasterizers (llvmpipe, lavapipe, WARP) are not used by `auto`. A GPU device lost at runtime switches to software. |

Choose with `--renderer auto|gpu|software`, then `CROW_TERM_RENDERER`, then
`renderer = "..."` in the config (invalid values are rejected). `software`
stays the default until the GPU path has been validated by hand on real drivers,
Wayland compositors and HiDPI displays; `auto` will become the default only
after that. `crow-term doctor` prints the effective renderer, where it came
from, and the GPU adapter that `gpu`/`auto` would use.

The GPU renderer uses a non-sRGB surface so blending matches the software
framebuffer: in CI, offscreen GPU frames of the real UI composition must match
the software frames within 2 per channel on at most 0.1% of the pixels (Metal
and Windows WARP match exactly). On CPU rasterizer adapters (Mesa lavapipe in
the Linux job) fixed-function blending rounds one step differently on 0.2–2.1%
of the pixels of the fixture frames; those one-step differences are not counted
there, while the 2-per-channel bound still applies. The software renderer itself
must match its goldens in `tests/goldens/render/` exactly. Frames are only drawn on
request (no idle frames), the surface is configured only on resize/DPI changes,
and presentation is vsync (FIFO).

Redraws are event-driven, output updates are coalesced to at most 60 Hz, queues
and scrollback are bounded, and release builds use thin LTO. For measurements,
`crow-term bench-render --backend software|gpu --cols 120 --rows 40` times
offscreen frames (idle, scroll, one changed row; p50/p95/p99, JSON) and
`CROW_TERM_FRAME_STATS=1` prints the draw+present times of a real window on
exit. These are measurements, not a claim of maximum performance.

## Licensing and upstream components

Application: MIT. Crowquill Mono: SIL OFL 1.1, with original family name and
notices preserved. Crowquill Ink: MIT. Plugin manifests contain exact upstream
commits, hashes and licenses. See `assets/`, `shell/plugins.lock.json`, and
[`NOTICE.md`](NOTICE.md). No personal full dotfiles or credentials are bundled.
