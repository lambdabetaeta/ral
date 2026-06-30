//! Host environment snapshot — gathered once at startup and stitched
//! into the system prompt so the model knows what kind of machine it
//! is on, where it stands on disk, and when "now" is.  Each probe is
//! best-effort: missing values silently drop their line rather than
//! erroring, so the prompt stays well-formed on bare or exotic hosts.
//!
//! The probes themselves live in [`ral_core::host`]; this module only
//! formats them into the markdown snapshot, except for [`os_line`],
//! which uses `os_info` for a richer version/codename string than the
//! std target constants can provide.

/// Multi-line markdown list — `os`, `now`, `cwd`, `user`, `home`,
/// `exarch logs`, and (when cwd is inside a repo) `git`.  Stable for
/// the process lifetime, so safe inside the cached system prefix.
pub fn snapshot(exarch_state: &std::path::Path) -> String {
    let mut out = String::new();
    out.push_str(&format!("- os: {}\n", os_line()));
    if let Some(d) = ral_core::host::now() {
        out.push_str(&format!("- now: {d}\n"));
    }
    if let Some(cwd) = ral_core::host::cwd() {
        out.push_str(&format!("- cwd: {}\n", cwd.display()));
    }
    let user = ral_core::host::user();
    if user != "?" {
        out.push_str(&format!("- user: {user}\n"));
    }
    let home = ral_core::host::home();
    if !home.is_empty() {
        out.push_str(&format!("- home: {home}\n"));
    }
    if let Some(g) = git_line() {
        out.push_str(&format!("- git: {g}\n"));
    }
    out.push_str(&format!("- exarch logs: {}\n", exarch_state.display()));
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

/// `branch (clean)` or `branch (dirty)` when cwd is inside a git
/// working tree; `None` otherwise.  Formats [`ral_core::host::git`]'s
/// structured probe.
fn git_line() -> Option<String> {
    ral_core::host::git()
        .map(|g| format!("{} ({})", g.branch, if g.dirty { "dirty" } else { "clean" }))
}
