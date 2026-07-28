class Ral < Formula
  desc "Shell based on algebraic effects"
  homepage "https://github.com/lambdabetaeta/ral"
  url "https://github.com/lambdabetaeta/ral/archive/refs/tags/v0.2.0.tar.gz"
  sha256 "b0928718a43f968a60e35ae82f91803fa526c72b0cae33380f7fcaa45fb5abdd"
  license any_of: ["MIT", "Apache-2.0"]
  head "https://github.com/lambdabetaeta/ral.git", branch: "main"

  depends_on "rust" => :build

  def install
    system "cargo", "install", *std_cargo_args(path: "ral")
    system "cargo", "install", *std_cargo_args(path: "ral-sh")
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
