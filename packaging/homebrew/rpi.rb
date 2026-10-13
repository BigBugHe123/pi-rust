# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.3.22. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.3.22"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.22/rpi-v0.3.22-aarch64-apple-darwin.tar.gz"
      sha256 "04653c58a87ecf61137a3d96a6f7135120b9183c451bc0d3e993b3fe661c2eef"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.22/rpi-v0.3.22-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "c54e9900d80de56e1abfa5b3912573ffc901a900fc1914e8671ef736350f7c60"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.22/rpi-v0.3.22-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "e58b41a0ad8194012a68ffe216e27fd4dbfa42997bdfda8ae6d6ad65664b9480"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
