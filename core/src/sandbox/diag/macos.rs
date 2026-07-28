//! macOS Seatbelt denial reader — the `platform` module `sandbox::diag`
//! selects here, mirroring `diag/linux.rs`.
//!
//! A denial reaches user code as a bare `EPERM`; only the unified system log
//! names the operation and path, in the shape
//! `(Sandbox) Sandbox: <comm>(<pid>) deny(<n>) <op> <operand…>` that every
//! parser below keys off.

use std::time::Duration;

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:log-show] macOS sandbox diagnostics: shells out to `/usr/bin/log show` to read Seatbelt denial records from the unified log; a diagnostic probe, not turn-time model data I/O, raises no surface card."
)]
pub(super) fn read_window(elapsed: Duration) -> Option<String> {
    // `--last` rounds to whole seconds, so pad by one or a sub-second call
    // asks for a zero-length window and sees nothing.
    let secs = elapsed.as_secs().saturating_add(1).to_string();
    let last_arg = format!("{secs}s");
    let output = std::process::Command::new("/usr/bin/log")
        .args([
            "show",
            "--predicate",
            "eventMessage BEGINSWITH \"Sandbox: \"",
            "--last",
            last_arg.as_str(),
            "--style",
            "compact",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
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

/// Split a denial into `(operation, path)`.  The operand is the whole trimmed
/// remainder, since macOS paths contain spaces, and counts as a path only for
/// `file-*`: an `ipc-*`/`mach-*`/`network-*` operand names a service, which
/// `build_hint` must never offer the user as a path to grant.  Seatbelt
/// resolves the filesystem paths it logs, so the path returned is exactly the
/// one the grant needs.
pub(super) fn parse_denial(line: &str) -> Option<(&str, Option<&str>)> {
    let after_tag = line.split_once("Sandbox: ")?.1;
    let after_deny = after_tag.split_once("deny(")?.1.split_once(')')?.1;
    let mut rest = after_deny.trim_start().splitn(2, char::is_whitespace);
    let op = rest.next()?;
    if op.is_empty() {
        return None;
    }
    let path = rest
        .next()
        .map(str::trim)
        .filter(|p| !p.is_empty() && op.starts_with("file-"));
    Some((op, path))
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
                "file-read-data",
                Some("/private/var/folders/ab/secret/data.txt")
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
                "file-read-data",
                Some("/Users/me/My Documents/the file.txt")
            ))
        );
    }

    #[test]
    fn parse_denial_handles_op_without_a_path() {
        let line = "kernel[0] (Sandbox) Sandbox: foo(1) deny(1) network-outbound";
        assert_eq!(parse_denial(line), Some(("network-outbound", None)));
    }

    #[test]
    fn parse_denial_drops_non_filesystem_operand() {
        let shm = "kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                   ipc-posix-shm-read-data apple.shm.notification_center";
        assert_eq!(parse_denial(shm), Some(("ipc-posix-shm-read-data", None)));

        let mach = "kernel[0] (Sandbox) Sandbox: git(58522) deny(1) \
                    mach-lookup com.apple.system.notification_center";
        assert_eq!(parse_denial(mach), Some(("mach-lookup", None)));
    }

    #[test]
    fn parse_denial_rejects_non_denial_lines() {
        assert_eq!(parse_denial("no sandbox tag here"), None);
    }
}
