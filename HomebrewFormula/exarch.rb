class Exarch < Formula
  desc "Coding agent that drives the ral shell under a capability grant"
  homepage "https://github.com/lambdabetaeta/ral"
  url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "8d35eaf2d31e85efbbc9fc1a8bdd50fabbc5b269780d30811ca140ad203432ca"
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
