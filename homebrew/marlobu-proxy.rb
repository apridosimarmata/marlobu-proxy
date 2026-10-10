class MarlobuProxy < Formula
  desc "Postgres wire protocol proxy for session-based database isolation"
  homepage "https://github.com/apridosimarmata/marlobu-proxy"
  version "0.1.0"
  license "MIT"

  on_macos do
    on_arm do
      url "https://github.com/apridosimarmata/marlobu-proxy/releases/download/v#{version}/marlobu-proxy-darwin-arm64.tar.gz"
      sha256 "PLACEHOLDER_ARM64_SHA256"
    end
    on_intel do
      url "https://github.com/apridosimarmata/marlobu-proxy/releases/download/v#{version}/marlobu-proxy-darwin-x86_64.tar.gz"
      sha256 "PLACEHOLDER_X86_64_SHA256"
    end
  end

  on_linux do
    url "https://github.com/apridosimarmata/marlobu-proxy/releases/download/v#{version}/marlobu-proxy-linux-x86_64.tar.gz"
    sha256 "PLACEHOLDER_LINUX_SHA256"
  end

  def install
    bin.install "marlobu-proxy"
  end

  test do
    assert_match "marlobu-proxy", shell_output("#{bin}/marlobu-proxy --version 2>&1", 1)
  end
end
