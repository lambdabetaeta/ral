class Exarch < Formula
  desc "Coding agent that drives the ral shell under a capability grant"
  homepage "https://github.com/lambdabetaeta/ral"
  url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.10.0.tar.gz"
  sha256 "f50e106ef29a8b0c020da07c6d00c871bfc0d1bf96e5a5933a427b5f80fa20de"
  license any_of: ["MIT", "Apache-2.0"]
  head do
    url "https://github.com/lambdabetaeta/ral.git", branch: "main"
    license all_of: ["Apache-2.0", "MIT"]

    depends_on "cargo-about" => :build
    depends_on "just" => :build
  end

  depends_on "rust" => :build

  def install
    # Releases generate the notices with cargo-about 0.9.3; so must this build.
    if build.head?
      about = Formula["cargo-about"].version.to_s
      odie "cargo-about #{about} is installed, but THIRDPARTY notices need 0.9.3" if about != "0.9.3"
    end

    system "cargo", "install", *std_cargo_args(path: "exarch")
    if build.head?
      prefix.install "NOTICE", "LICENSE-APACHE", "LICENSE-MIT"
      prefix.install "vendor/edtui/LICENSE.md" => "LICENSE-EDTUI"
      system "just", "third-party", "exarch", Utils.safe_popen_read("rustc", "--print", "host-tuple").chomp,
             "THIRDPARTY-EXARCH"
      prefix.install "THIRDPARTY-EXARCH"
    end
  end

  test do
    assert_match "Exarch", shell_output("#{bin}/exarch --help")
  end
end
