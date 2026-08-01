//! What machine ral is running on: OS, architecture and family, current
//! directory, user and home, git working-tree state, wall-clock time.
//!
//! [`crate::boot`] boots a `Shell` *in* a host process; this only reports
//! on one.  The live probes ([`git`], [`now`]) yield `None` rather than
//! erroring, so a caller stitching them into a prompt or an `$ENV`
//! baseline stays well-formed on a bare or exotic host.

use std::process::Command;

/// Grant a console child its console but withhold the window: synod is a
/// windowless desktop binary (`windows_subsystem = "windows"`), and Windows
/// answers a console child in such a parent by opening a real console that
/// blinks for the child's lifetime.  Piped output is unaffected either way;
/// off Windows there is nothing to suppress.
#[cfg_attr(not(windows), allow(unused_mut))]
fn without_a_console_window(mut cmd: Command) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// The checked-out branch, and whether the tree carries uncommitted changes.
pub struct GitStatus {
    pub branch: String,
    pub dirty: bool,
}

/// The working-tree state when the current directory is inside a
/// repository; `None` on any failure, "not a repository" included.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:git-launch] shells out to git(1) to probe the working tree; best-effort host info, not turn-time data I/O"
)]
pub fn git() -> Option<GitStatus> {
    let head = without_a_console_window(Command::new("git"))
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !head.status.success() {
        return None;
    }
    let branch = String::from_utf8(head.stdout).ok()?.trim().to_string();
    let porcelain = without_a_console_window(Command::new("git"))
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !porcelain.status.success() {
        return None;
    }
    Some(GitStatus {
        branch,
        dirty: !porcelain.stdout.is_empty(),
    })
}

/// The host OS — `"macos"`, `"linux"`, `"windows"`, …
pub fn os_name() -> &'static str {
    std::env::consts::OS
}

/// The host architecture — `"aarch64"`, `"x86_64"`, …
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// The host OS family — `"unix"` or `"windows"`.
pub fn family() -> &'static str {
    std::env::consts::FAMILY
}

/// Local date, time, and timezone via `date(1)`; `None` on any failure.
/// One formatted string does not earn a dependency on `chrono` or `time`.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:date-launch] shells out to date(1) for the host info line; not turn-time data I/O"
)]
pub fn now() -> Option<String> {
    let out = without_a_console_window(Command::new("date"))
        .arg("+%Y-%m-%d %H:%M:%S %Z")
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8(out.stdout).ok())
        .flatten()
        .map(|s| s.trim().to_string())
}

// Process cwd lives in `path`, which enforces the resolution discipline; it
// is re-exported so host facts have one address.
pub use crate::path::process_cwd as cwd;

/// `$HOME` (then `$USERPROFILE` on Windows) read from the host process env
/// alone; empty when nothing is set.
///
/// This is a host fact — where the tool itself is installed and who launched
/// it — never language semantics: a `within [env: …]` overlay must not apply.
/// Code holding a shell env threads its overlay through [`crate::path::home`]
/// instead.  The clippy denylist bans this function so every call site is a
/// written decision, carrying an `#[allow]` whose reason says why the read is
/// host-level.
pub fn home() -> String {
    crate::path::home(&crate::types::EnvVars::new())
}

/// [`home`], falling back to `.`, so callers joining onto home never build a
/// path off an empty base.  Same discipline as [`home`].
pub fn home_or_dot() -> String {
    let h = crate::path::home(&crate::types::EnvVars::new());
    if h.is_empty() { ".".into() } else { h }
}

/// `$USER` (then `$USERNAME` on Windows) from the host process env alone.
///
/// `"?"` when nothing is set, matching the prompt/audit placeholder.  Same
/// discipline as [`home`] — overlay-holding code goes through
/// [`crate::path::user_name`].
pub fn user() -> String {
    crate::path::user_name(&crate::types::EnvVars::new())
}
