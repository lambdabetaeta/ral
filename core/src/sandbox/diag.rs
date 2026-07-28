//! Turns a kernel-reported sandbox denial into an actionable hint on the
//! failing command's [`Error`], which `crate::diagnostic` renders for whichever
//! host is driving.
//!
//! Seatbelt and the seccomp filter inside the bwrap envelope report denials into
//! a system log keyed by `(comm, pid)`, while the caller sees only an opaque
//! `EPERM` or nonzero exit.  So [`augment_failure`] reads that log over the
//! call's wall window and keeps the lines whose PID lay in the call's descendant
//! tree.  `diag/macos.rs` and `diag/linux.rs` each supply the reader plus a
//! parser triple; everywhere else a stub `platform` reports no denials.
//!
//! Windows differs in kind: an `AppContainer` denial carries no audit record at
//! all, so there the hint is gated on the exit code and never names a path.

use crate::types::{Error, Shell};
use std::collections::{HashMap, HashSet};
#[cfg(not(windows))]
use std::fmt::Write;
use std::time::Instant;

#[cfg(target_os = "macos")]
#[path = "diag/macos.rs"]
mod platform;
#[cfg(target_os = "linux")]
#[path = "diag/linux.rs"]
mod platform;
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
mod platform {
    use std::time::Duration;
    pub(super) fn read_window(_: Duration) -> Option<String> {
        None
    }
    pub(super) fn is_denial_line(_: &str) -> bool {
        false
    }
    pub(super) fn extract_pid(_: &str) -> Option<u32> {
        None
    }
    pub(super) fn parse_denial(_: &str) -> Option<(&str, Option<&str>)> {
        None
    }
}

/// Denial lines the hint reproduces verbatim before it starts counting; more
/// would bury the guidance under kernel noise.
#[cfg(not(windows))]
const MAX_DENIAL_LINES: usize = 3;

/// Append a kernel-denial diagnostic to `err`, below any hint it already
/// carries, when an external command failed under an active OS sandbox.
///
/// Called only from the failure arms of the command runners, and returns `err`
/// untouched unless a denial in the window is attributable to a PID in `pids` —
/// the sandbox gate comes first so an ordinary failure never pays for the
/// kernel-log read.
pub(crate) fn augment_failure(
    mut err: Error,
    shell: &Shell,
    pids: &HashSet<u32>,
    since: Instant,
) -> Error {
    if shell.sandbox_projection().is_none() {
        return err;
    }
    let Some(diagnostic) = collect_denial_hint(pids, since, err.exit_code()) else {
        return err;
    };
    let hint = match err.hint.take() {
        Some(existing) => format!("{existing}\n\n{diagnostic}"),
        None => diagnostic,
    };
    err.with_hint(hint)
}

/// The platform's denial hint, or `None` when there is nothing to say.
#[cfg(not(windows))]
fn collect_denial_hint(pids: &HashSet<u32>, since: Instant, _exit_code: i32) -> Option<String> {
    if pids.is_empty() {
        return None;
    }
    let text = platform::read_window(since.elapsed())?;
    let denials: Vec<&str> = text
        .lines()
        .filter(|l| platform::is_denial_line(l))
        .filter(|l| platform::extract_pid(l).is_some_and(|p| pids.contains(&p)))
        .collect();
    if denials.is_empty() {
        return None;
    }
    Some(build_hint(&denials))
}

/// Exit codes an `AppContainer` denial plausibly produces: the raw Win32
/// `ERROR_ACCESS_DENIED`, which a child that does not translate its own Win32
/// failures surfaces verbatim, and the NTSTATUS `STATUS_ACCESS_DENIED`, which
/// the loader exits with when a denied read kills the process before its own
/// code runs.  The gate stands in for the denial line Unix gets: without it, a
/// `diff` reporting a difference would have the sandbox blamed.
#[cfg(windows)]
fn plausible_access_denied_exit(code: i32) -> bool {
    const ERROR_ACCESS_DENIED: i32 = 5;
    const STATUS_ACCESS_DENIED: i32 = 0xC000_0022_u32.cast_signed();
    code == ERROR_ACCESS_DENIED || code == STATUS_ACCESS_DENIED
}

/// With no denial log and no per-PID attribution, the exit code alone decides,
/// and the hint is fixed text: there is no path here to name.
#[cfg(windows)]
fn collect_denial_hint(_pids: &HashSet<u32>, _since: Instant, exit_code: i32) -> Option<String> {
    if !plausible_access_denied_exit(exit_code) {
        return None;
    }
    Some(
        "the command failed while an OS sandbox (AppContainer) was active. On Windows a blocked \
         filesystem access surfaces only as an access-denied error on the command, with no kernel \
         log naming the path, so the exact path cannot be shown here. If the command needs a file \
         or directory the active grant's fs allow-list does not admit, widen the grant's read (or \
         write) set — the `grant [ fs: [read: […]] ] { … }` block in ral, or `--extend-base` for \
         exarch. A `net: false` grant likewise withholds the network capability, so a command that \
         needs a socket will fail the same way."
            .to_string(),
    )
}

/// Compose the hint from the already-attributed kernel lines — pure, so the
/// wording is testable without a kernel log.  No ANSI: `crate::diagnostic`
/// colours the `hint:` line as a whole.
#[cfg(not(windows))]
fn build_hint(denials: &[&str]) -> String {
    let mut out = String::from(
        "the OS sandbox blocked a filesystem access this command needs — the kernel reported:",
    );
    for line in denials.iter().take(MAX_DENIAL_LINES) {
        out.push_str("\n  ");
        out.push_str(line.trim());
    }
    if denials.len() > MAX_DENIAL_LINES {
        let _ = write!(out, "\n  ({} more)", denials.len() - MAX_DENIAL_LINES);
    }

    // Not the first denial but the first that yields a path: a benign `ipc-*`
    // or `mach-*` startup denial often comes first, and its operand is a
    // service name that `parse_denial` rightly withholds.
    let path = denials
        .iter()
        .find_map(|l| platform::parse_denial(l).and_then(|(_, p)| p));
    if let Some(path) = path {
        let _ = write!(
            out,
            "\n\nthe path `{path}` is outside the active grant's fs.read, so the access was \
             denied. To allow it, add this path (or a parent directory) to the grant's read \
             set: the `grant [ fs: [read: ['{path}']] ] {{ … }}` block in ral, or \
             `--extend-base` for exarch."
        );
        out.push_str(
            "\n\nNote: the sandbox matches fully-resolved paths. If this path was reached \
             through a symlink inside a granted directory (e.g. ~/.config), granting the \
             link's directory will not help — grant the resolved path shown above.",
        );
    } else {
        let op = denials
            .iter()
            .find_map(|l| platform::parse_denial(l).map(|(op, _)| op));
        let what = op.map_or_else(
            || "A sandboxed operation was denied".to_string(),
            |op| format!("The sandboxed operation `{op}` was denied"),
        );
        let _ = write!(
            out,
            "\n\n{what} (the kernel record carries no path here). The access lies outside the \
             active grant; widen the grant's fs read set — the `grant [ fs: [read: […]] ] \
             {{ … }}` block in ral, or `--extend-base` for exarch."
        );
    }
    out
}

/// The live descendants of `root`, `root` itself excluded: one `/bin/ps` sample
/// of every `(pid, ppid)` pair, inverted and walked transitively.
///
/// Denial records carry only `(comm, pid)`, so a PID set is the only way to tell
/// our subprocess tree from a system service that ran in the same wall second.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:ps-sample] sandbox diagnostics: shells out to `/bin/ps` to sample the live process tree for denial attribution; a diagnostic probe, not turn-time model data I/O, raises no surface card."
)]
pub(crate) fn sample_descendants(root: u32) -> HashSet<u32> {
    let Ok(out) = std::process::Command::new("/bin/ps")
        .args(["-axo", "pid=,ppid="])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return HashSet::new();
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut children_of: HashMap<u32, Vec<u32>> = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(pid_s) = parts.next() else {
            continue;
        };
        let Some(ppid_s) = parts.next() else {
            continue;
        };
        let Ok(pid) = pid_s.parse::<u32>() else {
            continue;
        };
        let Ok(ppid) = ppid_s.parse::<u32>() else {
            continue;
        };
        children_of.entry(ppid).or_default().push(pid);
    }
    let mut seen = HashSet::new();
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        if let Some(children) = children_of.get(&p) {
            for &c in children {
                if seen.insert(c) {
                    frontier.push(c);
                }
            }
        }
    }
    seen
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hint names the denied path verbatim, the symlink caveat, and both
    /// grant surfaces.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_names_path_and_symlink_caveat() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: cat(69436) deny(1) \
                    file-read-data /private/var/folders/ab/secret/data.txt";
        let hint = build_hint(&[line]);
        assert!(
            hint.contains("/private/var/folders/ab/secret/data.txt"),
            "hint must name the denied path; got {hint:?}"
        );
        assert!(
            hint.contains("symlink") && hint.contains("resolved"),
            "hint must carry the symlink caveat; got {hint:?}"
        );
        assert!(
            hint.contains("fs.read") && hint.contains("--extend-base"),
            "hint must name both grant surfaces; got {hint:?}"
        );
        assert!(hint.contains("deny(1)"), "hint must show the kernel line");
    }

    /// A benign non-filesystem denial logged first must not hijack the
    /// path-to-grant slot with its service name.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_selects_filesystem_path_over_ipc_operand() {
        let shm = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                   ipc-posix-shm-read-data apple.shm.notification_center";
        let read = "2026-06-19 12:00:00.100 kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                    file-read-data /Users/me/dotfiles/git/.config/git/config";
        let hint = build_hint(&[shm, read]);
        assert!(
            hint.contains("the path `/Users/me/dotfiles/git/.config/git/config`"),
            "hint must offer the resolved filesystem path to grant; got {hint:?}"
        );
        assert!(
            !hint.contains("the path `apple.shm.notification_center`"),
            "hint must not offer the IPC service operand as a path to grant; got {hint:?}"
        );
        assert!(
            hint.contains("ipc-posix-shm-read-data") && hint.contains("file-read-data"),
            "hint must reproduce every attributed denial line; got {hint:?}"
        );
    }

    /// A lone non-filesystem denial degrades to the pathless wording rather than
    /// inventing a path from a service name.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_without_filesystem_denial_offers_no_path() {
        let shm = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                   ipc-posix-shm-read-data apple.shm.notification_center";
        let hint = build_hint(&[shm]);
        assert!(
            hint.contains("carries no path here"),
            "a lone non-fs denial must use the no-path arm; got {hint:?}"
        );
        assert!(
            !hint.contains("apple.shm.notification_center`"),
            "the service operand must never appear as a grantable path; got {hint:?}"
        );
    }

    /// Denials past the cap collapse into an `(N more)` tail.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_caps_reproduced_lines() {
        let mk = |n: u32| {
            format!(
                "kernel[0] (Sandbox) Sandbox: cat({n}) deny(1) file-read-data /private/tmp/f{n}.txt"
            )
        };
        let lines: Vec<String> = (0..5).map(mk).collect();
        let refs: Vec<&str> = lines.iter().map(String::as_str).collect();
        let hint = build_hint(&refs);
        assert!(
            hint.contains("(2 more)"),
            "five denials over a cap of three must note two more; got {hint:?}"
        );
    }

    #[test]
    fn sample_descendants_excludes_root() {
        let me = std::process::id();
        let kids = sample_descendants(me);
        assert!(!kids.contains(&me), "root pid must be excluded");
    }

    #[cfg(windows)]
    #[test]
    fn plausible_access_denied_exit_recognises_both_codes_and_rejects_ordinary_exits() {
        assert!(plausible_access_denied_exit(5), "ERROR_ACCESS_DENIED");
        assert!(
            plausible_access_denied_exit(0xC000_0022_u32.cast_signed()),
            "STATUS_ACCESS_DENIED"
        );
        assert!(!plausible_access_denied_exit(0));
        assert!(!plausible_access_denied_exit(1));
        assert!(!plausible_access_denied_exit(2));
    }
}
