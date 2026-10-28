# LazyVim E2E fixture

A pinned LazyVim configuration for `tests/lazyvim_e2e.py`.

- `nvim/init.lua`, `nvim/lua/config/*.lua`: from
  [LazyVim/starter](https://github.com/LazyVim/starter) at commit
  `803bc181d7c0d6d5eeba9274d9be49b287294d99`, Apache-2.0 (`LICENSE.starter`).
  `lazy.lua` is modified (see its header); the other files are empty stubs.
- `nvim/lua/plugins/ci.lua`: Crow Term overrides (no Mason, Lua fuzzy matcher
  for blink.cmp, no treesitter parser installs).
- `nvim/lazy-lock.json`: exact plugin commits, generated once with LazyVim
  `v16.0.1`; `tests/lazyvim_bootstrap.py` restores exactly these.

Only `tests/lazyvim_bootstrap.py` uses the network. The tests run offline
against the bootstrapped plugin directory (cached in CI by the lockfile hash).
