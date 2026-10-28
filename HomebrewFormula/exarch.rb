class Exarch < Formula
  desc "Coding agent that drives the ral shell under a capability grant"
  homepage "https://github.com/lambdabetaeta/ral"
  url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.2.0.tar.gz"
  sha256 "b0928718a43f968a60e35ae82f91803fa526c72b0cae33380f7fcaa45fb5abdd"
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
