//! macOS Seatbelt denial reader.
//!
//! Seatbelt denials surface to user code as raw `EPERM`; the denied
//! path and operation appear only in the unified system log, under
//! `(Sandbox) Sandbox: <proc>(<pid>) deny(...) <op> <path>`.  We read
//! the relevant slice via `/usr/bin/log show --last <N>s --predicate
//! 'eventMessage BEGINSWITH "Sandbox: "'` and pass each kernel-format
//! line through verbatim; the lines are self-describing (timestamp,
//! process, PID, operation, path) so no further work is needed beyond
//! extracting the PID for descendant attribution.

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
