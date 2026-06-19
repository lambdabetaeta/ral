//! macOS Seatbelt denial reader.
//!
//! Seatbelt denials surface to user code as raw `EPERM`; the denied
//! path and operation appear only in the unified system log, under
//! `(Sandbox) Sandbox: <proc>(<pid>) deny(...) <op> <path>`.  We read
//! the relevant slice via `/usr/bin/log show --last <N>s --predicate
//! 'eventMessage BEGINSWITH "Sandbox: "'`.  The lines are self-describing
//! (timestamp, process, PID, operation, path); [`extract_pid`] reads the
//! PID for descendant attribution and [`parse_denial`] splits out the
//! operation and the fully-resolved path Seatbelt logs.

use std::time::Duration;

pub(super) fn read_window(elapsed: Duration) -> Option<String> {
    // `log show --last <N>s` is whole-second granularity; pad by one
    // second so a sub-second call still catches its denials.
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

/// `... (Sandbox) Sandbox: cargo(12345) deny(1) file-read-data /…`
/// → `Some(12345)`.  Returns `None` on any deviation from that shape.
pub(super) fn extract_pid(line: &str) -> Option<u32> {
    let after_tag = line.rsplit_once("Sandbox: ")?.1;
    let (_, after_open) = after_tag.split_once('(')?;
    let (digits, _) = after_open.split_once(')')?;
    digits.parse().ok()
}

/// Split a denial line into its `(operation, path)` after the
/// `Sandbox: ` tag, where the body reads `comm(pid) deny(n) <op>
/// <path…>`.  The operation is the whitespace token following the
/// `deny(...)` token; the path is the trimmed remainder of the line —
/// macOS paths can contain spaces, so the remainder is taken whole and
/// never split further.  The path is logged fully resolved, so it is
/// exactly the path the user must grant.  Returns `None` on any
/// deviation from that shape; a parsed line with no path tail yields
/// `Some((op, None))`.
pub(super) fn parse_denial(line: &str) -> Option<(&str, Option<&str>)> {
    let after_tag = line.rsplit_once("Sandbox: ")?.1;
    // Skip the `comm(pid)` token, then the `deny(n)` token, landing on
    // the operation.  `splitn` keeps the path tail (with its spaces)
    // intact as the final piece.
    let after_deny = after_tag.split_once("deny(")?.1.split_once(')')?.1;
    let mut rest = after_deny.trim_start().splitn(2, char::is_whitespace);
    let op = rest.next()?;
    if op.is_empty() {
        return None;
    }
    let path = rest.next().map(str::trim).filter(|p| !p.is_empty());
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
    fn parse_denial_rejects_non_denial_lines() {
        assert_eq!(parse_denial("no sandbox tag here"), None);
    }
}
