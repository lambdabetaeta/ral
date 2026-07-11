//! Probing the underlying host *machine*.
//!
//! The one place to ask what kind of machine ral is running on: the OS,
//! the architecture and family, the current directory, the user and
//! their home, the git working-tree state, and the wall-clock time.
//! Distinct from [`crate::driver`], which embeds and *drives* a `Shell`
//! in a host *process* — this module reports facts about the host, it
//! does not run anything in it.
//!
//! The compile-time constants ([`os_name`], [`arch`], [`family`]) are
//! free and infallible.  The probes that ask the live system —
//! [`git`], [`now`] — are best-effort: any failure yields `None` rather
//! than erroring, so a caller stitching them into a prompt or an `$env`
//! baseline stays well-formed on bare or exotic hosts.

use std::process::Command;

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
    let head = Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .output()
        .ok()?;
    if !head.status.success() {
        return None;
    }
    let branch = String::from_utf8(head.stdout).ok()?.trim().to_string();
    let porcelain = Command::new("git")
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
    let out = Command::new("date")
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
