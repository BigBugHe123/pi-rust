# Homebrew formula for rpi.
#
# Not in homebrew-core: that requires notability (stars/forks) rpi does not have
# yet. It works today from a tap — publish this file as `Formula/rpi.rb` in a
# repo named `homebrew-tap` under the same owner, then:
#
#   brew tap bigfish1913/tap
#   brew install rpi
#
# Hashes below are for v0.3.23. See packaging/README.md for the refresh procedure.
class Rpi < Formula
  desc "Rust-native, library-first coding-agent runtime and terminal CLI"
  homepage "https://rpi.laofu.online/"
  version "0.3.23"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.23/rpi-v0.3.23-aarch64-apple-darwin.tar.gz"
      sha256 "e64cbd4fe5bfde6450b87fee96f8018dc7eada786cce2a6e66408860cf826523"
    end
    # Intel macOS prebuilt binaries are installed via scripts/install.sh.
    # This formula currently covers Apple silicon on macOS.
  end

  on_linux do
    on_arm do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.23/rpi-v0.3.23-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "4c10967d91c46d2bb8d3de81f5d37288e3a1099a66e1dafec8c24a00be4c6e57"
    end
    on_intel do
      url "https://github.com/bigfish1913/pi-rust/releases/download/v0.3.23/rpi-v0.3.23-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "44c4137cadf9c2e3fcd5f3affbc590029574d648b00f07e299d4324834296575"
    end
  end

  def install
    bin.install "rpi"
  end

  test do
    assert_match "rpi", shell_output("#{bin}/rpi --version")
  end
end
