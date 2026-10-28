# Template rendered by scripts/update_tap.py into <tap>/Formula/crow-term.rb.
# Do not edit the rendered file by hand; tokens like @VERSION@ are replaced.
class CrowTerm < Formula
  desc "Native terminal with persistent sessions, built in Rust"
  homepage "https://github.com/andersonflima/crow-term"
  version "@VERSION@"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/andersonflima/crow-term/releases/download/v#{version}/crow-term-#{version}-macos-arm64.tar.gz"
      sha256 "@SHA256_MACOS_ARM64@"
    end
    on_intel do
      url "https://github.com/andersonflima/crow-term/releases/download/v#{version}/crow-term-#{version}-macos-x86_64.tar.gz"
      sha256 "@SHA256_MACOS_X86_64@"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/andersonflima/crow-term/releases/download/v#{version}/crow-term-#{version}-linux-arm64.tar.gz"
      sha256 "@SHA256_LINUX_ARM64@"
    end
    on_intel do
      url "https://github.com/andersonflima/crow-term/releases/download/v#{version}/crow-term-#{version}-linux-x86_64.tar.gz"
      sha256 "@SHA256_LINUX_X86_64@"
    end
  end

  def install
    # Keep the archive layout intact: the binary (on macOS, a link into the signed
    # Crow Term.app) finds shell/setup.py next to itself or in the app's Resources.
    libexec.install Dir["*"]
    bin.write_exec_script libexec/"crow-term"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/crow-term --version")
  end
end
