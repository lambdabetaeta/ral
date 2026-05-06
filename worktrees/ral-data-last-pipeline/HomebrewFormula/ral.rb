class Ral < Formula
  desc "Shell based on algebraic effects"
  homepage "https://github.com/lambdabetaeta/ral"
  # Update url and sha256 when cutting a release:
  #   url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.1.0.tar.gz"
  #   sha256 "<sha256 of tarball>"
  url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.1.0.tar.gz"
  sha256 "0000000000000000000000000000000000000000000000000000000000000000"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/lambdabetaeta/ral.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "build", *std_cargo_args(root: buildpath), "--release",
           "--package", "ral", "--package", "ral-sh"
    bin.install "target/release/ral"
    bin.install "target/release/ral-sh"
  end

  def caveats
    <<~EOS
      To use ral-sh as your login shell, register it and change your shell:

        sudo sh -c 'echo #{opt_bin}/ral-sh >> /etc/shells'
        chsh -s #{opt_bin}/ral-sh

      ral-sh forwards non-interactive invocations to /bin/sh so that
      POSIX-assuming tools (scp, rsync, git-over-ssh) are unaffected.
      ral itself is launched for interactive sessions.

      To try ral without changing your login shell, add to ~/.zshrc or ~/.bashrc:

        [[ $- == *i* ]] && exec ral
    EOS
  end

  test do
    assert_equal "hello\n", shell_output("#{bin}/ral -c 'echo hello'")
    assert_equal "2\n", shell_output("#{bin}/ral-sh -c 'echo $((1+1))'")
    assert_match version.to_s, shell_output("#{bin}/ral --version")
  end
end
