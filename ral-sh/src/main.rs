//! POSIX-bridge login shell dispatcher for `ral`.
//!
//! `ral-sh` is a thin binary intended to be registered as a login shell.
//! It inspects its invocation context and either execs `ral` (interactive
//! sessions) or forwards to `/bin/sh` (everything else), so that
//! POSIX-assuming tools — `scp`, `rsync`, `git-over-ssh`, `ansible` — are
//! unaffected by `ral`'s non-POSIX syntax.
//!
//! **Dispatch rules** (first match wins):
//! - `-c` present → `/bin/sh`.  A `-c` invocation is a POSIX tool running
//!   a command string; `ral`'s syntax is not POSIX, so it must not see it.
//!   This wins even alongside `-l` (`$SHELL -lc 'scp …'`).
//! - `-l` or `-i` present → `ral`.  Editors and multiplexers launch a
//!   fully interactive session as `$SHELL -l` (VS Code `"args": ["-l"]`)
//!   or `$SHELL -i` (tmux `default-command`); those belong in `ral`.
//! - No arguments and both stdin and stdout are ttys → `ral`.  A bare
//!   interactive login.
//! - Everything else (non-interactive, a script path, …) → `/bin/sh`.
//!
//! The login-shell convention (argv\[0\] prefixed with `-`) is preserved for
//! whichever binary is exec'd, so both `ral` and `/bin/sh` source their
//! respective login profiles.  argv is read with `args_os` throughout: a
//! login dispatcher must forward non-UTF-8 arguments faithfully, not panic.
//!
//! **Registration:**
//! ```sh
//! sudo sh -c 'echo /usr/local/bin/ral-sh >> /etc/shells'
//! chsh -s /usr/local/bin/ral-sh
//! ```

use std::ffi::{OsStr, OsString};
#[cfg(unix)]
use std::os::unix::process::CommandExt;

fn main() {
    // Refuse to execute under a setuid environment: an elevated euid with a
    // different uid is a signal that the binary has been installed setuid,
    // which is never intentional and would be a security hazard.
    #[cfg(unix)]
    unsafe {
        if libc::geteuid() != libc::getuid() {
            eprintln!("ral-sh: refusing to run setuid");
            std::process::exit(1);
        }
    }

    dispatch();
}

/// Which binary an invocation routes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Target {
    Ral,
    PosixSh,
}

/// Decide the dispatch target from the invocation context (see the
/// module-level rules).  Pure: takes the parsed args and tty state so the
/// matrix is unit-testable without a real terminal.
fn decide(args: &[OsString], stdin_tty: bool, stdout_tty: bool) -> Target {
    let mut has_c = false;
    let mut has_interactive = false;
    for arg in args {
        if let Some(cluster) = short_flag_cluster(arg) {
            has_c |= cluster.contains('c');
            has_interactive |= cluster.contains('l') || cluster.contains('i');
        } else if arg == "--login" {
            has_interactive = true;
        }
    }

    // `-c` is a POSIX command string and always wins.  Otherwise ral takes
    // an explicit interactive request (`-l`/`-i`) or a bare both-ttys login.
    let bare_interactive = args.is_empty() && stdin_tty && stdout_tty;
    if !has_c && (has_interactive || bare_interactive) {
        Target::Ral
    } else {
        Target::PosixSh
    }
}

/// The letters of a single-dash short-flag cluster (`-l`, `-lc`, `-i`),
/// or `None` for `--long` options, bare `-`, or non-UTF-8 / non-letter
/// tokens.  Only ASCII-letter clusters are flags worth classifying here;
/// anything else is opaque and routes by the fall-through rule.
fn short_flag_cluster(arg: &OsStr) -> Option<&str> {
    let s = arg.to_str()?;
    let rest = s.strip_prefix('-')?;
    if rest.is_empty() || rest.starts_with('-') || !rest.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(rest)
}

/// Read the invocation context and exec the chosen binary.
fn dispatch() -> ! {
    let argv0 = std::env::args_os().next().unwrap_or_default();
    // ral-sh has no dependency on ral-core (it's the minimal
    // outer dispatcher binary, intentionally standalone), so it
    // can't reach for `ral_core::path::basename` like ral/main.rs
    // does.  Same basename-from-argv0 idiom, inlined.
    #[allow(clippy::disallowed_methods)]
    let is_login = std::path::Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.starts_with('-'));

    let args: Vec<OsString> = std::env::args_os().skip(1).collect();

    #[cfg(unix)]
    let (stdin_tty, stdout_tty) = {
        use std::io::IsTerminal;
        (
            std::io::stdin().is_terminal(),
            std::io::stdout().is_terminal(),
        )
    };
    // Without tty introspection (non-Unix) a bare invocation cannot be
    // classified as interactive; only explicit `-l`/`-i` reaches `ral`.
    #[cfg(not(unix))]
    let (stdin_tty, stdout_tty) = (false, false);

    match decide(&args, stdin_tty, stdout_tty) {
        Target::Ral => exec_ral(is_login, &args),
        Target::PosixSh => exec_posix_sh(is_login, &args),
    }
}

#[cfg(unix)]
fn exec_ral(is_login: bool, args: &[OsString]) -> ! {
    // Find the ral binary in the same directory as ral-sh.
    let ral = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("ral")))
        .unwrap_or_else(|| std::path::PathBuf::from("ral"));

    let mut cmd = std::process::Command::new(&ral);
    if is_login {
        cmd.arg0("-ral");
    }
    cmd.args(args);
    let err = cmd.exec();
    eprintln!("ral-sh: exec {}: {err}", ral.display());
    std::process::exit(127)
}

#[cfg(not(unix))]
fn exec_ral(_is_login: bool, _args: &[OsString]) -> ! {
    // Non-Unix has no `exec`; ral-sh is a Unix login-shell bridge.
    eprintln!("ral-sh: ral dispatch is only supported on Unix");
    std::process::exit(127)
}

fn exec_posix_sh(is_login: bool, args: &[OsString]) -> ! {
    let mut cmd = std::process::Command::new("/bin/sh");
    #[cfg(unix)]
    if is_login {
        cmd.arg0("-sh");
    }
    #[cfg(not(unix))]
    let _ = is_login;
    cmd.args(args);
    #[cfg(unix)]
    {
        let err = cmd.exec();
        eprintln!("ral-sh: exec /bin/sh: {err}");
        std::process::exit(127)
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().unwrap_or_else(|e| {
            eprintln!("ral-sh: exec /bin/sh: {e}");
            std::process::exit(127)
        });
        std::process::exit(status.code().unwrap_or(1))
    }
}

#[cfg(test)]
mod tests {
    use super::{Target, decide};
    use std::ffi::OsString;

    fn args(items: &[&str]) -> Vec<OsString> {
        items.iter().map(OsString::from).collect()
    }

    /// The (tty × args) dispatch matrix.  The login bit (dash-argv0) does
    /// not affect *which* binary runs — only how it is exec'd — so it is
    /// not a `decide` input; the third axis is covered by the dash-argv0
    /// handling in `dispatch`, exercised at the integration level.
    #[test]
    fn dispatch_matrix() {
        // Bare interactive: both ttys, no args → ral.
        assert_eq!(decide(&args(&[]), true, true), Target::Ral);
        // Bare, not both ttys → /bin/sh (a pipe / redirect, POSIX territory).
        assert_eq!(decide(&args(&[]), false, true), Target::PosixSh);
        assert_eq!(decide(&args(&[]), true, false), Target::PosixSh);

        // Interactive-session flags route to ral regardless of tty: VS Code
        // (`-l`) and tmux (`-i`) launch through a PTY but want ral.
        assert_eq!(decide(&args(&["-l"]), true, true), Target::Ral);
        assert_eq!(decide(&args(&["-l"]), false, false), Target::Ral);
        assert_eq!(decide(&args(&["-i"]), true, true), Target::Ral);
        assert_eq!(decide(&args(&["--login"]), false, false), Target::Ral);

        // `-c` is a POSIX command string: always /bin/sh, even with `-l`.
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

        // A script path or any other positional → /bin/sh.
        assert_eq!(decide(&args(&["script.sh"]), true, true), Target::PosixSh);
        assert_eq!(
            decide(&args(&["-x", "script.sh"]), true, true),
            Target::PosixSh
        );
    }

    /// `--long` options other than `--login`, bare `-`, and non-letter
    /// tokens are opaque and route by the fall-through rule, not as short
    /// flags.
    #[test]
    fn non_short_flags_are_opaque() {
        // `-` is the conventional "read stdin" positional, not a flag.
        assert_eq!(decide(&args(&["-"]), true, true), Target::PosixSh);
        // A made-up long option is not interactive and not `-c`.
        assert_eq!(decide(&args(&["--posix"]), false, false), Target::PosixSh);
        // `-il` bundles `-i` and `-l`: interactive.
        assert_eq!(decide(&args(&["-il"]), false, false), Target::Ral);
    }
}
