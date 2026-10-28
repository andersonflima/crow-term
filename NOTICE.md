# Third-party notices

Crow Term is an original application, not a fork of another terminal.

- `alacritty_terminal`: Apache-2.0; reusable VT parser/grid, not Alacritty's GUI.
- `portable-pty`: MIT; portable PTY library from the WezTerm project, not its GUI.
- `winit`, `softbuffer`: Apache-2.0/MIT; native windowing and framebuffer.
- `cosmic-text`: Apache-2.0/MIT; shaping, fallback and rasterization.
- Other Rust dependency versions and license metadata are recorded by
  `Cargo.lock` and upstream package manifests. Original license/copyright texts
  are included in `THIRD_PARTY_LICENSES.txt`, not only SPDX labels or links.
  That bundle explicitly identifies two upstream omissions (`dispatch` and
  `drm-fourcc`): their original manifests declare MIT but their source trees omit
  a license file. Their source declarations and a separately labelled standard
  MIT template are retained, without inventing copyright notices.
- Crowquill Mono font files: `assets/fonts/OFL.txt`; provenance in that directory.
- Crowquill Ink theme: `assets/theme-license.txt`.
- Vendored shell plugins: `shell/plugins.lock.json`; notices shipped alongside
  each source package under `shell/vendor/`.
- Test fixture only (not shipped): `tests/lazyvim/` contains files from
  LazyVim/starter (Apache-2.0, `tests/lazyvim/LICENSE.starter`); provenance in
  `tests/lazyvim/README.md`.

Bundled assets retain their original names. Crow Term does not claim authorship
of those upstream libraries, fonts, themes or plugins.
