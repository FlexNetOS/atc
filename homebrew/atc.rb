# Reference template only — do NOT use this file directly.
# The authoritative, checksum-filled formula is maintained at
# https://github.com/FlexNetOS/homebrew-tap/blob/main/Formula/atc.rb
# and is updated automatically by .github/workflows/release.yml on each v* tag push.
class Atc < Formula
  desc "Air Traffic Control — agent orchestrator for AI coding agents"
  homepage "https://github.com/FlexNetOS/atc"
  version "VERSION_PLACEHOLDER"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/FlexNetOS/atc/releases/download/vVERSION_PLACEHOLDER/atc-darwin-arm64.tar.gz"
      sha256 "DARWIN_ARM64_PLACEHOLDER"
    end
    on_intel do
      url "https://github.com/FlexNetOS/atc/releases/download/vVERSION_PLACEHOLDER/atc-darwin-x64.tar.gz"
      sha256 "DARWIN_X64_PLACEHOLDER"
    end
  end

  on_linux do
    on_arm do
      url "https://github.com/FlexNetOS/atc/releases/download/vVERSION_PLACEHOLDER/atc-linux-arm64.tar.gz"
      sha256 "LINUX_ARM64_PLACEHOLDER"
    end
    on_intel do
      url "https://github.com/FlexNetOS/atc/releases/download/vVERSION_PLACEHOLDER/atc-linux-x64.tar.gz"
      sha256 "LINUX_X64_PLACEHOLDER"
    end
  end

  def install
    bin.install "atc"
  end

  test do
    system bin/"atc", "--help"
  end
end
