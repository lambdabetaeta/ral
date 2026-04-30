//! Host environment snapshot — gathered once at startup and stitched
//! into the system prompt so the model knows what kind of machine it
//! is on, where it stands on disk, and when "now" is.  Each probe is
//! best-effort: missing values silently drop their line rather than
//! erroring, so the prompt stays well-formed on bare or exotic hosts.

use std::process::Command;

/// Multi-line markdown list — `os`, `now`, `cwd`, `user`, `home`, and
/// (when cwd is inside a repo) `git`.  Stable for the process
/// lifetime, so safe inside the cached system prefix.
pub fn snapshot() -> String {
    let mut out = String::new();
    out.push_str(&format!("- os: {}\n", os_line()));
    if let Some(d) = date_line() {
        out.push_str(&format!("- now: {d}\n"));
    }
    if let Ok(cwd) = std::env::current_dir() {
        out.push_str(&format!("- cwd: {}\n", cwd.display()));
    }
    if let Ok(u) = std::env::var("USER") {
        out.push_str(&format!("- user: {u}\n"));
    }
    if let Ok(h) = std::env::var("HOME") {
        out.push_str(&format!("- home: {h}\n"));
    }
    if let Some(g) = git_line() {
        out.push_str(&format!("- git: {g}\n"));
    }
    out
}

/// `Mac OS 14.6.1 [64-bit] (arm64)` or `Ubuntu 24.04 [64-bit] (jammy,
/// x86_64)` — `os_info`'s `Display` plus the codename and architecture
/// fields it omits.
fn os_line() -> String {
    let info = os_info::get();
    let mut extras = Vec::new();
    if let Some(c) = info.codename() {
        extras.push(c.to_string());
    }
    if let Some(a) = info.architecture() {
        extras.push(a.to_string());
    }
    if extras.is_empty() {
        info.to_string()
    } else {
        format!("{info} ({})", extras.join(", "))
    }
}

/// Local date, time, and timezone via `date(1)` — shelling out is
/// cheaper than dragging in `chrono`/`time` for a single string.
fn date_line() -> Option<String> {
    let out = Command::new("date").arg("+%Y-%m-%d %H:%M:%S %Z").output().ok()?;
    out.status.success().then(|| String::from_utf8(out.stdout).ok())
        .flatten()
        .map(|s| s.trim().to_string())
}

/// `branch (clean)` or `branch (dirty)` when cwd is inside a git
/// working tree; `None` otherwise.  Two cheap subprocesses; `--porcelain`
/// short-circuits on the first untracked or modified path.
fn git_line() -> Option<String> {
    let head = Command::new("git").args(["rev-parse", "--abbrev-ref", "HEAD"]).output().ok()?;
    if !head.status.success() {
        return None;
    }
    let branch = String::from_utf8(head.stdout).ok()?.trim().to_string();
    let porcelain = Command::new("git").args(["status", "--porcelain"]).output().ok()?;
    let state = if porcelain.stdout.is_empty() { "clean" } else { "dirty" };
    Some(format!("{branch} ({state})"))
}
