class Exarch < Formula
  desc "Coding agent that drives the ral shell under a capability grant"
  homepage "https://github.com/lambdabetaeta/ral"
  url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.2.0.tar.gz"
  sha256 "b0928718a43f968a60e35ae82f91803fa526c72b0cae33380f7fcaa45fb5abdd"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/lambdabetaeta/ral.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "exarch")
  end

  test do
    assert_match "Exarch", shell_output("#{bin}/exarch --help")
  end
end
