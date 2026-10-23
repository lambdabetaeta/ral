//! Format [`ral_core::host`]'s probes into the markdown snapshot opening
//! exarch's `Host` section — host truths.
//!
//! That is why synod, whose agent lives in a guest VM where none hold,
//! composes its own section instead.

use crate::site::{Silent, site};

/// Markdown list: `os`, `now`, `cwd`, `user`, `home`, `git`, `exarch logs`.
///
/// An empty probe drops its line rather than erroring, so bare and exotic
/// hosts still yield a well-formed prompt; baked once at boot with the rest
/// of the prompt, so `now` names boot time.  A host with no state directory
/// has no `exarch logs` to name.
pub fn snapshot(exarch_state: Option<&std::path::Path>) -> String {
    use std::fmt::Write;
    let mut out = String::new();
    let _ = writeln!(out, "- os: {}", os_line());
    if let Some(d) = now() {
        let _ = writeln!(out, "- now: {d}");
    }
    if let Some(cwd) = ral_core::host::cwd() {
        let _ = writeln!(out, "- cwd: {}", ral_core::path::shown(&cwd));
    }
    if let Some(user) = site!(Silent::HostSection, ral_core::host::user()) {
        let _ = writeln!(out, "- user: {user}");
    }
    if let Some(home) = site!(Silent::HostSection, ral_core::host::home()) {
        let _ = writeln!(out, "- home: {}", ral_core::path::shown(home));
    }
    if let Some(g) = git_line() {
        let _ = writeln!(out, "- git: {g}");
    }
    if let Some(state) = exarch_state {
        let _ = writeln!(out, "- exarch logs: {}", ral_core::path::shown(state));
    }
    out
}

/// `Ubuntu 24.04 [64-bit] (jammy, x86_64)` — `os_info`'s `Display`, which
/// knows a version std's target constants cannot, plus the fields it omits.
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

/// Local date, time and timezone; `None` if the format cannot render.
pub fn now() -> Option<String> {
    jiff::fmt::strtime::format("%Y-%m-%d %H:%M:%S %Z", &jiff::Zoned::now()).ok()
}

/// `branch (clean)` or `branch (dirty)`; `None` outside a git working tree.
fn git_line() -> Option<String> {
    let git = |args: &[&str]| {
        let mut cmd = site!(Silent::GitLaunch, std::process::Command::new("git"));
        cmd.args(args);
        let out = ral_core::process::output(&mut cmd).ok()?;
        out.status.success().then_some(out.stdout)
    };
    let branch = String::from_utf8(git(&["rev-parse", "--abbrev-ref", "HEAD"])?).ok()?;
    let dirty = !git(&["status", "--porcelain"])?.is_empty();
    Some(format!(
        "{} ({})",
        branch.trim(),
        if dirty { "dirty" } else { "clean" }
    ))
}
