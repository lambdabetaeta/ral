//! macOS Seatbelt denial reader — the `platform` module `sandbox::diag`
//! selects here, mirroring `diag/linux.rs`.
//!
//! A denial reaches user code as a bare `EPERM`; only the unified system log
//! names the operation and path, in the shape
//! `(Sandbox) Sandbox: <comm>(<pid>) deny(<n>) <op> <operand…>` that every
//! parser below keys off.

use super::Denied;
use std::time::Duration;

#[allow(
    clippy::disallowed_methods,
    reason = "[silent:log-show] macOS sandbox diagnostics: shells out to `/usr/bin/log show` to read Seatbelt denial records from the unified log; a diagnostic probe, not turn-time model data I/O, raises no surface card."
)]
pub(super) fn read_window(elapsed: Duration) -> Option<String> {
    // `--last` rounds to whole seconds, so pad by one or a sub-second call
    // asks for a zero-length window and sees nothing.
    let secs = elapsed.as_secs().saturating_add(1).to_string();
    let last_arg = format!("{secs}s");
    let mut cmd = std::process::Command::new("/usr/bin/log");
    cmd.args([
        "show",
        "--predicate",
        "eventMessage BEGINSWITH \"Sandbox: \"",
        "--last",
        last_arg.as_str(),
        "--style",
        "compact",
    ]);
    let output = crate::process::output(&mut cmd).ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(super) fn is_denial_line(line: &str) -> bool {
    line.contains("(Sandbox) Sandbox:") && line.contains("deny")
}

/// The PID Seatbelt attributed the denial to — the one inside `comm(pid)`,
/// which `sandbox::diag` matches against the call's descendant set.
pub(super) fn extract_pid(line: &str) -> Option<u32> {
    let after_tag = line.split_once("Sandbox: ")?.1;
    let (_, after_open) = after_tag.split_once('(')?;
    let (digits, _) = after_open.split_once(')')?;
    digits.parse().ok()
}

/// Split a denial into its operation and what that operation names.  The
/// operand is the whole trimmed remainder, since macOS paths contain spaces,
/// and the operation decides which remedy class it belongs to — Seatbelt
/// resolves the paths it logs, so a [`Denied::Read`] path is exactly the one
/// the grant needs, while a Mach or IPC name reaches the caller as a
/// [`Denied::Door`] it cannot mistake for one.
///
/// The operation is owned because Linux's counterpart builds a syscall name
/// rather than borrowing one, and one caller reads both.
pub(super) fn parse_denial(line: &str) -> Option<(String, Denied<'_>)> {
    let after_tag = line.split_once("Sandbox: ")?.1;
    let after_deny = after_tag.split_once("deny(")?.1.split_once(')')?.1;
    let mut rest = after_deny.trim_start().splitn(2, char::is_whitespace);
    let op = rest.next()?;
    if op.is_empty() {
        return None;
    }
    let operand = rest.next().map(str::trim).filter(|o| !o.is_empty());
    let denied = match operand {
        // `file-write*` names the write set; metadata and ioctl ride with read.
        Some(o) if op.starts_with("file-write") => Denied::Write(o),
        Some(o) if op.starts_with("file-") => Denied::Read(o),
        Some(o) if op == "process-exec" => Denied::Exec(o),
        Some(o) if op.starts_with("mach-") || op.starts_with("ipc-") => Denied::Door(o),
        _ if op.starts_with("network-") => Denied::Socket,
        _ => Denied::Opaque,
    };
    Some((op.to_string(), denied))
}

/// Seatbelt's log line already names the operation; there is no typed
/// deny-set here to add anything to it.
pub(crate) fn describe_denial(_op: &str) -> Option<String> {
    None
}

/// Why the base profile withholds this door, where it has a reason on record.
pub(super) fn door_reason(name: &str) -> Option<&'static str> {
    super::super::macos::withheld_door(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE: &str = "2026-06-19 12:00:00.000 kernel[0] (Sandbox) Sandbox: \
                        cat(69436) deny(1) file-read-data /private/var/folders/ab/secret/data.txt";

    #[test]
    fn is_denial_line_recognises_seatbelt_lines() {
        assert!(is_denial_line(LINE));
        assert!(!is_denial_line("2026-06-19 some unrelated log line"));
    }

    #[test]
    fn extract_pid_reads_the_attributed_pid() {
        assert_eq!(extract_pid(LINE), Some(69436));
        assert_eq!(extract_pid("garbage without the tag"), None);
    }

    #[test]
    fn parse_denial_splits_op_and_resolved_path() {
        assert_eq!(
            parse_denial(LINE),
            Some((
                "file-read-data".to_string(),
                Denied::Read("/private/var/folders/ab/secret/data.txt")
            ))
        );
    }

    #[test]
    fn parse_denial_keeps_spaces_in_the_path() {
        let line = "kernel[0] (Sandbox) Sandbox: bat(1) deny(1) file-read-data \
                    /Users/me/My Documents/the file.txt";
        assert_eq!(
            parse_denial(line),
            Some((
                "file-read-data".to_string(),
                Denied::Read("/Users/me/My Documents/the file.txt")
            ))
        );
    }

    /// A write answered by widening `read` fails a second time, so the write
    /// ops are their own class.
    #[test]
    fn parse_denial_separates_a_denied_write_from_a_denied_read() {
        let write = "kernel[0] (Sandbox) Sandbox: tee(1) deny(1) file-write-create \
                     /Users/me/notes.txt";
        assert_eq!(
            parse_denial(write),
            Some((
                "file-write-create".to_string(),
                Denied::Write("/Users/me/notes.txt")
            ))
        );
        let meta = "kernel[0] (Sandbox) Sandbox: ls(1) deny(1) file-read-metadata /etc/hosts";
        assert_eq!(
            parse_denial(meta),
            Some(("file-read-metadata".to_string(), Denied::Read("/etc/hosts")))
        );
    }

    #[test]
    fn parse_denial_handles_op_without_an_operand() {
        let line = "kernel[0] (Sandbox) Sandbox: foo(1) deny(1) network-outbound";
        assert_eq!(
            parse_denial(line),
            Some(("network-outbound".to_string(), Denied::Socket))
        );
    }

    /// A service name reaches the caller as a door, so no wording can offer it
    /// as a path to grant.
    #[test]
    fn parse_denial_calls_a_service_operand_a_door() {
        let shm = "kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                   ipc-posix-shm-read-data apple.shm.notification_center";
        assert_eq!(
            parse_denial(shm),
            Some((
                "ipc-posix-shm-read-data".to_string(),
                Denied::Door("apple.shm.notification_center")
            ))
        );

        let mach = "kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                    mach-lookup com.apple.system.notification_center";
        assert_eq!(
            parse_denial(mach),
            Some((
                "mach-lookup".to_string(),
                Denied::Door("com.apple.system.notification_center")
            ))
        );
    }

    /// The kernel layer is the only one a re-exec reaches, and its remedy is
    /// the exec set, not the fs one.
    #[test]
    fn parse_denial_calls_a_denied_spawn_an_exec() {
        let line = "kernel[0] (Sandbox) Sandbox: sh(1) deny(1) process-exec /usr/bin/security";
        assert_eq!(
            parse_denial(line),
            Some((
                "process-exec".to_string(),
                Denied::Exec("/usr/bin/security")
            ))
        );
    }

    /// The reason is quoted from the profile's own table, and only for the
    /// doors it names.
    #[test]
    fn door_reason_answers_a_withheld_door_and_withholds_it_from_a_probe() {
        let why = door_reason("com.apple.SecurityServer").expect("securityd is on record");
        assert!(why.contains("keychain"), "{why:?}");
        assert!(
            door_reason("com.apple.pasteboard.1").is_some(),
            "an instance suffix must still match its door"
        );
        assert_eq!(door_reason("com.apple.metadata.mds"), None);
    }

    #[test]
    fn parse_denial_rejects_non_denial_lines() {
        assert_eq!(parse_denial("no sandbox tag here"), None);
    }
}
