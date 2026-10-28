# Third-party notices

Crow Term is an original application, not a fork of another terminal.

- `alacritty_terminal`: Apache-2.0; reusable VT parser/grid, not Alacritty's GUI.
- `portable-pty`: MIT; portable PTY library from the WezTerm project, not its GUI.
- `winit`, `softbuffer`: Apache-2.0/MIT; native windowing and framebuffer.
- `cosmic-text`: Apache-2.0/MIT; shaping, fallback and rasterization.
- Other Rust dependency versions and license metadata are recorded by
  `Cargo.lock` and upstream package manifests. Transitive licenses still apply.
- Crowquill Mono font files: `assets/fonts/OFL.txt`; provenance in that directory.
- Crowquill Ink theme: `assets/theme-license.txt`.
- Vendored shell plugins: `shell/plugins.lock.json`; notices shipped alongside
  each source package under `shell/vendor/`.

Bundled assets retain their original names. Crow Term does not claim authorship
of those upstream libraries, fonts, themes or plugins.
