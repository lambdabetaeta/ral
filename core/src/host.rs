//! Probing the underlying host *machine*.
//!
//! The one place to ask what kind of machine ral is running on: the OS,
//! the architecture and family, the current directory, the user and
//! their home, the git working-tree state, and the wall-clock time.
//! Distinct from [`crate::boot`], which embeds and *boots* a `Shell`
//! in a host *process* — this module reports facts about the host, it
//! does not run anything in it.
//!
//! The compile-time constants ([`os_name`], [`arch`], [`family`]) are
//! free and infallible.  The probes that ask the live system —
//! [`git`], [`now`] — are best-effort: any failure yields `None` rather
//! than erroring, so a caller stitching them into a prompt or an `$env`
//! baseline stays well-formed on bare or exotic hosts.

use std::process::Command;

/// Ask Windows not to give this child a console window.
///
/// Both probes below run a console program, and either may be called from
/// a process that has no console of its own: synod is a desktop
/// application (`windows_subsystem = "windows"` in `synod/src/main.rs`).
/// Windows answers a console child in a windowless parent by allocating a
/// console *with* a window, which blinks open and shut for as long as the
/// child lives — one black rectangle across the screen at the start of
/// every session, from a probe whose whole purpose is a line of prompt
/// text.  `CREATE_NO_WINDOW` grants the console and withholds the window.
///
/// Nothing else changes: both callers read the child through
/// [`Command::output`], and a redirected pipe is unaffected by whether a
/// console window exists.  Off Windows there is nothing to suppress — a
/// child inherits its parent's terminal and no window is created for it —
/// so this hands the command straight back.
#[cfg_attr(not(windows), allow(unused_mut))]
fn without_a_console_window(mut cmd: Command) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(windows_sys::Win32::System::Threading::CREATE_NO_WINDOW);
    }
    cmd
}

/// The git working-tree state of the current directory: the checked-out
/// branch and whether the tree carries uncommitted changes.
pub struct GitStatus {
    pub branch: String,
    pub dirty: bool,
}

/// The git working-tree state when the current directory is inside a
/// repository; `None` otherwise.
///
/// Two cheap subprocesses:
/// `git rev-parse --abbrev-ref HEAD` for the branch and
/// `git status --porcelain` for the dirty bit (non-empty output ⇒
/// dirty).  Best-effort — any failure yields `None`.
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

/// The host OS — `"macos"`, `"linux"`, `"windows"`, … — the compile-time
/// target constant, free and infallible.
pub fn os_name() -> &'static str {
    std::env::consts::OS
}

/// The host architecture — `"aarch64"`, `"x86_64"`, … — the compile-time
/// target constant, free and infallible.
pub fn arch() -> &'static str {
    std::env::consts::ARCH
}

/// The host OS family — `"unix"`, `"windows"` — the compile-time target
/// constant, free and infallible.
pub fn family() -> &'static str {
    std::env::consts::FAMILY
}

/// Local date, time, and timezone via `date(1)` (`%Y-%m-%d %H:%M:%S %Z`);
/// `None` on any failure.  Shelling out is cheaper than dragging in
/// `chrono`/`time` for a single string.
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

// The path-anchored probes keep living in `path`, where their resolution
// discipline is enforced; re-exposing them here makes this module the
// single discoverable place for host facts.
pub use crate::path::process_cwd as cwd;
pub use crate::path::{home_from_env as home, user_name_from_env as user};
