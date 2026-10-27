//! Linux seccomp records come from the journal's JSON MESSAGE, or dmesg.

use super::{Collector, Denied, Inert, Record, collect};
use seccompiler::SeccompAction;
use std::fmt;
use std::time::Duration;

pub(super) fn read_window(elapsed: Duration) -> Option<Vec<Record>> {
    if let Some(text) = collect(Collector::Journal(elapsed)) {
        return Some(text.lines().filter_map(parse_journal).collect());
    }
    let text = collect(Collector::Dmesg(elapsed))?;
    Some(text.lines().filter_map(parse_denial).collect())
}

fn parse_journal(line: &str) -> Option<Record> {
    #[derive(serde::Deserialize)]
    struct Event {
        #[serde(rename = "MESSAGE")]
        message: String,
    }
    let event: Event = serde_json::from_str(line).ok()?;
    parse_denial(&event.message)
}

#[cfg(target_arch = "x86_64")]
const AUDIT_ARCH: u32 = 0xC000_003E;
#[cfg(target_arch = "aarch64")]
const AUDIT_ARCH: u32 = 0xC000_00B7;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct Operation {
    nr: i64,
    arch: u32,
    action: SeccompAction,
}

impl fmt::Display for Operation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "syscall {} (arch={:x}, action={:?})",
            self.nr, self.arch, self.action
        )
    }
}

fn action(code: u32) -> Option<SeccompAction> {
    let data = code & libc::SECCOMP_RET_DATA;
    match code & libc::SECCOMP_RET_ACTION_FULL {
        libc::SECCOMP_RET_KILL_THREAD => Some(SeccompAction::KillThread),
        libc::SECCOMP_RET_KILL_PROCESS => Some(SeccompAction::KillProcess),
        libc::SECCOMP_RET_TRAP => Some(SeccompAction::Trap),
        libc::SECCOMP_RET_ERRNO => Some(SeccompAction::Errno(data)),
        libc::SECCOMP_RET_TRACE => Some(SeccompAction::Trace(data)),
        libc::SECCOMP_RET_LOG => Some(SeccompAction::Log),
        libc::SECCOMP_RET_ALLOW => Some(SeccompAction::Allow),
        _ => None,
    }
}

pub(in crate::authority::sandbox) fn parse_denial(message: &str) -> Option<Record> {
    // Quoted comm/exe fields are not audit fields.
    let fields = message
        .split('"')
        .step_by(2)
        .flat_map(str::split_whitespace);
    let (mut kind, mut pid, mut nr, mut arch, mut code) = (None, None, None, None, None);
    for field in fields {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "type" => kind = Some(value),
            "pid" => pid = Some(value.parse().ok()?),
            "syscall" => nr = Some(value.parse().ok()?),
            "arch" => arch = Some(u32::from_str_radix(value, 16).ok()?),
            "code" => {
                code =
                    Some(u32::from_str_radix(value.strip_prefix("0x").unwrap_or(value), 16).ok()?);
            }
            _ => {}
        }
    }
    if kind != Some("1326") {
        return None;
    }
    Some(Record {
        pid: pid?,
        op: Operation {
            nr: nr?,
            arch: arch?,
            action: action(code?)?,
        },
        denied: Denied::Opaque,
    })
}

pub(super) fn service_reason(_name: &Inert) -> Option<&'static str> {
    None
}

pub(super) fn describe_denial(op: &Operation) -> Option<String> {
    if op.arch != AUDIT_ARCH {
        return (op.action == SeccompAction::KillProcess)
            .then(|| "the filter refuses every syscall from a foreign ABI outright".to_string());
    }
    let denied = super::super::linux::seccomp::Filter::ENVELOPE.explain(op.nr, &op.action)?;
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

    fn line(arch: u32, syscall: i64, code: u32) -> String {
        format!(
            "audit: type=1326 audit(1719400000.123:789): auid=1000 ppid=54321 pid=12345 \
             comm=\"rustc\" arch={arch:x} syscall={syscall} ip=0x0 code=0x{code:x}"
        )
    }

    #[test]
    fn parse_denial_requires_the_audit_type_and_each_attribution_field() {
        let line = line(AUDIT_ARCH, libc::SYS_ptrace, libc::SECCOMP_RET_KILL_THREAD);
        let record = parse_denial(&line).unwrap();
        assert_eq!(record.pid, 12345);
        assert_eq!(record.op.nr, libc::SYS_ptrace);
        assert_eq!(record.op.action, SeccompAction::KillThread);
        assert_eq!(record.denied, Denied::Opaque);
        for field in ["type=1326", "pid=12345", "arch=", "syscall=", "code="] {
            assert!(
                parse_denial(&line.replace(field, "invalid=")).is_none(),
                "{field}"
            );
        }
        assert!(parse_denial(&line.replace("type=1326", "type=1300")).is_none());
    }

    #[test]
    fn quoted_program_names_cannot_forge_audit_fields() {
        let line = line(AUDIT_ARCH, libc::SYS_ptrace, libc::SECCOMP_RET_KILL_THREAD)
            .replace("comm=\"rustc\"", "comm=\"pid=7 syscall=0 code=0x50001\"");
        let record = parse_denial(&line).unwrap();
        assert_eq!(record.pid, 12345);
        assert_eq!(record.op.nr, libc::SYS_ptrace);
        assert_eq!(record.op.action, SeccompAction::KillThread);
    }

    #[test]
    fn journal_message_is_one_record_even_with_an_embedded_newline() {
        let message = line(AUDIT_ARCH, libc::SYS_ptrace, 0).replace(
            "comm=\"rustc\"",
            "comm=\"rustc\n type=1326 pid=7 syscall=0 arch=c000003e code=0x50001\"",
        );
        let json = serde_json::json!({"MESSAGE": message}).to_string();
        assert_eq!(json.lines().count(), 1);
        assert_eq!(parse_journal(&json).unwrap().pid, 12345);
        assert_eq!(parse_journal(&json), parse_denial(&message));
        assert!(parse_journal("not JSON").is_none());
        assert!(parse_journal(r#"{"MESSAGE":[1,2]}"#).is_none());
    }

    #[test]
    fn remedies_require_the_rules_action_including_the_errno() {
        let op = |nr, action| Operation {
            nr,
            arch: AUDIT_ARCH,
            action,
        };
        let ptrace = describe_denial(&op(libc::SYS_ptrace, SeccompAction::KillThread)).unwrap();
        assert!(
            ptrace.contains("ptrace") && ptrace.contains("carries no path"),
            "{ptrace}"
        );
        assert!(describe_denial(&op(libc::SYS_ptrace, SeccompAction::Errno(1))).is_none());
        assert!(describe_denial(&op(libc::SYS_ptrace, SeccompAction::KillProcess)).is_none());
        assert!(describe_denial(&op(libc::SYS_read, SeccompAction::KillThread)).is_none());
        assert!(
            describe_denial(&op(
                libc::SYS_clone3,
                SeccompAction::Errno(libc::ENOSYS.unsigned_abs())
            ))
            .is_some()
        );
        assert!(
            describe_denial(&op(
                libc::SYS_clone3,
                SeccompAction::Errno(libc::EPERM.unsigned_abs())
            ))
            .is_none()
        );
    }

    #[test]
    fn a_foreign_abi_requires_the_arch_prologues_action() {
        let mut op = Operation {
            nr: libc::SYS_ptrace,
            arch: 0xC0DE_0001,
            action: SeccompAction::KillProcess,
        };
        assert!(describe_denial(&op).unwrap().contains("foreign ABI"));
        op.action = SeccompAction::Errno(1);
        assert!(describe_denial(&op).is_none());
    }

    #[test]
    fn action_decodes_the_kernel_code() {
        assert_eq!(action(0x0005_0001), Some(SeccompAction::Errno(1)));
        assert_eq!(action(0x8000_0000), Some(SeccompAction::KillProcess));
        assert_eq!(action(0x0003_0000), Some(SeccompAction::Trap));
        assert_eq!(action(0x1234_0000), None);
    }
}
