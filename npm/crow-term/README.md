# @andespindola/crow-term

[Crow Term](https://github.com/andersonflima/crow-term): a native terminal in
Rust for command-line tools and Vim/Neovim/LazyVim, with persistent sessions.
Early alpha; see the project's coverage and limitations before relying on it.

```sh
npm i -g @andespindola/crow-term
crow-term            # open a window (session "main")
crow-term --help
```

npm installs exactly one platform package with the native binary
(`@andespindola/crow-term-<os>-<cpu>`: darwin-arm64, darwin-x64, linux-x64,
linux-arm64, win32-x64) through optional dependencies; this package only holds a
small launcher. Nothing is downloaded by install scripts. Do not install with
`--omit=optional` / `--no-optional`, or the binary is missing.

The macOS binary is the signed (and, when the release says so, notarized) app
bundle; Windows builds are not code-signed yet. Linux needs libxkbcommon, plus
libxkbcommon-x11 on X11 and libwayland-client on Wayland.

MIT licensed; third-party notices are in `THIRD_PARTY_LICENSES.txt`.
