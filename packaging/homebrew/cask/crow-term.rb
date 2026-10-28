# Template rendered by scripts/update_tap.py into <tap>/Casks/crow-term.rb.
# Do not edit the rendered file by hand; tokens like @VERSION@ are replaced.
cask "crow-term" do
  arch arm: "arm64", intel: "x86_64"

  version "@VERSION@"
  sha256 arm:   "@SHA256_MACOS_ARM64_APP@",
         intel: "@SHA256_MACOS_X86_64_APP@"

  url "https://github.com/andersonflima/crow-term/releases/download/v#{version}/crow-term-#{version}-macos-#{arch}.app.zip"
  name "Crow Term"
  desc "Native terminal with persistent sessions, built in Rust"
  homepage "https://github.com/andersonflima/crow-term"

  depends_on :macos

  app "Crow Term.app"
  binary "#{appdir}/Crow Term.app/Contents/MacOS/crow-term"

  zap trash: [
    "~/Library/Application Support/dev.crow.crow-term",
    "~/Library/Caches/dev.crow.crow-term",
  ]
  @CASK_CAVEATS@
end
