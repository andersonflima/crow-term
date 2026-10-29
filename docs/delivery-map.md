# Delivery map / architecture-note / native-change

Date: 2026-10-28. Target: first `0.1.0-alpha.1` implementation. This is an
engineering alpha, not a certification of complete terminal compatibility.

## Decisions and coverage

| Requirement | Produced artifact | Evidence / boundary |
|---|---|---|
| Public `crow-term` repository | Original Rust application, MIT | Owner explicitly authorized public repo |
| New engine, no fork | Own native frontend, protocol and daemon | `src/ui.rs:1`, `src/ipc.rs:1`, `src/server.rs:1`; reusable crates are attributed in NOTICE |
| macOS / Windows / Linux | Winit, native PTY/ConPTY, sockets/named pipes; CI matrix | `.github/workflows/ci.yml:1`; native build/test coverage is distinct from graphical validation |
| Vim/Neovim/LazyVim/CLIs | VT emulation, alt screen, truecolor, mouse, bracketed paste, keypad, kitty keyboard, underline styles, terminal queries | CI (Linux, macOS) with pinned upstream Neovim 0.11.7 and 0.12.6 (SHA-256, `scripts/fetch_neovim.py`): `tests/nvim_e2e.py` (keyboard chords through the real encoder, termguicolors, background, cursor shapes, undercurl in the capture, no leaked replies), `tests/protocol_probe.py` (reply matrix, also in the native job), and `tests/lazyvim_e2e.py` against LazyVim v16.0.1 from a committed `lazy-lock.json` (dashboard, which-key, snacks picker, noice cmdline, lualine, bufferline, gitsigns sign, treesitter highlight, blink menu, `:checkhealth` errors vs allowlist). CI overrides: no Mason, Lua fuzzy matcher, nvim-treesitter disabled in favour of Neovim's bundled parsers; network only in `tests/lazyvim_bootstrap.py`. Windows: Neovim 0.11.7 through ConPTY in the GUI journeys below (editing, paste, mouse, focus, Ctrl chords), not the keyboard/LazyVim suites. Not covered: Windows LazyVim, LSP servers, LazyVim extras, the user's own plugin tree |
| Native sessions/windows/panes | Daemon owns PTYs; recursive splits; named sessions and windows | `src/model.rs:1`, `src/server.rs:1`; E2E creates, resizes, reconnects and isolates sessions |
| Existing tmux configuration | Real tmux compatibility profile; native prefix disabled | `src/config.rs:1`, `src/main.rs:1`; E2E config marker/prefix and detach, not arbitrary plugin certification |
| Fish/Fisher and zsh | Existing shells plus opt-in offline isolated profiles | `shell/setup.py:1`, pinned licensed sources; real fish/zsh syntax and plugin load tested when installed |
| Crowquill Ink and Mono | Owner-selected light default, dark example, four embedded OFL faces | `src/config.rs:1`, `assets/`; exact upstream attribution preserved |
| Underline styles (Neovim diagnostics) | SGR 4:1-4:5 single/double/curly/dotted/dashed and SGR 58/59 underline color carried engine → model → IPC (wire v4) → renderers (`underline_rects`) | `src/engine.rs` and `src/render/` unit tests, daemon E2E JSON capture; geometry pinned by the software goldens and compared GPU vs software in CI |
| Terminal queries and events | XTVERSION, XTGETTCAP (Tc/RGB/Smulx/Setulc/Ss/Se, Ms only with OSC 52 copy), DECRQSS (SGR, DECSCUSR), DA2 with the crow-term version, CSI 14t/16t from the GUI's real cell metrics (also TIOCSWINSZ pixels), DEC 1004 focus on pane switch, OSC 8 links kept in the snapshot (not clickable yet), visual bell, DEC 1007 alternate scroll, window title from OSC 0/2, OSC 10/11 color queries answered with the configured theme, `CSI ? 996 n` light/dark report and mode 2031 notifications (DECRQM-queryable), `minimum_contrast` text readability on both renderers | `src/intercept.rs`, `src/engine.rs` unit tests; GUI behaviours are covered by unit tests of their pure parts; DEC 1004 focus reports are also exercised with native focus changes in GUI journey J8 (see Graphical validation), the other GUI-side behaviours are not. Pixel reports are withheld until a GUI reported cell metrics; OSC 52 strings over the cap are discarded before decoding; OSC 8 targets are bounded (64 KiB per snapshot) |
| Ligatures, icons, emojis | Advanced shaping, cell anchoring, fallback, raster cache; Private Use Area icons from an installed Symbols Nerd Font when the configured font lacks them | UI unit tests verify substitutions and grid alignment; GUI journey J2 checks `->`, accents, CJK width, colour emoji and Nerd icons in the renderer's frames against reviewed goldens on Linux (X11, Wayland), Windows and macOS (see Graphical validation) |
| Crow agent status (owner request) | Footer status line + toggleable agent panel from `crow-skills events snapshot --json`; focused-pane cwd via `PaneCwd` IPC, git branch from `.git/HEAD` | `src/crow.rs` parsing/format/throttle unit tests, `src/engine.rs` cwd test; versioned, tolerant parser; read-only, no prompt content |
| Lightweight / fast | Native renderers, no WebView; content-keyed row caches (scrolling reuses shaped rows), glyph caches, coalesced output, bounded queues/history, binary IPC, LTO | Mechanisms implemented and measured with `bench-render`; **no claim of maximum performance or comparative superiority** |
| GPU renderer | wgpu backend (Metal / DX12 / Vulkan+GL) behind `renderer = "gpu"` or `"auto"` (`--renderer`, `CROW_TERM_RENDERER`); default stays `software`. Same `Scene` and shaping as software, R8 + RGBA glyph atlases, instanced quads, non-sRGB blending, block-cursor inversion by blend state, FIFO, no idle frames, runtime fallback to software in `auto` | `src/render/gpu.rs` unit tests; CI `gpu` jobs compare offscreen GPU frames with the software renderer (Linux lavapipe, Windows WARP, macOS Metal when exposed); a real macOS window matched the software frame byte for byte. Not validated by hand on discrete GPUs, Wayland compositors, Windows hardware drivers or mixed-DPI setups |

The owner explicitly chose a **new engine**, rejecting the fork alternative, and
**Crowquill Ink Light** over the existing dark terminal profile. Reversal is by
normal code/configuration change, not hidden migration. A native mux cannot
interpret arbitrary tmux shell commands/plugins exactly; the explicit compatibility
path delegates to tmux itself rather than importing a misleading subset.

## Graphical validation (CI)

Date: 2026-10-28. Workflows `gui-linux.yml` (X11: Xvfb + openbox; Wayland:
headless sway 1.9, pixman, no Xwayland), `gui-windows.yml` (windows-2025
interactive desktop) and `gui-macos.yml` (macos-15) run the real GUI against an
isolated daemon and real PTYs/ConPTY with `tests/gui/journeys.py`. Input is
native: XTEST (xdotool), the Wayland virtual keyboard/pointer protocols (wayvnc
driven by an in-repo RFB client), `SendInput` on Windows and Quartz events plus
System Events on macOS (the hosted image grants Accessibility to its tools).
The harness never types into the PTY on these runners; a journey that has to
fall back to daemon input reports `degraded`, which `--expect-pass` rejects.

Rendering evidence is the renderer's own frame (`--features frame-dump`,
`CROW_TERM_FRAME_DUMP`, `src/frame_dump.rs`; not in release builds), so goldens
do not depend on compositor or screenshot quirks. An OS screenshot per run only
proves the window reached the screen (the frame's header strip is located in
it). Goldens live in `tests/gui/goldens/<platform>/`, were produced by CI and
reviewed by eye; consecutive CI runs produced pixel-identical frames on every
platform. The diff is tolerant (channel ≤ 24, ≤ 0.3 % of pixels, ≤ 2 % of any
cell). `workflow_dispatch` with `update_goldens` uploads candidates as an
artifact; nothing is committed automatically. Each run uploads frames, diffs,
screenshots, logs and `results.json` (14 days).

| Journey | Linux X11 | Linux Wayland | Windows | macOS |
|---|---|---|---|---|
| J1 launch, first frame, window on screen | pass | pass | pass | pass |
| J2 fixture text (ligature, accents, CJK width, colour emoji, Nerd icon, golden) | pass | pass | pass | pass |
| J3 Ctrl-t % / Ctrl-t " splits and h/l navigation (golden) | pass | pass | pass | pass |
| J4 window resize changes the PTY size | pass | pass | pass | pass |
| J5 Neovim edit + `:w` through native keys | pass | pass | pass | pass |
| J6 clipboard paste reaches Neovim as a bracketed paste | pass | pass | pass | pass |
| J7 mouse click moves the Neovim cursor | pass | pass | pass | pass |
| J8 focus out/in reach Neovim (DEC 1004) | pass | pass | pass | pass |
| J10 Ctrl+/ and Ctrl+Return reach Neovim | pass | pass | pass | pass |
| J9 window close, `attach`, unsaved buffer intact | pass | pass | pass | pass |

Neovim is the pinned 0.11.7 release on every platform (Windows: `nvim-win64.zip`
by SHA-256), so the Windows column is also the first Neovim-through-ConPTY
coverage; the keyboard/LazyVim E2E suites (`nvim_e2e.py`, `lazyvim_e2e.py`)
remain POSIX-only. On Windows Neovim 0.11 negotiates no kitty keyboard flags;
Ctrl+/ and Ctrl+Return still arrive as distinct keys there.

Defects these journeys found and that are fixed: the Ctrl-t prefix was consumed
by the Shift press needed to type `%` or `"` (and by Alt), so prefix actions on
shifted keys never worked with a real keyboard; on Wayland without Xwayland the
clipboard was unavailable (arboard's `wayland-data-control` is now enabled;
compositors without the data-control protocol, e.g. GNOME, still go through
Xwayland); on Windows a CJK system font's vendor glyph in the Private Use Area
replaced the Nerd icon (PUA cells now prefer an installed Symbols Nerd Font);
the X11 GUI aborts at startup when `libxkbcommon-x11` is missing (a runtime
dependency, now documented in [release](release.md)).

On macOS, chords are posted as Quartz key events with the modifier keys
themselves: System Events' `keystroke "/" using control down` delivered a plain
`/` (a tool artifact; the same chord as Quartz events reaches Neovim as
`<C-/>`).

Known and recorded, not fixed: a ZWJ emoji sequence (👩‍💻) is drawn as one glyph
in the first two cells while the grid model gives the second emoji its own two
cells, so text after such a sequence is visually offset (informational in J2).

What CI does not prove and still needs a person: input methods and dead keys
(only ASCII and Ctrl/Shift chords are synthesized; no IME composition, no
non-US layouts); real compositors and desktops (GNOME/KDE, decorations, fractional
and HiDPI scaling — CI runs at scale 1, single virtual monitor, no multi-monitor
or DPI change), GPU drivers (the GUI journeys use the default CPU softbuffer renderer; the
opt-in GPU renderer is compared offscreen only), screen readers and accessibility trees, perceived
latency and smoothness, WSL, and long-running sessions.

## Responsibilities and interfaces

- `engine`: PTY I/O, VT state, theme defaults, synchronized output, responses,
  bounded input, child lifecycle and Unix terminal-session cleanup.
- `server`: sole owner of sessions, windows, layouts and engines. A session keeps
  its original shell profile for subsequent windows/splits; reattaching cannot
  silently switch the shell. Explicit close/kill differs from GUI detach.
- `ipc`: local-only framed Postcard protocol, bounded frames, finite transfer
  waits; private Unix runtime/socket + EUID checks, Windows owner ACL + peer SID
  checks in both directions. The internal wire format is versioned, not public
  stable API. CLI output remains JSON.
- `ui`: native event loop, worker-based I/O, raster/shaping, keyboard/IME, mouse,
  clipboard and selection; one owner per GUI resource.
- `config`/CLI: validated TOML, explicit shell argument arrays, shell helper only
  from the bundled location or an explicitly selected `--source` directory.

## Observed verification

Local machine: Apple M3 Pro, macOS arm64, Rust 1.99.1. No global install or update.
Commands used on the produced tree:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --all-targets --locked
cargo build --locked
python3 tests/daemon_e2e.py
python3 tests/shell_setup.py
```

Results are recorded with the final revision in the PR/CI. Local iterations
included 36 passing Rust tests, six daemon E2E tests and ten shell tests; additional
regressions were added before the final run. Counts are not a substitute for
checking the final run's output.

Native GUI smoke used only a development bundle and isolated daemon: launch,
light rendering, real input/paste, ligatures/emoji/icon/CJK, split, Neovim editing,
close GUI then reopen with the same unsaved buffer still running. Screenshots
were inspected, not committed. A default-profile mismatch exposed during this
test was corrected by retaining the session's initial shell profile server-side.

Two independent read-only reviewers attempted to refute lifecycle/IPC/security
and input/rendering/performance claims. Findings were reproduced or checked
against primary crate sources, then corrected: macOS socket chmod-at-bind,
theme queries, Watch revision race, large aggregate frames, unbounded PTY queue,
CWD helper execution, background cleanup, synchronized output, mouse protocols,
mouse capture, keypad, macOS Option, large paste, and fallback baseline alignment.
The first three-panes JSON snapshot was ~15.6 MB; the binary protocol measured
~1.44 MB for four and ~5.77 MB for sixteen panes of 40,000 cells each. These are
wire-size observations, not rendering/latency benchmarks.

## Explicitly not verified / remaining work

- What the graphical CI cannot show (see Graphical validation): IME and
  non-US layouts, real desktops/compositors, HiDPI and fractional scaling,
  multi-monitor, GPU drivers, screen readers, perceived latency, WSL; ZWJ
  emoji sequences are drawn offset from the grid.
- Every Vim/LazyVim plugin, terminal protocol or keyboard extension. Graphics
  protocols, sixel and tmux control-mode integration are not advertised as
  implemented. The kitty keyboard protocol (flags 1/2/4/8/16, push/pop/query/set)
  and xterm modifyOtherKeys are implemented (`src/keyboard.rs`,
  `src/intercept.rs`) with reference-table unit tests and real-Neovim decoding in
  `tests/nvim_e2e.py`; caps/num-lock modifier bits are not reported (winit does
  not expose lock state), Super/Command chords stay GUI shortcuts, Shift+Space
  is a plain space unless the application requests flag 8 (kitty semantics), and
  the encoder is exercised end-to-end through the hidden `debug-encode-key`
  command, not through synthetic native key events. A bound of 256 kitty pushes
  per screen guards an upstream panic in alacritty_terminal 0.26; a query that
  the engine answers itself inside a synchronized-output block (DEC 2026)
  commits the pending block first, so answers keep stream order.
- Semantic screen-reader grid accessibility, GUI session switcher, draggable
  split ratios, search UI, crash/reboot layout restore and process persistence
  beyond daemon lifetime.
- Manual GPU validation on real drivers (discrete/integrated GPUs on Windows and
  Linux, Wayland compositors, HiDPI and mixed-DPI moves) before `auto` becomes
  the default; end-to-end input latency and present-time comparisons on a
  visible window (`CROW_TERM_FRAME_STATS`).
- Release pipeline exists (`.github/workflows/release.yml`, [release](release.md)):
  macOS Developer ID signing and notarization run only when the signing/notary
  secrets are configured, otherwise builds are explicitly unsigned; Linux
  artifacts get keyless build provenance; Windows Authenticode is a documented
  TODO (no certificate). Homebrew formula/cask templates are rendered by
  `scripts/update_tap.py`. Not yet verified: a real signed/notarized run on
  GitHub, Linuxbrew runtime dependencies, auto-update. Linux libc/desktop
  requirements still follow the build environment.
- IPC stale-socket recovery after ungraceful daemon termination is currently
  manual: verify no daemon is alive before removing its private socket. A daemon
  crash cannot preserve its child processes.
- OS-detached processes that deliberately create new sessions are not owned by
  the pane's Unix terminal session. Windows descendant/job-object cleanup needs
  native integration evidence rather than extrapolation from Unix tests.
- Independently attached GUI clients can interleave input to the same pane;
  paste ordering is serialized within one GUI, not a global interactive lease.

## Effects, safety and continuation

Only allowlisted font/theme/plugin sources were copied, retaining licenses and
hashes. Existing user dotfiles, histories, credentials, MCPs, shells and installed
terminals were not modified. The public tree was scanned for secret/machine-path
patterns before publication; this does not claim a formal full dependency audit.
`THIRD_PARTY_LICENSES.txt` preserves notices for the locked all-target Cargo graph;
`scripts/third_party_notices.py --check` checks freshness. Missing crate-package
notices are cached from immutable official upstream revisions. Two upstream
source trees omit original license files; their MIT declarations and a labelled
standard template are explicitly distinguished from original copyright notices.
OSC 52 from terminal output is copy-only by owner decision (2026-10-28): writes
are accepted by default (`[terminal] osc52 = "copy"`, opt out with `"off"`),
capped at 1 MiB, delivered to attached GUIs on demand, and advertised (XTGETTCAP
`Ms`) only when enabled; clipboard read queries are never answered. Explicit
user clipboard shortcuts remain enabled. Nonexistent/unknown config fields fail explicitly.

Network: official documentation/crate/upstream sources and the authorized GitHub
repo/CI. External writes: coordination ledger and memory records; disposable
runtime/profile test files. Test GUI/daemon processes are closed after verification.
No hostile instruction was identified in the inspected files; no assertion of
an exhaustive hostile-content scan across every transitive dependency is made.

Next owner: Crow Term maintainers, with available `crow-qa` and `crow-performance`
skills, using the CI artifacts, E2E tests and limitations above. Do not replace
these limits with an unqualified “works without problems on every platform.”
