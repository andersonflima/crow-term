# Crow Term development

- New native Rust application, not a fork or a WebView terminal.
- Trunk-based: short `feature/`, `fix/`, or `chore/` branches from `main`, PR back
  to `main`, squash after CI. No `develop` branch. Conventional Commits.
- Keep platform differences explicit. A cross-platform compile is not manual
  verification of input, rendering, WSL, signing, or packaging.
- Never copy full personal dotfiles, environment files, histories or credentials
  into this public repository. Vendor only licensed allowlists with provenance.
- PTYs belong to the daemon. Closing GUI must not terminate them. Explicit
  kill/close must clean up the terminal's owned processes.
- Preserve bounded frames/queues/scrollback and event-driven idle behavior.
- Run formatting, clippy, Rust tests, isolated shell tests, and daemon E2E tests.
- Do not claim complete tmux config interpretation: compatibility uses real tmux.
