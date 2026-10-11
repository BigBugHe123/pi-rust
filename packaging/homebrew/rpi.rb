# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.3.21. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.3.21"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.21/rpi-v0.3.21-aarch64-apple-darwin.tar.gz"
      sha256 "8a6a96bbf99265acb7a34ad560ed3de3a76916755bfcbb7264a9a3bae4910690"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.21/rpi-v0.3.21-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "6ad98e3908a3fd952034d49d98b973300914b82e4867df30dcd08041940516a5"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.21/rpi-v0.3.21-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "9ceeb5fe434fbeca9c27ef9f4b622ebb9652cc5563d488664529a8d501367afd"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
