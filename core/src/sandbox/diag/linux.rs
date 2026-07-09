//! Linux seccomp denial reader.
//!
//! The seccomp BPF filter installed by `bwrap --seccomp` returns
//! `SECCOMP_RET_KILL_THREAD` on each denied syscall; the kernel ends
//! the offending thread with SIGSYS and emits a `type=1326` audit
//! record carrying pid, comm, exe, arch, syscall, and ip.  We read
//! these via `journalctl -k --since <window>` (systemd-based distros),
//! with plain `dmesg` as a fallback for unreadable journals or
//! non-systemd hosts.
//!
//! bwrap's mount-namespace restrictions, by contrast, surface as
//! ENOENT on the affected syscall with no kernel-side deny event;
//! those are not harvested here.

use std::time::Duration;

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:journal-read] Spawns journalctl/dmesg to harvest the kernel's seccomp-denial audit window for a sandbox diagnostic. A post-mortem diagnostic probe, not the model's exec image — raises no exec card."
)]
pub(super) fn read_window(elapsed: Duration) -> Option<String> {
    // `journalctl --since` accepts relative strings like "2 seconds
    // ago"; pad by one second to cover sub-second calls, mirroring
    // the macOS reader.
    let secs = elapsed.as_secs().saturating_add(1).to_string();
    let since = format!("{secs} seconds ago");
    let journal = std::process::Command::new("journalctl")
        .args([
            "-k",
            "--since",
            since.as_str(),
            "--no-pager",
            "--output",
            "short",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();
    if let Ok(out) = journal
        && out.status.success() {
            return Some(String::from_utf8_lossy(&out.stdout).into_owned());
        }
    let dmesg = std::process::Command::new("dmesg")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&dmesg.stdout).into_owned())
}

pub(super) fn is_denial_line(line: &str) -> bool {
    line.contains("type=1326")
}

/// `... audit: type=1326 audit(…): … pid=12345 comm="rustc" …`
/// → `Some(12345)`.  The leading space on ` pid=` is what
/// distinguishes the audit record's `pid=` from its `ppid=`.
pub(super) fn extract_pid(line: &str) -> Option<u32> {
    let after_pid = line.split_once(" pid=")?.1;
    let end = after_pid
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_pid.len());
    after_pid[..end].parse().ok()
}

/// Split a denial line into `(operation, path)`.  The `type=1326`
/// seccomp audit record names the blocked syscall but carries no path,
/// so the operation is the `syscall=<n>` token and the path is always
/// `None`; the hint degrades to a pathless explanation accordingly.
pub(super) fn parse_denial(line: &str) -> Option<(&str, Option<&str>)> {
    let after = line.split_once("syscall=")?.1;
    let end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    let syscall = &after[..end];
    if syscall.is_empty() {
        return None;
    }
    Some((syscall, None))
}
