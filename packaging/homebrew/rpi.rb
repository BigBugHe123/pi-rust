# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.4.0. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.4.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.4.0/rpi-v0.4.0-aarch64-apple-darwin.tar.gz"
      sha256 "b46ef466108156a6d5dbdb63f00b6d269e5feac37722452334c5ef10de6e57be"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.4.0/rpi-v0.4.0-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "eba149d0c60056ee5a2f5619284b210729790d0e6befc41ec0a149662cf7af5b"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.4.0/rpi-v0.4.0-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "793c5041c80ff76fbb920b69596ef6022c2a745ec7fe7137e42909f16eab04c7"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
