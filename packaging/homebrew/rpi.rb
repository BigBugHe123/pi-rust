# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.5.0. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.5.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.5.0/rpi-v0.5.0-aarch64-apple-darwin.tar.gz"
      sha256 "93e925ac17501007077a0ba22bce0c3b334b4340033167a7b8cb4aa0f13af5c1"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.5.0/rpi-v0.5.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "89035f3152afaddfde234228a7bbaacc893b0d05a53f80847075dee729d29583"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.5.0/rpi-v0.5.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "6c7283fa3242c123ee26848f3f54e4c0e7b63cbbe6946e91cb50981bafda528a"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
