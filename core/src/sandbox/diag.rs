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

// `pub(super)`, not private: Linux's live seccomp tests
// (`sandbox::linux::tests`) call `platform::describe_denial` directly, to
// prove the whole diagnostic pipeline names a denial the envelope actually
// produced, not just the module that renders it.
#[cfg(target_os = "macos")]
#[path = "diag/macos.rs"]
pub(super) mod platform;
#[cfg(target_os = "linux")]
#[path = "diag/linux.rs"]
pub(super) mod platform;
#[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
pub(super) mod platform {
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
    pub(super) fn parse_denial(_: &str) -> Option<(String, super::Denied<'_>)> {
        None
    }
    pub(crate) fn describe_denial(_: &str) -> Option<String> {
        None
    }
    pub(super) fn door_reason(_: &str) -> Option<&'static str> {
        None
    }
}

/// What a denial's record names, and so which remedy the hint owes.  A service
/// never arrives as a path, so no wording can offer a door as something to
/// grant — the confusion this taxonomy exists to make unsayable.
#[cfg(not(windows))]
#[cfg_attr(
    not(target_os = "macos"),
    allow(
        dead_code,
        reason = "a seccomp record names no operand, so Linux constructs `Opaque` alone; the other classes are Seatbelt's, and the hint reading them is shared"
    )
)]
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Denied<'a> {
    /// A resolved path the grant's `fs.read` set can admit.  Metadata and
    /// ioctl denials ride here: read is the set that admits them.
    Read(&'a str),
    /// A resolved path the grant's `fs.write` set can admit.
    Write(&'a str),
    /// A binary outside the grant's `exec` allow-list — the re-execs
    /// (`sh -c`, `find -exec`) the in-process gate never sees.
    Exec(&'a str),
    /// A Mach or IPC name.  The base profile decides these, so no grant
    /// widens one, and most such denials are a library probing a service it
    /// can do without.
    Door(&'a str),
    /// A socket.  The grant's `net:` bit is the whole remedy, so the endpoint
    /// the record names is not worth repeating.
    Socket,
    /// A record with no operand a remedy could use — every Linux seccomp
    /// record, where [`platform::describe_denial`] speaks instead.
    Opaque,
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
    let mut out = String::from("the OS sandbox denied this command — the kernel reported:");
    for line in denials.iter().take(MAX_DENIAL_LINES) {
        out.push_str("\n  ");
        out.push_str(line.trim());
    }
    if denials.len() > MAX_DENIAL_LINES {
        let _ = write!(out, "\n  ({} more)", denials.len() - MAX_DENIAL_LINES);
    }
    for remedy in remedies(denials) {
        out.push_str("\n\n");
        out.push_str(&remedy);
    }
    out
}

/// One remedy per class of denial present, because the classes have different
/// answers and only some are the grant's: a path it can admit, a binary its
/// `exec` set can admit, a door it cannot open at all, a socket that is the
/// `net:` bit alone.  Leading with whichever line happened to carry a path is
/// what once offered a startup plist as the cure for a withheld keychain door.
#[cfg(not(windows))]
fn remedies(denials: &[&str]) -> Vec<String> {
    let (mut reads, mut writes, mut execs, mut doors) = (vec![], vec![], vec![], vec![]);
    let mut socket = false;
    let mut opaque = None;
    for (op, denied) in denials
        .iter()
        .filter_map(|line| platform::parse_denial(line))
    {
        match denied {
            Denied::Read(path) => note(&mut reads, path),
            Denied::Write(path) => note(&mut writes, path),
            Denied::Exec(path) => note(&mut execs, path),
            Denied::Door(name) => note(&mut doors, name),
            Denied::Socket => socket = true,
            Denied::Opaque => opaque = opaque.or(Some(op)),
        }
    }
    let mut out = Vec::new();
    // A door the profile can explain leads, because it is the one class the
    // reader cannot act on: left below a grantable path, it reads as an aside
    // while the path it cannot help gets granted in vain.
    let explained = doors
        .iter()
        .any(|door| platform::door_reason(door).is_some());
    if explained {
        out.push(door_remedy(&doors));
    }
    if !reads.is_empty() {
        out.push(path_remedy(&reads, "read"));
    }
    if !writes.is_empty() {
        out.push(path_remedy(&writes, "write"));
    }
    if !execs.is_empty() {
        out.push(exec_remedy(&execs));
    }
    if !doors.is_empty() && !explained {
        out.push(door_remedy(&doors));
    }
    if socket {
        out.push(NET_REMEDY.to_string());
    }
    if out.is_empty() {
        out.push(opaque_remedy(opaque.as_deref()));
    }
    out
}

/// Keep what is not already listed: one denied operand is logged once per
/// probe, and a hint that repeats it reads as several separate problems.
#[cfg(not(windows))]
fn note<'a>(seen: &mut Vec<&'a str>, item: &'a str) {
    if !seen.contains(&item) {
        seen.push(item);
    }
}

/// The class's operands under the same cap as the verbatim lines above them.
#[cfg(not(windows))]
fn listing(items: &[&str]) -> String {
    let mut out = String::new();
    for item in items.iter().take(MAX_DENIAL_LINES) {
        let _ = write!(out, "\n  {item}");
    }
    if items.len() > MAX_DENIAL_LINES {
        let _ = write!(out, "\n  ({} more)", items.len() - MAX_DENIAL_LINES);
    }
    out
}

/// `set` is the grant's own field, `read` or `write`: a denied write answered
/// by widening `read` is advice that fails a second time.
#[cfg(not(windows))]
fn path_remedy(paths: &[&str], set: &str) -> String {
    let one = paths.len() == 1;
    let mut out = format!(
        "{} outside the active grant's fs.{set}:",
        if one {
            "this path lies"
        } else {
            "these paths lie"
        }
    );
    out.push_str(&listing(paths));
    let _ = write!(
        out,
        "\nAdd {} (or a parent directory) to the grant's {set} set — the \
         `grant [ fs: [{set}: ['{}']] ] {{ … }}` block in ral, or `--extend-base` for \
         exarch. The sandbox matches fully-resolved paths, so a path reached through a \
         symlink inside a granted directory (e.g. ~/.config) needs the resolved path \
         above, not the link's own.",
        if one { "it" } else { "each" },
        paths[0]
    );
    out
}

/// Exec is its own set, and the kernel layer is the only one that sees a
/// re-exec — so this denial is never the fs grant's to answer.
#[cfg(not(windows))]
fn exec_remedy(paths: &[&str]) -> String {
    let mut out = String::from("the active grant's exec allow-list does not admit what ran here:");
    out.push_str(&listing(paths));
    let _ = write!(
        out,
        "\nAdmit it by name or directory — the `grant [ exec: ['{}': 'allow'] ] {{ … }}` \
         block in ral, or `--extend-base` for exarch. A re-exec (`sh -c`, `find -exec`) \
         reaches only this layer, so an in-process admit alone does not carry it.",
        paths[0]
    );
    out
}

/// Doors carrying a stated reason first: a refusal the profile can explain is
/// worth more than the startup probes that outnumber it.
#[cfg(not(windows))]
fn door_remedy(doors: &[&str]) -> String {
    let mut ranked: Vec<(&str, Option<&'static str>)> = doors
        .iter()
        .map(|door| (*door, platform::door_reason(door)))
        .collect();
    ranked.sort_by_key(|(_, why)| why.is_none());
    let mut out = String::from(
        "the base profile withheld a door no grant opens — a Mach or IPC name is the \
         profile's to decide, and the grant's fs, net and exec sets do not reach one:",
    );
    for (door, why) in ranked.iter().take(MAX_DENIAL_LINES) {
        match why {
            Some(why) => {
                let _ = write!(out, "\n  {door} — {why}");
            }
            None => {
                let _ = write!(out, "\n  {door}");
            }
        }
    }
    if ranked.len() > MAX_DENIAL_LINES {
        let _ = write!(out, "\n  ({} more)", ranked.len() - MAX_DENIAL_LINES);
    }
    if ranked.iter().any(|(_, why)| why.is_none()) {
        out.push_str(
            "\nA door with no reason given is usually harmless: a library probes a service \
             it can do without, and the denial is noise beside whatever actually failed.",
        );
    }
    out
}

#[cfg(not(windows))]
const NET_REMEDY: &str = "a socket was denied, so the active grant is `net: false` — and that bit is the whole of \
     it. No fs or exec widening opens a socket, and a hostname cannot carry bytes out as a \
     query label either: the resolver is closed at the same layer.";

/// The record names nothing to act on, so the hint says so rather than
/// guessing at an fs grant — unless a typed deny-set (Linux) has its own words
/// for the syscall.
#[cfg(not(windows))]
fn opaque_remedy(op: Option<&str>) -> String {
    if let Some(described) = op.and_then(platform::describe_denial) {
        return described;
    }
    let what = op.map_or_else(
        || "a sandboxed operation was denied".to_string(),
        |op| format!("the sandboxed operation `{op}` was denied"),
    );
    format!(
        "{what}, and the kernel record names no operand — so this hint cannot say what to \
         widen. If the command needs a path the grant does not admit, add it to the grant's \
         read or write set: the `grant [ fs: [read: […]] ] {{ … }}` block in ral, or \
         `--extend-base` for exarch."
    )
}

/// The live descendants of `root`, `root` itself excluded: one `/bin/ps` sample
/// of every `(pid, ppid)` pair, inverted and walked transitively.
///
/// Denial records carry only `(comm, pid)`, so a PID set is the only way to tell
/// our subprocess tree from a system service that ran in the same wall second.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:ps-sample] sandbox diagnostics: shells out to `/bin/ps` to sample the live process tree for denial attribution; a diagnostic probe, not turn-time model data I/O, raises no surface card."
)]
pub(crate) fn sample_descendants(root: u32) -> HashSet<u32> {
    let mut cmd = std::process::Command::new("/bin/ps");
    cmd.args(["-axo", "pid=,ppid="]);
    let Ok(out) = crate::process::output(&mut cmd) else {
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

    /// Each class present answers for itself: the path is offered to the fs
    /// set, the service named as a door, and neither is dressed as the other.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_answers_every_class_of_denial_it_was_given() {
        let shm = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                   ipc-posix-shm-read-data apple.shm.notification_center";
        let read = "2026-06-19 12:00:00.100 kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                    file-read-data /Users/me/dotfiles/git/.config/git/config";
        let hint = build_hint(&[shm, read]);
        assert!(
            hint.contains("fs.read:\n  /Users/me/dotfiles/git/.config/git/config"),
            "hint must offer the resolved filesystem path to grant; got {hint:?}"
        );
        assert!(
            hint.contains("no grant opens") && hint.contains("apple.shm.notification_center"),
            "hint must name the IPC operand as a door, not a path; got {hint:?}"
        );
        assert!(
            !hint.contains("fs.read:\n  apple.shm"),
            "hint must never list a service under a filesystem set; got {hint:?}"
        );
        assert!(
            hint.contains("ipc-posix-shm-read-data") && hint.contains("file-read-data"),
            "hint must reproduce every attributed denial line; got {hint:?}"
        );
    }

    /// A withheld door is answered by the profile's own reason, not by an fs
    /// widening no grant could perform — the securityd case that offered a
    /// startup plist as the cure.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_quotes_the_profile_reason_for_a_withheld_door() {
        let plist = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: cargo(1) deny(1) \
                     file-read-data /Users/me/Library/Preferences/.GlobalPreferences.plist";
        let door = "2026-06-19 12:00:00.100 kernel[0] (Sandbox) Sandbox: cargo(1) deny(1) \
                    mach-lookup com.apple.SecurityServer";
        let hint = build_hint(&[plist, door]);
        assert!(
            hint.contains("keychain") && hint.contains("git-fetch-with-cli"),
            "a withheld door must carry the profile's reason; got {hint:?}"
        );
        assert!(
            hint.contains("com.apple.SecurityServer —"),
            "the reason must be attached to the door that has one; got {hint:?}"
        );
    }

    /// A denied write is answered by the write set: `read` would fail again.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_offers_the_write_set_for_a_denied_write() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: tee(1) deny(1) \
                    file-write-create /Users/me/notes.txt";
        let hint = build_hint(&[line]);
        assert!(
            hint.contains("fs.write") && hint.contains("[write: ['/Users/me/notes.txt']]"),
            "a denied write must name the write set; got {hint:?}"
        );
        assert!(
            !hint.contains("fs.read"),
            "a denied write must not be blamed on the read set; got {hint:?}"
        );
    }

    /// `net: false` is the whole remedy for a socket, and no fs wording
    /// belongs anywhere near it.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_names_the_net_bit_for_a_denied_socket() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: curl(1) deny(1) \
                    network-outbound";
        let hint = build_hint(&[line]);
        assert!(
            hint.contains("`net: false`"),
            "a denied socket must name the net bit; got {hint:?}"
        );
        assert!(
            !hint.contains("read set"),
            "a denied socket must not be answered with an fs widening; got {hint:?}"
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
