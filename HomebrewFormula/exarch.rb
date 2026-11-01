class Exarch < Formula
  desc "Coding agent that drives the ral shell under a capability grant"
  homepage "https://github.com/lambdabetaeta/ral"
  url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.10.0.tar.gz"
  sha256 "f50e106ef29a8b0c020da07c6d00c871bfc0d1bf96e5a5933a427b5f80fa20de"
  license any_of: ["MIT", "Apache-2.0"]
  head do
    url "https://github.com/lambdabetaeta/ral.git", branch: "main"
    license all_of: ["Apache-2.0", "MIT"]
  end

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "exarch")
    if build.head?
      prefix.install "NOTICE", "LICENSE-APACHE", "LICENSE-MIT"
      prefix.install "vendor/edtui/LICENSE.md" => "LICENSE-EDTUI"
    end
  end

  test do
    assert_match "Exarch", shell_output("#{bin}/exarch --help")
  end
end
