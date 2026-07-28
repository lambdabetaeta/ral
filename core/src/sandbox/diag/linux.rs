//! Linux seccomp denial reader for `sandbox::diag`, mirroring `diag/macos.rs`.
//!
//! bwrap's filter returns `SECCOMP_RET_KILL_THREAD`, so a denied syscall kills
//! the thread with SIGSYS and leaves a `type=1326` audit record; we read those
//! from `journalctl -k`, or `dmesg` where the journal is absent or unreadable.
//! Mount attenuation leaves no record at all — an unbound path is merely absent,
//! and the caller just sees ENOENT.

use std::time::Duration;

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:journal-read] Spawns journalctl/dmesg to harvest the kernel's seccomp-denial audit window for a sandbox diagnostic. A post-mortem diagnostic probe, not the model's exec image — raises no exec card."
)]
pub(super) fn read_window(elapsed: Duration) -> Option<String> {
    // Pad by a second so a sub-second call still spans its own denials.
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
        && out.status.success()
    {
        return Some(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let dmesg = std::process::Command::new("dmesg")
        .args(["--since", since.as_str()])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&dmesg.stdout).into_owned())
}

pub(super) fn is_denial_line(line: &str) -> bool {
    line.contains("type=1326")
}

/// The audit record's ` pid=`; the leading space is what excludes `ppid=`.
pub(super) fn extract_pid(line: &str) -> Option<u32> {
    let after_pid = line.split_once(" pid=")?.1;
    let end = after_pid
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(after_pid.len());
    after_pid[..end].parse().ok()
}

/// The blocked `syscall=<n>`; a `type=1326` record carries no path, so always `None`.
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

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "audit: type=1326 audit(1719400000.123:789): auid=1000 ppid=54321 \
                        pid=12345 comm=\"rustc\" arch=c00000b7 syscall=101 ip=0x0 code=0x0";

    #[test]
    fn is_denial_line_matches_only_type_1326() {
        assert!(is_denial_line(LINE));
        assert!(!is_denial_line("audit: type=1300 audit(...): syscall=101"));
    }

    #[test]
    fn extract_pid_picks_pid_not_ppid() {
        assert_eq!(extract_pid(LINE), Some(12345));
        assert_eq!(extract_pid("no pid token here"), None);
    }

    #[test]
    fn parse_denial_returns_syscall_and_no_path() {
        assert_eq!(parse_denial(LINE), Some(("101", None)));
        assert_eq!(parse_denial("type=1326 with no syscall token"), None);
    }
}
