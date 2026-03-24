# Reference template only — do NOT use this file directly.
# The authoritative, checksum-filled formula is maintained at
# https://github.com/harmony-labs/homebrew-tap/blob/main/Formula/atc.rb
# and is updated automatically by .github/workflows/release.yml on each v* tag push.
class Atc < Formula
  desc "Air Traffic Control — agent orchestrator for AI coding agents"
  homepage "https://github.com/harmony-labs/atc"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/harmony-labs/atc/releases/download/v#{version}/atc-darwin-arm64.tar.gz"
      sha256 "PLACEHOLDER"
    end
    on_intel do
      url "https://github.com/harmony-labs/atc/releases/download/v#{version}/atc-darwin-x64.tar.gz"
      sha256 "PLACEHOLDER"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/harmony-labs/atc/releases/download/v#{version}/atc-linux-arm64.tar.gz"
      sha256 "PLACEHOLDER"
    end
    on_intel do
      url "https://github.com/harmony-labs/atc/releases/download/v#{version}/atc-linux-x64.tar.gz"
      sha256 "PLACEHOLDER"
    end
  end

  def install
    bin.install "atc"
  end

  test do
    system bin/"atc", "--help"
  end
end
