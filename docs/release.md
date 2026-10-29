# Releasing Crow Term

The `Release` workflow (`.github/workflows/release.yml`) builds on native runners,
signs and notarizes the macOS app when the secrets exist, and publishes a GitHub
Release on a `v*` tag. Homebrew is updated afterwards from a local tap checkout.

## Cut a release

1. On a `chore/` branch, set `version` in `Cargo.toml` (for example
   `0.1.0-alpha.2`), run `cargo update -p crow-term` so `Cargo.lock` records
   it, and merge to `main` after Native CI is green.
2. Tag the merged commit and push the tag:
   ```sh
   git tag v0.1.0-alpha.2 && git push origin v0.1.0-alpha.2
   ```
   The tag must equal `v` + the Cargo version, or the workflow stops. A tag with
   `-` (alpha, beta, rc) becomes a GitHub *prerelease*.
3. The publish job waits for a successful Native CI run of the tagged commit,
   attests build provenance, creates the release and uploads every archive,
   its `.sha256` and a combined `SHA256SUMS`.
4. Update the tap with the command printed in the run summary (see below).

Dry run: **Actions → Release → Run workflow** on any branch builds, signs and
notarizes (if configured) and keeps the results as workflow artifacts only. It
creates no release and no attestation.

## Assets

`crow-term-<version>-<os>-<arch>.<ext>`, `os` = `macos` | `linux` | `windows`,
`arch` = `arm64` | `x86_64`:

| Runner | Assets |
|---|---|
| `macos-15` | `…-macos-arm64.tar.gz`, `…-macos-arm64.app.zip` |
| `macos-15-intel` | `…-macos-x86_64.tar.gz`, `…-macos-x86_64.app.zip` |
| `ubuntu-24.04` | `…-linux-x86_64.tar.gz` |
| `ubuntu-24.04-arm` | `…-linux-arm64.tar.gz` |
| `windows-2025` | `…-windows-x86_64.zip` |

Each has `<asset>.sha256`. The macOS `.tar.gz` holds the portable tree with
`Crow Term.app` and a `crow-term` link into it; the `.app.zip` holds only the
app (used by the cask). The bundle is self-contained: the shell setup lives in
`Contents/Resources/shell`. Linux binaries follow the glibc of Ubuntu 24.04.
At run time the Linux GUI loads libxkbcommon dynamically, plus
libxkbcommon-x11 on X11 and libwayland-client on Wayland (Debian/Ubuntu:
`libxkbcommon0 libxkbcommon-x11-0 libwayland-client0`). Without
libxkbcommon-x11 the X11 GUI aborts at startup with "Library
libxkbcommon-x11.so could not be loaded" (seen in the GUI journeys CI).

## Secrets and variables

Repository secrets (Settings → Secrets and variables → Actions). All optional:

| Secret | Content |
|---|---|
| `CSC_LINK` | Developer ID Application certificate + private key as a `.p12`, **base64** encoded (`base64 -i cert.p12`) |
| `CSC_KEY_PASSWORD` | Password of that `.p12` |
| `APPLE_API_KEY_P8` | App Store Connect API key `.p8`: the raw PEM text or its base64 encoding |
| `APPLE_API_KEY_ID` | Key ID of that API key (10 characters) |
| `APPLE_API_ISSUER` | Issuer ID (UUID) of the App Store Connect team |
| `APPLE_TEAM_ID` | Apple Developer Team ID; must equal the signing certificate's team |
| `WINDOWS_CERT_PFX` | Reserved for Authenticode (not implemented; see below) |

Repository variable `MACOS_SIGN_IDENTITY` overrides the default identity
`Developer ID Application: Anderson Espindola (W395FZKTJ7)`.

What happens when they are absent:

- No `CSC_LINK`: the log says `CSC_LINK not set: producing UNSIGNED (ad-hoc signed) build`; the
  release still ships (unsigned, not notarized).
- Signed but any of the four `APPLE_*` missing: `Notarization skipped: missing
  secrets …`; the app is signed but not notarized, so Gatekeeper still warns.
- The certificate is imported into a temporary keychain with a random password
  and deleted, together with the `.p8` file, in an `always()` step. A failed
  notarization fails the job and prints the notary log.
- Hardened runtime with an empty entitlements file
  (`packaging/macos/entitlements.plist`). Add an entitlement only after a
  reproduced failure under the signed build.

## Windows signing (TODO)

There is no Windows code-signing certificate. The Windows build is unsigned and
the step logs `WINDOWS_CERT_PFX not set`. If that secret is set before signing
is implemented, the job fails on purpose rather than ship something that looks
signed. Implementing it means `signtool sign /fd sha256 /tr <timestamp URL> /td
sha256` on `crow-term.exe` before archiving, plus `signtool verify /pa`.

## Homebrew

Templates live in `packaging/homebrew/` (formula `crow-term.rb`, cask
`cask/crow-term.rb`). After the release is published:

```sh
python3 scripts/update_tap.py --version 0.1.0-alpha.2 \
  --tap "$(brew --repository andersonflima/tap)" \
  --sha256sums https://github.com/andersonflima/crow-term/releases/download/v0.1.0-alpha.2/SHA256SUMS \
  --notarized          # omit when the run summary says the macOS app is not notarized
```

`--dry-run` shows the diff only; `--assets-dir DIR` uses local archives instead.
The script writes `Formula/crow-term.rb` and `Casks/crow-term.rb`, never commits
or pushes, and prints the `git` commands to run after review. Without
`--notarized` the cask carries a caveat about Gatekeeper.

Install:

```sh
brew install andersonflima/tap/crow-term          # CLI + app in libexec (macOS/Linux)
brew install --cask andersonflima/tap/crow-term   # Crow Term.app + crow-term on PATH (macOS)
```

Both provide a `crow-term` command; install one of them, not both.

## npm

On a tag, after the GitHub Release, the `npm` job of `release.yml` builds six
packages from the release archives with `scripts/npm_package.py` (version from
Cargo.toml) and publishes them with npm Trusted Publishing (OIDC, no token):

| Package | Contents |
|---|---|
| `@andespindola/crow-term` | `bin/crow-term.js` launcher; every platform package as an exact-version `optionalDependency` |
| `@andespindola/crow-term-darwin-arm64`, `-darwin-x64` | the signed (and notarized, when the release is) `Crow Term.app` |
| `@andespindola/crow-term-linux-x64`, `-linux-arm64` | `crow-term` and `shell/` |
| `@andespindola/crow-term-win32-x64` | `crow-term.exe` and `shell/` |

npm installs only the platform package whose `os`/`cpu` match; there are no
install scripts and nothing is downloaded at install or run time. Platform
packages are published first, the launcher last. Prereleases also take the
`latest` dist-tag while the project is alpha-only, so `npm i -g
@andespindola/crow-term` installs them. A `workflow_dispatch` run builds, packs
and smoke-tests the linux-x64 layout (`crow-term --version` through the
launcher) without publishing.

One-time setup by the npm account owner, per package (six times): the package
must exist before a trusted publisher can be attached, so publish the first
version by hand from the dry-run `npm-packages` artifact (`npm publish
./<tgz> --access public --tag latest`; npm needs the explicit tag for a
prerelease) or with a short-lived token, then on npmjs.com → package → Settings
→ Trusted Publisher add GitHub Actions `andersonflima/crow-term`, workflow
`release.yml` (or `npm trust github`). Until that is done for a package, its
publish fails and the job stops before the launcher is published. The job skips
versions already on the registry, so re-running it after a partial failure, or
tagging the version that was hand-published as the bootstrap, is safe.

## Verify a release

```sh
codesign -dv --verbose=2 "/Applications/Crow Term.app"   # Authority=Developer ID…, TeamIdentifier, Runtime
codesign --verify --strict --deep --verbose=2 "/Applications/Crow Term.app"
spctl --assess --type execute -vv "/Applications/Crow Term.app"   # source=Notarized Developer ID
xcrun stapler validate "/Applications/Crow Term.app"
shasum -a 256 -c SHA256SUMS --ignore-missing
gh attestation verify crow-term-0.1.0-alpha.2-linux-x86_64.tar.gz --repo andersonflima/crow-term
```
