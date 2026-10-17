# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.4.1. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.4.1"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.4.1/rpi-v0.4.1-aarch64-apple-darwin.tar.gz"
      sha256 "e7972f5f963607152468ff6593c343ae129204c9368a377dff1f86b6f99cf1e4"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.4.1/rpi-v0.4.1-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "8e0a6941c00afeb9eeff7b316cbade26e9c9ecdf27979bf769264aaef5f7c328"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.4.1/rpi-v0.4.1-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "253aa132946dc3631027421a5b6b1892b77db31d23e21e04ae43b334478b2fc1"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
