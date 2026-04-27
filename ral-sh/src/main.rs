//! POSIX-bridge login shell dispatcher for `ral`.
//!
//! `ral-sh` is a thin binary intended to be registered as a login shell.
//! It inspects its invocation context and either execs `ral` (interactive
//! sessions) or forwards to `/bin/sh` (everything else), so that
//! POSIX-assuming tools — `scp`, `rsync`, `git-over-ssh`, `ansible` — are
//! unaffected by `ral`'s non-POSIX syntax.
//!
//! **Dispatch rules:**
//! - Interactive (stdin *and* stdout are both ttys) with no arguments → exec `ral`.
//! - Any other invocation (non-interactive, `-c`, script path, …) → forward to `/bin/sh`.
//!
//! The login-shell convention (argv\[0\] prefixed with `-`) is preserved for
//! whichever binary is exec'd, so both `ral` and `/bin/sh` source their
//! respective login profiles.
//!
//! **Registration:**
//! ```sh
//! sudo sh -c 'echo /usr/local/bin/ral-sh >> /etc/shells'
//! chsh -s /usr/local/bin/ral-sh
//! ```

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

/// Determine whether to exec `ral` or fall through to `/bin/sh`.
///
/// Reads `argv[0]` to establish the login-shell flag, then checks
/// whether the session is fully interactive (both stdin and stdout are
/// ttys with no extra arguments). Interactive sessions are handed off to
/// `ral`; everything else is delegated to [`exec_posix_sh`].
fn dispatch() -> ! {
    let argv0 = std::env::args().next().unwrap_or_default();
    let is_login = std::path::Path::new(&argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .is_some_and(|n| n.starts_with('-'));

    let args: Vec<String> = std::env::args().skip(1).collect();

    // Interactive with no arguments: exec ral.
    #[cfg(unix)]
    if args.is_empty() {
        use std::io::IsTerminal;
        let stdin_tty = std::io::stdin().is_terminal();
        let stdout_tty = std::io::stdout().is_terminal();
        if stdin_tty && stdout_tty {
            exec_ral(is_login);
        }
    }

    // Everything else: forward to /bin/sh.
    exec_posix_sh(is_login, &args)
}

#[cfg(unix)]
fn exec_ral(is_login: bool) -> ! {
    // Find the ral binary in the same directory as ral-sh.
    let ral = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("ral")))
        .unwrap_or_else(|| std::path::PathBuf::from("ral"));

    let mut cmd = std::process::Command::new(&ral);
    if is_login {
        cmd.arg0("-ral");
    }
    let err = cmd.exec();
    eprintln!("ral-sh: exec {}: {err}", ral.display());
    std::process::exit(127)
}

fn exec_posix_sh(is_login: bool, args: &[String]) -> ! {
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
    }
    #[cfg(not(unix))]
    {
        let status = cmd.status().unwrap_or_else(|e| {
            eprintln!("ral-sh: exec /bin/sh: {e}");
            std::process::exit(127)
        });
        std::process::exit(status.code().unwrap_or(1));
    }
    std::process::exit(127)
}
