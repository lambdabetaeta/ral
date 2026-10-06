//! The POSIX bridge: `ral` invoked by the name `ral-sh`.
//!
//! A login shell must serve `$SHELL -c <POSIX string>` (sshd, `sudo -s`, vim
//! `:!`, `ProxyCommand`) as well as interactive sessions, and nothing in argv
//! tells the two apart from `ral -c <ral source>`. The one bit that survives
//! `/etc/shells` and `exec` is argv\[0\], so `ral-sh` is a symlink to `ral`
//! and this module, the only reader of that name, routes by it.
//!
//! **Dispatch** (first match wins), over the arguments after argv\[0\]:
//! 1. a single-dash letter cluster containing `c`, or any argument that is
//!    neither such a cluster nor `--login` (a positional, `--`, `-`, an opaque
//!    `--long`) → `/bin/sh`;
//! 2. a cluster containing `l` or `i`, or `--login` → ral;
//! 3. no arguments, stdin and stdout both terminals → ral;
//! 4. otherwise → `/bin/sh`.
//!
//! So `-lc …`, `-l script.sh`, `-s` and `script` reach `/bin/sh`, while `-l`,
//! `-i`, `-il`, `--login` and a bare tty login reach ral.
//!
//! **Security.** `refuse_setuid` runs first. The bridge resolves nothing
//! through the environment: `/bin/sh` is a literal path, with no `$PATH`
//! lookup, no `current_exe` and no sibling search, and the ral branch does not
//! exec at all, since the process simply continues as `ral`. argv is forwarded
//! as `OsString`, unvalidated, before `text_args` could refuse it; the
//! environment is forwarded untouched. A leading dash on argv\[0\] becomes
//! `-sh`, so `/bin/sh` sources its login profile; ral sees the dash itself.
//! argv\[0\] is caller-controlled, and that gains nothing: the caller could run
//! `/bin/sh` or ral directly.
//!
//! **Registration:**
//! ```sh
//! sudo sh -c 'echo /usr/local/bin/ral-sh >> /etc/shells'
//! chsh -s /usr/local/bin/ral-sh
//! ```

use ral_core::errln;
use std::ffi::{OsStr, OsString};
use std::io::IsTerminal as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, ExitCode};

const NAME: &str = "ral-sh";

#[derive(Debug, PartialEq, Eq)]
enum Target {
    Ral,
    PosixSh,
}

/// `Some(code)` only when this process was the bridge and its exec of
/// `/bin/sh` failed; `None` means carry on as `ral`, whether because the
/// process was never the bridge or because the bridge chose ral.
pub(crate) fn serve() -> Option<ExitCode> {
    let mut argv = std::env::args_os();
    let argv0 = argv.next()?;
    let name = ral_core::path::basename(argv0.to_str()?);
    let (login, name) = match name.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, name),
    };
    if name != NAME {
        return None;
    }
    let args: Vec<OsString> = argv.collect();
    match decide(
        &args,
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    ) {
        Target::Ral => None,
        Target::PosixSh => Some(exec_posix_sh(login, &args)),
    }
}

/// Pure so the (tty x args) matrix is unit-testable without a terminal.
fn decide(args: &[OsString], stdin_tty: bool, stdout_tty: bool) -> Target {
    let mut interactive = false;
    for arg in args {
        match short_flag_cluster(arg) {
            Some(cluster) if cluster.contains('c') => return Target::PosixSh,
            Some(cluster) => interactive |= cluster.contains(['l', 'i']),
            None if arg == "--login" => interactive = true,
            None => return Target::PosixSh,
        }
    }
    if interactive || (args.is_empty() && stdin_tty && stdout_tty) {
        Target::Ral
    } else {
        Target::PosixSh
    }
}

/// The letters of a single-dash cluster (`-l`, `-lc`), or `None` for `--long`
/// options, bare `-`, and anything non-ASCII or non-letter.
fn short_flag_cluster(arg: &OsStr) -> Option<&str> {
    let rest = arg.to_str()?.strip_prefix('-')?;
    (!rest.is_empty() && rest.chars().all(|c| c.is_ascii_alphabetic())).then_some(rest)
}

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:respawn-posix-sh] the login-shell bridge hands a POSIX command string to /bin/sh; infra plumbing, not a model exec image"
)]
fn exec_posix_sh(login: bool, args: &[OsString]) -> ExitCode {
    let mut cmd = Command::new("/bin/sh");
    if login {
        cmd.arg0("-sh");
    }
    let err = cmd.args(args).exec();
    errln!("ral: cannot replace this process with /bin/sh: {err}");
    ExitCode::from(127)
}

#[cfg(test)]
mod tests {
    use super::{Target, decide};
    use std::ffi::OsString;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    #[test]
    fn dispatch_matrix() {
        assert_eq!(decide(&args(&[]), true, true), Target::Ral);
        assert_eq!(decide(&args(&[]), false, true), Target::PosixSh);
        assert_eq!(decide(&args(&[]), true, false), Target::PosixSh);

        assert_eq!(decide(&args(&["-l"]), true, true), Target::Ral);
        assert_eq!(decide(&args(&["-l"]), false, false), Target::Ral);
        assert_eq!(decide(&args(&["-i"]), true, true), Target::Ral);
        assert_eq!(decide(&args(&["-il"]), false, false), Target::Ral);
        assert_eq!(decide(&args(&["--login"]), false, false), Target::Ral);

        assert_eq!(
            decide(&args(&["-c", "echo hi"]), true, true),
            Target::PosixSh
        );
        assert_eq!(
            decide(&args(&["-lc", "scp x y"]), true, true),
            Target::PosixSh
        );
        assert_eq!(
            decide(&args(&["-c", "echo hi"]), false, false),
            Target::PosixSh
        );

        assert_eq!(decide(&args(&["script.sh"]), true, true), Target::PosixSh);
        assert_eq!(
            decide(&args(&["-x", "script.sh"]), true, true),
            Target::PosixSh
        );
    }

    #[test]
    fn a_positional_after_a_login_flag_is_a_script_for_sh() {
        assert_eq!(decide(&args(&["-l", "foo.sh"]), true, true), Target::PosixSh);
        assert_eq!(
            decide(&args(&["--login", "foo.sh"]), true, true),
            Target::PosixSh
        );
        assert_eq!(decide(&args(&["-s"]), true, true), Target::PosixSh);
        assert_eq!(decide(&args(&["script"]), true, true), Target::PosixSh);
    }

    #[test]
    fn non_short_flags_are_opaque() {
        assert_eq!(decide(&args(&["-"]), true, true), Target::PosixSh);
        assert_eq!(decide(&args(&["--"]), true, true), Target::PosixSh);
        assert_eq!(decide(&args(&["--posix"]), false, false), Target::PosixSh);
        assert_eq!(
            decide(&args(&["-l", "--posix"]), false, false),
            Target::PosixSh
        );
    }
}
