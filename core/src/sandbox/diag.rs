//! Sandbox diagnostics: harvest kernel-reported sandbox denials and
//! turn them into an actionable hint on the failing command's [`Error`].
//!
//! Kernel sandbox enforcement (Seatbelt on macOS, the seccomp BPF
//! filter inside the bwrap envelope on Linux) reports denied syscalls
//! into a system log keyed by `(comm, pid)`, while the caller sees only
//! an opaque `EPERM` / non-zero exit.  When an external command run
//! under an active OS-sandbox grant fails, this module reads that log
//! over the call's wall window, keeps only lines whose PID was in the
//! call's descendant tree, and appends the denial — the kernel line,
//! the exact path to grant, and the symlink caveat — to the error's
//! `hint`.  Both the `ral-sh` REPL and `exarch` render that hint, so a
//! single augmentation surfaces in both hosts.
//!
//! Each platform's [`platform`] submodule supplies a reader for the
//! kernel-log window plus a small parser triple: recognise a denial
//! line, extract its attributed PID, and split it into `(operation,
//! path)`.  Platforms without a denial source (everything except macOS,
//! Linux, and Windows) get a stub `platform` module that reports no denials.
//! macOS logs the fully-resolved path of the denied access, so the
//! path the user must grant is exactly the path in the line; Linux's
//! `type=1326` audit record carries no path, so the hint there names the
//! blocked syscall operation with no path or symlink note.
//!
//! Windows is different in kind: an AppContainer denial surfaces only as
//! `ERROR_ACCESS_DENIED` on the confined child, with no kernel audit log
//! naming the path.  There is nothing to scrape, so the Windows hint is a
//! fixed, pathless note that names the grant surfaces — it never fabricates
//! a path it does not have.

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

/// Cap on how many denial lines the hint reproduces verbatim.  A failed
/// command typically trips one or two distinct denials; reproducing more
/// would bury the actionable guidance under kernel noise.
#[cfg(not(windows))]
const MAX_DENIAL_LINES: usize = 3;

/// Attach a kernel-denial diagnostic to `err` when an external command
/// failed under an active OS sandbox.
///
/// No-op — returns `err` unchanged — when the process did not run under
/// the OS sandbox (`shell.sandbox_projection().is_none()`), when `pids`
/// is empty, when the platform has no denial source, or when no denial
/// line in the window is attributable to a PID in `pids`.  Only the
/// failure arms of the command runners call this, and its gate
/// short-circuits before the expensive kernel-log read on every
/// non-sandboxed failure.
///
/// When a denial is found, the kernel line(s), the path to grant, and
/// the symlink caveat are appended to any existing `err.hint` (an
/// exit-code hint, say) below a blank line; an absent hint is set
/// outright.
pub(crate) fn augment_failure(
    mut err: Error,
    shell: &Shell,
    pids: &HashSet<u32>,
    since: Instant,
) -> Error {
    if shell.sandbox_projection().is_none() {
        return err;
    }
    let Some(diagnostic) = collect_denial_hint(pids, since) else {
        return err;
    };
    let hint = match err.hint.take() {
        Some(existing) => format!("{existing}\n\n{diagnostic}"),
        None => diagnostic,
    };
    err.with_hint(hint)
}

/// The platform's denial hint for a command that failed under an active
/// sandbox, or `None` when there is nothing to say.
#[cfg(not(windows))]
fn collect_denial_hint(pids: &HashSet<u32>, since: Instant) -> Option<String> {
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

/// Windows has no kernel denial log to scrape and no per-PID attribution,
/// so any failure under an active sandbox yields the same fixed, pathless
/// hint (never a fabricated path).
#[cfg(windows)]
fn collect_denial_hint(_pids: &HashSet<u32>, _since: Instant) -> Option<String> {
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

/// Compose the denial hint from the attributed kernel lines.
///
/// Pure: takes the already-filtered denial lines and returns the hint
/// text, so the parsing and wording are unit-testable without shelling
/// out to the kernel log.  No ANSI codes — the diagnostic formatter
/// colorises the `hint:` line as a whole.
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

    // Prefer a concrete path from the first parseable denial; macOS
    // logs the fully-resolved path, Linux carries none.
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

/// One sample of the descendant set: shell out to `/bin/ps` for every
/// live `(pid, ppid)` pair, then BFS from `root` through the inverted
/// parent map.  Returns descendants only — `root` itself is excluded.
///
/// Kernel denial records carry only `(comm, pid)`, so a PID set is the
/// only way to attribute a denial to "our" subprocess tree rather than
/// to a concurrent system service that happened to run during the same
/// wall second.
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

    /// The hint names the denied path verbatim and carries the symlink
    /// caveat and the grant guidance for both surfaces.
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

    /// A benign non-filesystem denial logged before the real
    /// `file-read*` denial must not hijack the "path to grant" slot.
    /// This is the regression from the field: a `git status` under the
    /// sandbox logged an `ipc-posix-shm-read-data` startup denial first,
    /// and the hint offered `apple.shm.notification_center` as a path —
    /// nonsense — instead of the resolved config path the read actually
    /// hit through a dotfiles symlink.
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
        // Both kernel lines stay reproduced verbatim for transparency.
        assert!(
            hint.contains("ipc-posix-shm-read-data") && hint.contains("file-read-data"),
            "hint must reproduce every attributed denial line; got {hint:?}"
        );
    }

    /// When the only denial in the window is a non-filesystem one, the
    /// hint degrades to the no-grantable-path wording rather than
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

    /// More than [`MAX_DENIAL_LINES`] denials reproduce the cap plus an
    /// "(N more)" tail rather than the whole stream.
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

    /// A descendant sample of our own process never reports our own PID
    /// and parses the `ps` tree without panicking.
    #[test]
    fn sample_descendants_excludes_root() {
        let me = std::process::id();
        let kids = sample_descendants(me);
        assert!(!kids.contains(&me), "root pid must be excluded");
    }
}
