class Ral < Formula
  desc "Shell based on algebraic effects"
  homepage "https://github.com/lambdabetaeta/ral"
  license any_of: ["MIT", "Apache-2.0"]
  # HEAD-only: there is no tagged release tarball yet, so install with
  # `brew install --HEAD`.  When the first release is cut, add a `url`
  # pointing at the tag tarball and its `sha256` to enable a stable install.
  head "https://github.com/lambdabetaeta/ral.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "build", *std_cargo_args(root: buildpath), "--release",
           "--package", "ral", "--package", "ral-sh", "--package", "exarch"
    bin.install "target/release/ral"
    bin.install "target/release/ral-sh"
    bin.install "target/release/exarch"
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
    # HEAD builds report `ral <crate-version>+<commit>`, so match the
    # binary name rather than the formula's HEAD pseudo-version.
    assert_match(/^ral \d/, shell_output("#{bin}/ral --version"))
  end
end
