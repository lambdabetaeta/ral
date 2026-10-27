//! Turns a kernel-reported sandbox denial into an actionable hint for the
//! failing command, which `crate::diagnostic` renders for whichever host is
//! driving.
//!
//! Seatbelt and the seccomp filter inside the bwrap envelope report denials into
//! a system log keyed by `(comm, pid)`, while the caller sees only an opaque
//! `EPERM` or nonzero exit.  So [`denial_hint`] reads that log over the
//! call's wall window and keeps records whose PID lay in the call's descendant
//! tree. Each platform supplies a reader and a record parser.
//!
//! Windows differs in kind: an `AppContainer` denial carries no audit record at
//! all, so there the hint is gated on the exit code and never names a path.

#[cfg(not(windows))]
use crate::syntax::quote::quote_word;
use std::collections::HashSet;
#[cfg(not(windows))]
use std::fmt::Write;
use std::time::Instant;

#[cfg(not(windows))]
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Inert(String);

#[cfg(not(windows))]
impl Inert {
    #[cfg_attr(
        not(any(test, target_os = "macos")),
        allow(dead_code, reason = "Linux audit records carry no operands")
    )]
    pub(crate) fn of(text: &str) -> Self {
        let mut out = String::new();
        let mut chars = text.chars();
        for c in chars.by_ref().take(512) {
            match c {
                '\n' => out.push_str("\\n"),
                '\r' => out.push_str("\\r"),
                '\t' => out.push_str("\\t"),
                c if c.is_control() => {
                    let _ = write!(out, "\\u{{{:x}}}", u32::from(c));
                }
                c => out.push(c),
            }
        }
        if chars.next().is_some() {
            out.push_str(" [truncated]");
        }
        Self(out)
    }
}

#[cfg(not(windows))]
impl std::fmt::Display for Inert {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(not(windows))]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Record {
    pid: u32,
    op: platform::Operation,
    denied: Denied,
}

// Linux's live seccomp test also exercises the record parser.
#[cfg(target_os = "macos")]
#[path = "diag/macos.rs"]
pub(super) mod platform;
#[cfg(target_os = "linux")]
#[path = "diag/linux.rs"]
pub(super) mod platform;

/// What a denial's record names, and so which remedy the hint owes.  A service
/// never arrives as a path, so no wording can offer one as something to
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
pub(super) enum Denied {
    /// A resolved path the grant's `fs.read` set can admit.  Metadata and
    /// ioctl denials ride here: read is the set that admits them.
    Read(Inert),
    /// A resolved path the grant's `fs.write` set can admit.
    Write(Inert),
    /// A binary outside the grant's `exec` allow-list — the re-execs
    /// (`sh -c`, `find -exec`) the in-process guard never sees.
    Exec(Inert),
    /// A Mach or IPC name.  The base profile decides these, so no grant
    /// widens one, and most such denials are a library probing a service it
    /// can do without.
    Service(Inert),
    /// A socket.  The grant's `net:` bit is the whole remedy, so the endpoint
    /// the record names is not worth repeating.
    Socket,
    /// The GPU: Metal's device, an `IOSurface` or its compiler.  `gpu:` is the
    /// whole remedy, as `net:` is for a socket.
    Gpu,
    /// A Unix socket a stated fs write can admit; unconfined below Linux ABI 9.
    UnixSocket(Inert),
    /// A record with no operand a remedy could use — every Linux seccomp
    /// record, where [`platform::describe_denial`] speaks instead.
    Opaque,
}

#[cfg(not(windows))]
impl Denied {
    fn operand(&self) -> Option<&Inert> {
        match self {
            Self::Read(o)
            | Self::Write(o)
            | Self::Exec(o)
            | Self::Service(o)
            | Self::UnixSocket(o) => Some(o),
            Self::Socket | Self::Gpu | Self::Opaque => None,
        }
    }
}

/// Records shown before the hint starts counting.
#[cfg(not(windows))]
const MAX_DENIAL_LINES: usize = 3;

#[cfg(not(windows))]
enum Collector {
    #[cfg(target_os = "macos")]
    Log(std::time::Duration),
    #[cfg(target_os = "linux")]
    Journal(std::time::Duration),
    #[cfg(target_os = "linux")]
    Dmesg(std::time::Duration),
}

#[cfg(not(windows))]
impl Collector {
    #[cfg_attr(
        target_os = "macos",
        allow(
            clippy::unnecessary_wraps,
            reason = "Linux can lack both fixed system paths"
        )
    )]
    fn command(self) -> Option<std::process::Command> {
        use crate::site::{Silent, site};

        #[cfg(target_os = "linux")]
        fn system_path(names: [&'static str; 2]) -> Option<&'static str> {
            for name in names {
                let path = site!(Silent::JournalRead, std::path::Path::new(name));
                if site!(Silent::JournalRead, path => try_exists()).ok()? {
                    return Some(name);
                }
            }
            None
        }

        match self {
            #[cfg(target_os = "macos")]
            Self::Log(elapsed) => {
                let mut cmd = site!(Silent::LogShow, std::process::Command::new("/usr/bin/log"));
                cmd.args([
                    "show",
                    "--predicate",
                    "eventMessage BEGINSWITH \"Sandbox: \"",
                    "--style",
                    "ndjson",
                    "--last",
                    &format!("{}s", elapsed.as_secs().saturating_add(1)),
                ]);
                Some(cmd)
            }
            #[cfg(target_os = "linux")]
            Self::Journal(elapsed) => {
                let path = system_path(["/usr/bin/journalctl", "/bin/journalctl"])?;
                let mut cmd = site!(Silent::JournalRead, std::process::Command::new(path));
                cmd.args([
                    "-k",
                    "--since",
                    &format!("{} seconds ago", elapsed.as_secs().saturating_add(1)),
                    "--no-pager",
                    "-o",
                    "json",
                ]);
                Some(cmd)
            }
            #[cfg(target_os = "linux")]
            Self::Dmesg(elapsed) => {
                let path = system_path(["/usr/bin/dmesg", "/bin/dmesg"])?;
                let mut cmd = site!(Silent::JournalRead, std::process::Command::new(path));
                cmd.args([
                    "--since",
                    &format!("{} seconds ago", elapsed.as_secs().saturating_add(1)),
                ]);
                Some(cmd)
            }
        }
    }
}

#[cfg(not(windows))]
fn collect(collector: Collector) -> Option<String> {
    collect_output(&mut collector.command()?)
}

#[cfg(not(windows))]
fn collect_output(cmd: &mut std::process::Command) -> Option<String> {
    use crate::process::{ChildHandle, WaitOutcome, deadline::arm_callback};
    use crate::sync::LockExt as _;
    use std::io::Read as _;
    use std::process::Stdio;
    use std::sync::{Arc, Mutex, mpsc};
    use std::time::Duration;

    const MAX_BYTES: usize = 1024 * 1024;
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = ChildHandle::from_std(crate::process::spawn(cmd).ok()?);
    let stdout = child
        .take_stdout()
        .expect("the collector's stdout is piped");
    let (tx, rx) = mpsc::channel();
    let watch = Arc::new(Mutex::new(Some(
        child.into_watch(tx, std::convert::identity),
    )));
    let timer_watch = watch.clone();
    let deadline = arm_callback(Duration::from_secs(2), move || {
        if let Some(watch) = timer_watch.lock_ignore_poison().as_ref() {
            watch.kill();
        }
    });
    let mut bytes = Vec::new();
    let read = stdout.take(MAX_BYTES as u64).read_to_end(&mut bytes);
    let failed = read.is_err() || bytes.len() == MAX_BYTES;
    if failed && let Some(watch) = watch.lock_ignore_poison().as_ref() {
        watch.kill();
    }
    let outcome = rx.recv();
    // Disarm before reaping: the watch pins the PID.
    let timed_out = if let Some(claimed) = deadline.disarm() {
        claimed.wait();
        true
    } else {
        false
    };
    let watch = watch
        .lock_ignore_poison()
        .take()
        .expect("the collector owns its watch");
    if outcome.is_err() {
        watch.kill();
    }
    let reaped = watch.reap();
    if failed || timed_out || reaped.is_err() || !matches!(outcome, Ok(WaitOutcome::Exited(0))) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// The hint for a sandboxed command that failed with `exit_code`, or `None`
/// when there is nothing to say: the denials in the window since `since` that
/// are attributable to a PID in `pids`.
#[cfg(not(windows))]
pub(crate) fn denial_hint(pids: &HashSet<u32>, since: Instant, _exit_code: i32) -> Option<String> {
    if pids.is_empty() {
        return None;
    }
    let denials: Vec<Record> = platform::read_window(since.elapsed())?
        .into_iter()
        .filter(|record| pids.contains(&record.pid))
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
pub(crate) fn denial_hint(_pids: &HashSet<u32>, _since: Instant, exit_code: i32) -> Option<String> {
    if !plausible_access_denied_exit(exit_code) {
        return None;
    }
    Some(
        "the command failed while an OS sandbox (AppContainer) was active. On Windows a blocked \
         filesystem access surfaces only as an access-denied error on the command, with no kernel \
         log naming the path, so the exact path cannot be shown here. If the command needs a file \
         or directory the active grant's fs allow-list does not admit, widen the grant's read (or \
         write) set: the `` grant [ fs: [read: [`absolute …]] ] { … } `` block in ral, or `--extend` for \
         exarch (unless an exclusion covers it: an allowance cannot override one). A `` net: `offline` `` grant likewise withholds the network capability, so a command that \
         needs a socket will fail the same way."
            .to_string(),
    )
}

/// Render attributed records without trusting log text.
#[cfg(not(windows))]
pub(super) fn build_hint(denials: &[Record]) -> String {
    let mut out = String::from("sandbox denials were recorded for this command's processes:");
    for record in denials.iter().take(MAX_DENIAL_LINES) {
        let _ = write!(out, "\n  pid {}: {}", record.pid, record.op);
        if let Some(operand) = record.denied.operand() {
            let _ = write!(out, " {operand}");
        }
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
/// `exec` set can admit, a service it cannot open at all, a socket that is the
/// `net:` bit alone, the GPU that is the `gpu:` bit alone.  Leading with whichever line happened to carry a path is
/// what once offered a startup plist as the cure for a withheld keychain service.
#[cfg(not(windows))]
fn remedies(denials: &[Record]) -> Vec<String> {
    let (mut reads, mut writes, mut execs, mut services) = (vec![], vec![], vec![], vec![]);
    let mut unix_sockets = vec![];
    let mut socket = false;
    let mut gpu = false;
    let mut opaque = None;
    for Record { op, denied, .. } in denials {
        match denied {
            Denied::Read(path) => note(&mut reads, path),
            Denied::Write(path) => note(&mut writes, path),
            Denied::Exec(path) => note(&mut execs, path),
            Denied::Service(name) => note(&mut services, name),
            Denied::Socket => socket = true,
            Denied::Gpu => gpu = true,
            Denied::UnixSocket(path) => note(&mut unix_sockets, path),
            Denied::Opaque => opaque = opaque.or(Some(op)),
        }
    }
    let mut out = Vec::new();
    // A service the profile can explain leads, because it is the one class the
    // reader cannot act on: left below a grantable path, it reads as an aside
    // while the path it cannot help gets granted in vain.
    let explained = services
        .iter()
        .any(|service| platform::service_reason(service).is_some());
    if explained {
        out.push(service_remedy(&services));
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
    if !services.is_empty() && !explained {
        out.push(service_remedy(&services));
    }
    if socket {
        out.push(NET_REMEDY.to_string());
    }
    if gpu {
        out.push(GPU_REMEDY.to_string());
    }
    if !unix_sockets.is_empty() {
        out.push(format!("{UNIX_SOCKET_REMEDY}{}", listing(&unix_sockets)));
    }
    if out.is_empty() {
        out.push(opaque_remedy(opaque));
    }
    out
}

/// Keep what is not already listed: one denied operand is logged once per
/// probe, and a hint that repeats it reads as several separate problems.
#[cfg(not(windows))]
fn note<'a>(seen: &mut Vec<&'a Inert>, item: &'a Inert) {
    if !seen.contains(&item) {
        seen.push(item);
    }
}

/// The class's operands, under the same cap as the records.
#[cfg(not(windows))]
fn listing(items: &[&Inert]) -> String {
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
fn path_remedy(paths: &[&Inert], set: &str) -> String {
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
        "\nAdd {} (or a parent directory) to the grant's {set} set: the \
         `` grant [ fs: [{set}: [`absolute {}]] ] {{ … }} `` block in ral, or `--extend` for \
         exarch (unless an exclusion covers it: an allowance cannot override one). The sandbox matches fully-resolved paths, so a path reached through a \
         symlink inside a granted directory (e.g. ~/.config) needs the resolved path \
         above, not the link's own.",
        if one { "it" } else { "each" },
        quote_word(&paths[0].0)
    );
    out
}

/// Exec is its own set, and the kernel layer is the only one that sees a
/// re-exec — so this denial is never the fs grant's to answer.
#[cfg(not(windows))]
fn exec_remedy(paths: &[&Inert]) -> String {
    let mut out = String::from("the active grant's exec allow-list does not admit what ran here:");
    out.push_str(&listing(paths));
    let _ = write!(
        out,
        "\nAdmit it as a file or a directory: the `` grant [ exec: [allow: [`absolute {0}]] ] {{ … }} `` \
         block (or `` `dir `absolute {0} `` for a directory) in ral, or `--extend` for exarch, unless an exclusion covers it: an allowance cannot override one. A re-exec (`sh -c`, `find -exec`) \
         reaches only this layer, so an in-process admit alone does not carry it.",
        quote_word(&paths[0].0)
    );
    out
}

/// Services carrying a stated reason first: a refusal the profile can explain is
/// worth more than the startup probes that outnumber it.
#[cfg(not(windows))]
fn service_remedy(services: &[&Inert]) -> String {
    let mut ranked: Vec<(&Inert, Option<&'static str>)> = services
        .iter()
        .map(|service| (*service, platform::service_reason(service)))
        .collect();
    ranked.sort_by_key(|(_, why)| why.is_none());
    let mut out = String::from(
        "the base profile withheld a service no grant opens: a Mach or IPC name is the \
         profile's to decide, and the grant's fs, net and exec sets do not reach one:",
    );
    for (service, why) in ranked.iter().take(MAX_DENIAL_LINES) {
        match why {
            Some(why) => {
                let _ = write!(out, "\n  {service}: {why}");
            }
            None => {
                let _ = write!(out, "\n  {service}");
            }
        }
    }
    if ranked.len() > MAX_DENIAL_LINES {
        let _ = write!(out, "\n  ({} more)", ranked.len() - MAX_DENIAL_LINES);
    }
    if ranked.iter().any(|(_, why)| why.is_none()) {
        out.push_str(
            "\nA name with no reason given is usually harmless: a library probes a service \
             it can do without, and the denial is noise beside whatever actually failed.",
        );
    }
    out
}

#[cfg(not(windows))]
const NET_REMEDY: &str = "a socket was denied, so the active grant is `` net: `offline` ``; and that bit is the whole of \
     it. No fs or exec widening opens a socket, and a hostname cannot carry bytes out as a \
     query label either: the resolver is closed at the same layer.";

#[cfg(not(windows))]
const GPU_REMEDY: &str = "the GPU was denied: this frame withholds the GPU (`` gpu: `withheld ``), and \
     that is the whole of it. No fs, net or exec widening reaches the GPU, and no inner grant \
     restores it: run the command where no layer withholds the GPU.";

#[cfg(not(windows))]
const UNIX_SOCKET_REMEDY: &str = "a Unix socket was denied: request a stated fs write covering its \
     path, e.g. `` fs: [write: [`absolute /path/to/socket]] ``; an fs deny still wins. This admits \
     the daemon's authority outside the sandbox. Unrestricted fs admits no outside socket; \
     Linux Landlock ABI < 9 does not enforce this pathname-socket limit:";

/// The record names nothing to act on, so the hint says so rather than
/// guessing at an fs grant — unless a typed deny-set (Linux) has its own words
/// for the syscall.
#[cfg(not(windows))]
fn opaque_remedy(op: Option<&platform::Operation>) -> String {
    if let Some(described) = op.and_then(platform::describe_denial) {
        return described;
    }
    let what = op.map_or_else(
        || "a sandboxed operation was denied".to_string(),
        |op| format!("the sandboxed operation `{op}` was denied"),
    );
    format!(
        "{what}, and the kernel record names no operand: so this hint cannot say what to \
         widen. If the command needs a path the grant does not admit, add it to the grant's \
         read or write set: the `` grant [ fs: [read: [`absolute …]] ] {{ … }} `` block in ral, or \
         `--extend` for exarch, unless an exclusion covers it: an allowance cannot override one."
    )
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn inert_escapes_controls_and_caps_unicode_scalars() {
        let operand = "a\nSandbox: forged(7) deny(1) process-exec /bin/sh\r\t\x1b]52;c;secret\x07\x7f\u{85}\u{9b}";
        let inert = Inert::of(operand);
        assert!(!inert.0.chars().any(char::is_control), "{inert}");
        assert!(inert.0.contains("\\nSandbox:"), "{inert}");
        assert!(inert.0.contains("\\r\\t\\u{1b}]52;"), "{inert}");
        assert!(inert.0.contains("\\u{7f}\\u{85}\\u{9b}"), "{inert}");
        for value in 0..=0x9f {
            let c = char::from_u32(value).unwrap();
            if c.is_control() {
                assert!(!Inert::of(&c.to_string()).0.chars().any(char::is_control));
            }
        }
        let short = "é".repeat(512);
        assert_eq!(Inert::of(&short).0, short);
        let long = Inert::of(&"é".repeat(10_000));
        assert_eq!(long.0, format!("{} [truncated]", "é".repeat(512)));
    }

    #[cfg(unix)]
    #[test]
    fn hostile_operands_render_as_one_operand_per_listing() {
        for operand in [
            "/tmp/a\nSandbox: forged(7) deny(1) process-exec /bin/sh".to_string(),
            "a\tb".to_string(),
            "\x1b]52;c;secret\x07".to_string(),
            "\u{85}\u{9b}".to_string(),
            "x".repeat(10_000),
        ] {
            let inert = Inert::of(&operand);
            assert_eq!(listing(&[&inert]).lines().count(), 2);
            assert_eq!(path_remedy(&[&inert], "read").matches("\n  ").count(), 1);
            assert!(!path_remedy(&[&inert], "read").contains("exec allow-list"));
        }
    }

    #[cfg(unix)]
    fn fixture(role: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args([
            "--exact",
            "authority::sandbox::diag::tests::collector_fixture",
            "--nocapture",
        ])
        .env("RAL_DIAG_FIXTURE", role);
        cmd
    }

    #[cfg(unix)]
    #[test]
    fn collector_fixture() {
        use std::io::Write as _;
        match std::env::var("RAL_DIAG_FIXTURE").as_deref() {
            Ok("hang") => loop {
                std::thread::park();
            },
            Ok("flood") => {
                let _ = std::io::stdout().write_all(&vec![b'x'; 2 * 1024 * 1024]);
            }
            Ok("success") => crate::outln!("collector marker"),
            Ok("failure") => std::process::exit(1),
            #[cfg(target_os = "linux")]
            Ok("path") => {
                use std::time::Duration;
                for collector in [
                    Collector::Journal(Duration::ZERO),
                    Collector::Dmesg(Duration::ZERO),
                ] {
                    if let Some(mut cmd) = collector.command() {
                        assert!(std::path::Path::new(cmd.get_program()).is_absolute());
                        let _ = collect_output(&mut cmd);
                    }
                }
                crate::outln!("fixed collectors checked");
            }
            _ => {}
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_or_bounded_collector_supplies_no_hint() {
        use std::time::Duration;
        assert!(
            collect_output(&mut fixture("success"))
                .unwrap()
                .contains("collector marker")
        );
        for role in ["hang", "flood", "failure"] {
            let started = Instant::now();
            let diagnostic = collect_output(&mut fixture(role));
            assert!(diagnostic.is_none(), "{role}: {diagnostic:?}");
            assert!(
                started.elapsed() < Duration::from_secs(4),
                "{role} exceeded its bound"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_collector_first_on_path_never_runs() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("executed");
        for name in ["journalctl", "dmesg"] {
            let path = dir.path().join(name);
            std::fs::write(
                &path,
                format!("#!/bin/sh\nprintf ran > '{}'\n", marker.display()),
            )
            .unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let out = fixture("path").env("PATH", dir.path()).output().unwrap();
        assert!(out.status.success(), "{out:?}");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("fixed collectors checked"),
            "{out:?}"
        );
        assert!(!marker.exists(), "a diagnostic collector ran from PATH");
    }

    #[cfg(target_os = "macos")]
    fn hint(lines: &[&str]) -> String {
        let records: Vec<_> = lines
            .iter()
            .map(|line| {
                let message = format!("Sandbox: {}", line.split_once("Sandbox: ").unwrap().1);
                platform::parse_denial(&serde_json::json!({"eventMessage": message}).to_string())
                    .unwrap()
            })
            .collect();
        build_hint(&records)
    }

    /// The hint names the denied path verbatim, the symlink caveat, and both
    /// grant surfaces.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_names_path_and_symlink_caveat() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: cat(69436) deny(1) \
                    file-read-data /private/var/folders/ab/secret/data.txt";
        let hint = hint(&[line]);
        assert!(
            hint.contains("/private/var/folders/ab/secret/data.txt"),
            "hint must name the denied path; got {hint:?}"
        );
        assert!(
            hint.contains("symlink") && hint.contains("resolved"),
            "hint must carry the symlink caveat; got {hint:?}"
        );
        assert!(
            hint.contains("fs.read") && hint.contains("--extend"),
            "hint must name both grant surfaces; got {hint:?}"
        );
        assert!(
            hint.contains("pid 69436: file-read-data"),
            "hint must show the parsed record"
        );
    }

    /// Each class present answers for itself: the path is offered to the fs
    /// set, the Mach name reported as a service, and neither is dressed as
    /// the other.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_answers_every_class_of_denial_it_was_given() {
        let shm = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                   ipc-posix-shm-read-data apple.shm.notification_center";
        let read = "2026-06-19 12:00:00.100 kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                    file-read-data /Users/me/dotfiles/git/.config/git/config";
        let hint = hint(&[shm, read]);
        assert!(
            hint.contains("fs.read:\n  /Users/me/dotfiles/git/.config/git/config"),
            "hint must offer the resolved filesystem path to grant; got {hint:?}"
        );
        assert!(
            hint.contains("no grant opens") && hint.contains("apple.shm.notification_center"),
            "hint must name the IPC operand as a service, not a path; got {hint:?}"
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

    /// A withheld service is answered by the profile's own reason, not by an fs
    /// widening no grant could perform — the securityd case that offered a
    /// startup plist as the cure.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_quotes_the_profile_reason_for_a_withheld_service() {
        let plist = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: cargo(1) deny(1) \
                     file-read-data /Users/me/Library/Preferences/.GlobalPreferences.plist";
        let service = "2026-06-19 12:00:00.100 kernel[0] (Sandbox) Sandbox: cargo(1) deny(1) \
                    mach-lookup com.apple.SecurityServer";
        let hint = hint(&[plist, service]);
        assert!(
            hint.contains("keychain") && hint.contains("git-fetch-with-cli"),
            "a withheld service must carry the profile's reason; got {hint:?}"
        );
        assert!(
            hint.contains("com.apple.SecurityServer:"),
            "the reason must be attached to the service that has one; got {hint:?}"
        );
    }

    /// A denied write is answered by the write set: `read` would fail again.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_offers_the_write_set_for_a_denied_write() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: tee(1) deny(1) \
                    file-write-create /Users/me/notes.txt";
        let hint = hint(&[line]);
        assert!(
            hint.contains("fs.write") && hint.contains("[write: [`absolute /Users/me/notes.txt]]"),
            "a denied write must name the write set; got {hint:?}"
        );
        assert!(
            !hint.contains("fs.read"),
            "a denied write must not be blamed on the read set; got {hint:?}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn hint_offers_the_exec_set_for_a_denied_spawn() {
        let hint = hint(&["Sandbox: sh(1) deny(1) process-exec /usr/bin/security"]);
        assert!(hint.contains("exec allow-list"), "{hint}");
        assert!(
            hint.contains("[allow: [`absolute /usr/bin/security]]"),
            "{hint}"
        );
        assert!(!hint.contains("fs.read"), "{hint}");
    }

    /// `` net: `offline` `` is the whole remedy for a socket, and no fs wording
    /// belongs anywhere near it.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_names_the_net_bit_for_a_denied_socket() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: curl(1) deny(1) \
                    network-outbound";
        let hint = hint(&[line]);
        assert!(
            hint.contains("`` net: `offline` ``"),
            "a denied socket must name the net bit; got {hint:?}"
        );
        assert!(
            !hint.contains("read set"),
            "a denied socket must not be answered with an fs widening; got {hint:?}"
        );
    }

    /// The GPU is the `gpu` bit's alone, and no fs wording belongs near it.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_names_the_gpu_bit_for_a_denied_gpu() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: python3(1) deny(1) \
                    iokit-open-user-client AGXDeviceUserClient";
        let hint = hint(&[line]);
        assert!(
            hint.contains("`` gpu: `withheld ``"),
            "a denied GPU must name the gpu bit; got {hint:?}"
        );
        assert!(
            !hint.contains("read set"),
            "a denied GPU must not be answered with an fs widening; got {hint:?}"
        );
    }

    /// A Unix socket is not the net bit's: `` net: `offline` `` is no cure when
    /// the grant already has the network.
    #[cfg(target_os = "macos")]
    #[test]
    fn hint_answers_a_unix_socket_apart_from_the_net_bit() {
        let line = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: ssh-add(1) deny(1) \
                    network-outbound /private/tmp/com.apple.launchd.x/Listeners";
        let hint = hint(&[line]);
        assert!(
            hint.contains("fs: [write: [`absolute /path/to/socket]]")
                && hint.contains("Linux Landlock ABI < 9")
                && hint.contains("\n  /private/tmp/com.apple.launchd.x/Listeners"),
            "a denied Unix socket must name the write grant and its limit; got {hint:?}"
        );
        assert!(
            !hint.contains("`` net: `offline` ``"),
            "a denied Unix socket must not be blamed on the net bit; got {hint:?}"
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
        let hint = hint(&refs);
        assert!(
            hint.contains("(2 more)"),
            "five denials over a cap of three must note two more; got {hint:?}"
        );
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
