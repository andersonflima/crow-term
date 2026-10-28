# Delivery map / architecture-note / native-change

Date: 2026-10-28. Target: first `0.1.0-alpha.1` implementation. This is an
engineering alpha, not a certification of complete terminal compatibility.

## Decisions and coverage

| Requirement | Produced artifact | Evidence / boundary |
|---|---|---|
| Public `crow-term` repository | Original Rust application, MIT | Owner explicitly authorized public repo |
| New engine, no fork | Own native frontend, protocol and daemon | `src/ui.rs:1`, `src/ipc.rs:1`, `src/server.rs:1`; reusable crates are attributed in NOTICE |
| macOS / Windows / Linux | Winit, native PTY/ConPTY, sockets/named pipes; CI matrix | `.github/workflows/ci.yml:1`; native build/test coverage is distinct from graphical validation |
| Vim/Neovim/LazyVim/CLIs | VT emulation, alt screen, truecolor, mouse, bracketed paste, keypad | `src/engine.rs:1`, `src/ui.rs:1`; CI runs `nvim --clean` through the daemon PTY with pinned upstream Neovim 0.11.7 and 0.12.6 (SHA-256, `scripts/fetch_neovim.py`) on Linux and macOS, plus distro Neovim in the Linux native job. Windows Neovim and LazyVim itself are **not** covered by CI yet; the user's LazyVim plugin tree is not certified |
| Native sessions/windows/panes | Daemon owns PTYs; recursive splits; named sessions and windows | `src/model.rs:1`, `src/server.rs:1`; E2E creates, resizes, reconnects and isolates sessions |
| Existing tmux configuration | Real tmux compatibility profile; native prefix disabled | `src/config.rs:1`, `src/main.rs:1`; E2E config marker/prefix and detach, not arbitrary plugin certification |
| Fish/Fisher and zsh | Existing shells plus opt-in offline isolated profiles | `shell/setup.py:1`, pinned licensed sources; real fish/zsh syntax and plugin load tested when installed |
| Crowquill Ink and Mono | Owner-selected light default, dark example, four embedded OFL faces | `src/config.rs:1`, `assets/`; exact upstream attribution preserved |
| Underline styles (Neovim diagnostics) | SGR 4:1-4:5 single/double/curly/dotted/dashed and SGR 58/59 underline color carried engine → model → IPC (wire v4) → software renderer (`draw_underline`) | `src/engine.rs` and `src/ui.rs` unit tests, daemon E2E JSON capture; raster geometry is unit-tested, not screenshot-verified on every platform |
| Terminal queries and events | XTVERSION, XTGETTCAP (Tc/RGB/Smulx/Setulc/Ss/Se, Ms only with OSC 52 copy), DECRQSS (SGR, DECSCUSR), DA2 with the crow-term version, CSI 14t/16t from the GUI's real cell metrics (also TIOCSWINSZ pixels), DEC 1004 focus on pane switch, OSC 8 links kept in the snapshot (not clickable yet), visual bell, DEC 1007 alternate scroll, window title from OSC 0/2 | `src/intercept.rs`, `src/engine.rs` unit tests; GUI behaviours are covered by unit tests of their pure parts, not by native-event automation. Pixel reports are withheld until a GUI reported cell metrics; OSC 52 strings over the cap are discarded before decoding; OSC 8 targets are bounded (64 KiB per snapshot) |
| Ligatures, icons, emojis | Advanced shaping, cell anchoring, fallback, raster cache | UI unit tests verify substitutions and grid alignment; native Mac screenshot verified `->`, `!=`, `===`, accents, CJK, rocket emoji and Nerd Font icon |
| Crow agent status (owner request) | Footer status line + toggleable agent panel from `crow-skills events snapshot --json`; focused-pane cwd via `PaneCwd` IPC, git branch from `.git/HEAD` | `src/crow.rs` parsing/format/throttle unit tests, `src/engine.rs` cwd test; versioned, tolerant parser; read-only, no prompt content |
| Lightweight / fast | Native software renderer, no WebView; dirty row/glyph caches, coalesced output, bounded queues/history, binary IPC, LTO | Mechanisms implemented, **no claim of maximum performance or comparative superiority**; GPU rendering remains follow-up |

The owner explicitly chose a **new engine**, rejecting the fork alternative, and
**Crowquill Ink Light** over the existing dark terminal profile. Reversal is by
normal code/configuration change, not hidden migration. A native mux cannot
interpret arbitrary tmux shell commands/plugins exactly; the explicit compatibility
path delegates to tmux itself rather than importing a misleading subset.

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

- Full manual Windows/WSL and Linux/Wayland/X11 journeys, including GPU/driver,
  international input, IME, complex emoji ZWJ clusters and accessibility.
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
- GPU renderer and controlled comparative startup/RSS/throughput/input-latency
  benchmarks. Initial software rasterization is deliberate, not a GPU claim.
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
