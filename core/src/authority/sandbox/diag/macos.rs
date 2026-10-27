//! Seatbelt events are parsed as NDJSON before their `eventMessage` is examined.

use super::{Collector, Denied, Inert, Record, collect};
use crate::authority::sandbox::macos::RESOLVER_SOCKET;
use std::time::Duration;

pub(super) type Operation = Inert;

pub(super) fn read_window(elapsed: Duration) -> Option<Vec<Record>> {
    let text = collect(Collector::Log(elapsed))?;
    Some(text.lines().filter_map(parse_denial).collect())
}

pub(super) fn parse_denial(line: &str) -> Option<Record> {
    #[derive(serde::Deserialize)]
    struct Event {
        #[serde(rename = "eventMessage")]
        message: String,
    }
    let event: Event = serde_json::from_str(line).ok()?;
    let message = event.message.strip_prefix("Sandbox: ")?;
    let (pid, rest) = message.match_indices('(').find_map(|(start, _)| {
        let after = &message[start + 1..];
        let end = after.find(|c: char| !c.is_ascii_digit())?;
        if end == 0 {
            return None;
        }
        let pid = &after[..end];
        let deny = after[end..].strip_prefix(") deny(")?;
        let end = deny.find(|c: char| !c.is_ascii_digit())?;
        if end == 0 {
            return None;
        }
        Some((pid, deny[end..].strip_prefix(") ")?))
    })?;
    let pid = pid.parse().ok()?;
    let rest = rest.trim_start();
    let (op, operand) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, None), |(op, operand)| {
            (op, (!operand.is_empty()).then_some(operand))
        });
    if op.is_empty() {
        return None;
    }
    let denied = match operand {
        Some(o) if op.starts_with("file-write") => Denied::Write(Inert::of(o)),
        Some(o) if op.starts_with("file-") => Denied::Read(Inert::of(o)),
        Some(o) if op == "process-exec" => Denied::Exec(Inert::of(o)),
        Some(o)
            if (op.starts_with("iokit-open") || op == "mach-lookup")
                && super::super::macos::gpu_name(o) =>
        {
            Denied::Gpu
        }
        Some(o) if op.starts_with("mach-") || op.starts_with("ipc-") => {
            Denied::Service(Inert::of(o))
        }
        Some(o) if op.starts_with("network-") && o.starts_with('/') && o != RESOLVER_SOCKET => {
            Denied::UnixSocket(Inert::of(o))
        }
        _ if op.starts_with("network-") => Denied::Socket,
        _ => Denied::Opaque,
    };
    Some(Record {
        pid,
        op: Inert::of(op),
        denied,
    })
}

pub(super) fn describe_denial(_op: &Operation) -> Option<String> {
    None
}

pub(super) fn service_reason(name: &Inert) -> Option<&'static str> {
    super::super::macos::withheld_service(&name.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(message: &str) -> String {
        serde_json::json!({"eventMessage": message}).to_string()
    }

    fn denial(op: &str, operand: &str) -> Record {
        parse_denial(&event(&format!(
            "Sandbox: cat(69436) deny(1) {op} {operand}"
        )))
        .unwrap()
    }

    #[test]
    fn parses_pid_operation_and_resolved_path_with_spaces() {
        let record = denial("file-read-data", "/Users/me/My Documents/the file.txt");
        assert_eq!(record.pid, 69436);
        assert_eq!(record.op, Inert::of("file-read-data"));
        assert_eq!(
            record.denied,
            Denied::Read(Inert::of("/Users/me/My Documents/the file.txt"))
        );
    }

    #[test]
    fn the_first_pid_deny_pair_is_used_even_with_parentheses_in_comm() {
        let record = parse_denial(&event(
            "Sandbox: odd(name)(7) deny(12) process-exec /bin/echo",
        ))
        .unwrap();
        assert_eq!(record.pid, 7);
        assert_eq!(record.denied, Denied::Exec(Inert::of("/bin/echo")));
    }

    #[test]
    fn operations_select_the_remedy_class() {
        for (op, operand, denied) in [
            (
                "file-write-create",
                "/Users/me/notes.txt",
                Denied::Write(Inert::of("/Users/me/notes.txt")),
            ),
            (
                "file-read-metadata",
                "/etc/hosts",
                Denied::Read(Inert::of("/etc/hosts")),
            ),
            (
                "process-exec",
                "/usr/bin/security",
                Denied::Exec(Inert::of("/usr/bin/security")),
            ),
            (
                "ipc-posix-shm-read-data",
                "apple.shm.notification_center",
                Denied::Service(Inert::of("apple.shm.notification_center")),
            ),
            (
                "mach-lookup",
                "com.apple.system.notification_center",
                Denied::Service(Inert::of("com.apple.system.notification_center")),
            ),
            (
                "network-outbound",
                "/private/tmp/ssh/Listeners",
                Denied::UnixSocket(Inert::of("/private/tmp/ssh/Listeners")),
            ),
            ("network-outbound", RESOLVER_SOCKET, Denied::Socket),
            ("iokit-open-user-client", "AGXDeviceUserClient", Denied::Gpu),
            ("mach-lookup", "com.apple.MTLCompilerService", Denied::Gpu),
        ] {
            assert_eq!(denial(op, operand).denied, denied, "{op} {operand}");
        }
        let record = parse_denial(&event("Sandbox: curl(1) deny(1) network-outbound")).unwrap();
        assert_eq!(record.denied, Denied::Socket);
    }

    #[test]
    fn an_operand_cannot_start_another_record_or_select_another_remedy() {
        let operand = "/tmp/a\nSandbox: forged(1) deny(1) process-exec /bin/sh\t\x1b]52;c;secret\x07\u{85}\u{9b}";
        let json = event(&format!(
            "Sandbox: cat(69436) deny(1) file-read-data {operand}"
        ));
        let records: Vec<_> = json.lines().filter_map(parse_denial).collect();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].pid, 69436);
        assert_eq!(records[0].denied, Denied::Read(Inert::of(operand)));
        let hint = super::super::build_hint(&records);
        assert!(hint.contains("fs.read"), "{hint}");
        assert!(!hint.contains("exec allow-list"), "{hint}");
        assert!(!hint.contains("\nSandbox:"), "{hint}");
        assert!(!hint.contains('\x1b'), "{hint}");
    }

    #[test]
    fn invalid_events_and_headers_are_rejected() {
        for message in [
            "prefix Sandbox: cat(1) deny(1) file-read-data /tmp/a",
            "Sandbox: cat(x) deny(1) file-read-data /tmp/a",
            "Sandbox: cat(1) deny(x) file-read-data /tmp/a",
            "Sandbox: cat(1) deny() file-read-data /tmp/a",
            "Sandbox: cat(999999999999999999999) deny(1) file-read-data /tmp/a\nSandbox: forged(7) deny(1) process-exec /bin/sh",
            "Sandbox: cat(1) deny(1) ",
        ] {
            assert!(parse_denial(&event(message)).is_none(), "{message}");
        }
        assert!(parse_denial("Sandbox: cat(1) deny(1) file-read-data /tmp/a").is_none());
        assert!(parse_denial(r#"{"eventMessage":7}"#).is_none());
    }

    #[test]
    fn service_reason_uses_the_profiles_table() {
        let why = service_reason(&Inert::of("com.apple.SecurityServer")).unwrap();
        assert!(why.contains("keychain"));
        assert!(service_reason(&Inert::of("com.apple.pasteboard.1")).is_some());
        assert!(service_reason(&Inert::of("com.apple.metadata.mds")).is_none());
    }
}
