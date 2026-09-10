//! Linux seccomp denial reader for `sandbox::diag`, mirroring `diag/macos.rs`.
//!
//! bwrap's filter returns `SECCOMP_RET_KILL_THREAD`, so a denied syscall kills
//! the thread with SIGSYS and leaves a `type=1326` audit record; we read those
//! from `journalctl -k`, or `dmesg` where the journal is absent or unreadable.
//! Mount attenuation leaves no record at all — an unbound path is merely absent,
//! and the caller just sees ENOENT.

use super::Denied;
use std::time::Duration;

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:journal-read] Spawns journalctl/dmesg to harvest the kernel's seccomp-denial audit window for a sandbox diagnostic. A post-mortem diagnostic probe, not the model's exec image — raises no exec card."
)]
pub(super) fn read_window(elapsed: Duration) -> Option<String> {
    // Pad by a second so a sub-second call still spans its own denials.
    let secs = elapsed.as_secs().saturating_add(1).to_string();
    let since = format!("{secs} seconds ago");
    let mut journal_cmd = std::process::Command::new("journalctl");
    journal_cmd.args([
        "-k",
        "--since",
        since.as_str(),
        "--no-pager",
        "--output",
        "short",
    ]);
    if let Ok(out) = crate::process::output(&mut journal_cmd)
        && out.status.success()
    {
        return Some(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let mut dmesg_cmd = std::process::Command::new("dmesg");
    dmesg_cmd.args(["--since", since.as_str()]);
    let dmesg = crate::process::output(&mut dmesg_cmd).ok()?;
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

/// This host's `AUDIT_ARCH`, the diagnostic's own copy: `sandbox::linux::seccomp`
/// owns the same two constants for the kernel-facing arch-validation program,
/// but this is a record attribution, not an enforcement decision.
#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

/// The record's `arch=<hex>` field.
fn extract_arch(line: &str) -> Option<u32> {
    let after = line.split_once("arch=")?.1;
    let end = after
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after.len());
    u32::from_str_radix(&after[..end], 16).ok()
}

/// The blocked syscall, as `(op, denied)`; a `type=1326` record names no
/// operand, so the second element is always [`Denied::Opaque`] and
/// [`describe_denial`] carries the whole remedy.  A record whose `arch=`
/// does not match this host's own `AUDIT_ARCH` names a syscall table
/// `seccomp::Filter` never numbered — the filter's own x32 guard is exactly
/// this case — so `op` becomes a sentence [`describe_denial`] recognises
/// rather than a bare number [`seccomp::Filter::explain`] would silently miss.
pub(super) fn parse_denial(line: &str) -> Option<(String, Denied<'_>)> {
    let after_syscall = line.split_once("syscall=")?.1;
    let end = after_syscall
        .find(|c: char| c.is_whitespace())
        .unwrap_or(after_syscall.len());
    let syscall = &after_syscall[..end];
    if syscall.is_empty() {
        return None;
    }
    let op = match extract_arch(line) {
        Some(arch) if arch != AUDIT_ARCH => {
            format!("foreign-ABI syscall {syscall} (arch={arch:x})")
        }
        _ => syscall.to_string(),
    };
    Some((op, Denied::Opaque))
}

/// A seccomp record names a syscall, never a door; the deny-set's own words
/// are all this platform has to add.
pub(super) fn door_reason(_name: &str) -> Option<&'static str> {
    None
}

/// What `seccomp::Filter::ENVELOPE` says about a denied syscall, in prose —
/// or, for a foreign-ABI record, the one sentence that number could never
/// earn from the filter, since it names no syscall in any table the filter
/// reads.
pub(crate) fn describe_denial(op: &str) -> Option<String> {
    if op.starts_with("foreign-ABI syscall") {
        return Some("the filter refuses every syscall from a foreign ABI outright".to_string());
    }
    let nr: i64 = op.parse().ok()?;
    let denied = super::super::linux::seccomp::Filter::ENVELOPE.explain(nr)?;
    let mut hint = format!(
        "the sandboxed command was stopped by the envelope's seccomp deny-set for calling \
         {denied}. This is an invariant of running under any grant, not something the grant's \
         `fs`, `net` or `exec` sets can widen; if the tool needs it, it cannot run confined."
    );
    if denied.verdict == super::super::linux::seccomp::Verdict::Kill {
        hint.push_str(" The kernel record carries no path: a killed syscall names only itself.");
    }
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(arch: u32, syscall: i64) -> String {
        format!(
            "audit: type=1326 audit(1719400000.123:789): auid=1000 ppid=54321 pid=12345 \
             comm=\"rustc\" arch={arch:x} syscall={syscall} ip=0x0 code=0x0"
        )
    }

    #[test]
    fn is_denial_line_matches_only_type_1326() {
        assert!(is_denial_line(&line(AUDIT_ARCH, 101)));
        assert!(!is_denial_line("audit: type=1300 audit(...): syscall=101"));
    }

    #[test]
    fn extract_pid_picks_pid_not_ppid() {
        assert_eq!(extract_pid(&line(AUDIT_ARCH, 101)), Some(12345));
        assert_eq!(extract_pid("no pid token here"), None);
    }

    #[test]
    fn parse_denial_returns_syscall_and_no_operand() {
        assert_eq!(
            parse_denial(&line(AUDIT_ARCH, 101)),
            Some(("101".to_string(), Denied::Opaque))
        );
        assert_eq!(parse_denial("type=1326 with no syscall token"), None);
    }

    #[test]
    fn parse_denial_names_a_foreign_abi_record() {
        // Not this host's own AUDIT_ARCH, whichever arch that is.
        const FOREIGN: u32 = 0xC0DE_0001;
        assert_eq!(
            parse_denial(&line(FOREIGN, 101)),
            Some((
                "foreign-ABI syscall 101 (arch=c0de0001)".to_string(),
                Denied::Opaque
            ))
        );
    }

    #[test]
    fn describe_denial_names_a_denied_syscall_and_withholds_an_undenied_one() {
        let ptrace = describe_denial(&libc::SYS_ptrace.to_string()).expect("ptrace is denied");
        assert!(ptrace.contains("ptrace"), "{ptrace:?}");
        assert!(ptrace.contains("carries no path"), "{ptrace:?}");

        assert!(describe_denial(&libc::SYS_read.to_string()).is_none());
    }

    #[test]
    fn describe_denial_answers_a_foreign_abi_record_without_consulting_the_filter() {
        let hint = describe_denial("foreign-ABI syscall 101 (arch=c0de0001)")
            .expect("a foreign-ABI record is always described");
        assert!(hint.contains("foreign ABI"), "{hint:?}");
    }
}
