class Ral < Formula
  desc "Shell based on algebraic effects"
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

    system "cargo", "install", *std_cargo_args(path: "ral")
    bin.install_symlink bin/"ral" => "ral-sh"
    if build.head?
      prefix.install "NOTICE", "LICENSE-APACHE", "LICENSE-MIT"
      system "just", "third-party", "ral", Utils.safe_popen_read("rustc", "--print", "host-tuple").chomp,
             "THIRDPARTY-RAL"
      prefix.install "THIRDPARTY-RAL"
    end
  end

  def caveats
    <<~EOS
      To use ral-sh as your login shell, register it and change your shell:

        sudo sh -c 'echo #{opt_bin}/ral-sh >> /etc/shells'
        chsh -s #{opt_bin}/ral-sh

      ral-sh is ral invoked by that name: it forwards POSIX command strings
      and scripts to /bin/sh so that POSIX-assuming tools (scp, rsync,
      git-over-ssh) are unaffected, and runs ral for interactive sessions.

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
