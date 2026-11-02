#![allow(clippy::disallowed_methods, reason = "test code")]

// Batch (`-c`) diagnostics must not carry ANSI when stderr is not a tty.

mod common;

use std::process::Command;

/// Run `ral -c <code>` with the given env overrides and return (stdout, stderr).
fn run_c(code: &str, env: &[(&str, &str)]) -> (Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(common::ral_bin());
    cmd.arg("-c").arg(code);
    // Ensure a clean slate for the vars we toggle.
    cmd.env_remove("NO_COLOR")
        .env_remove("CLICOLOR_FORCE")
        .env_remove("RAL_INTERACTIVE_MODE");
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("failed to run ral");
    (out.stdout, out.stderr)
}

fn has_ansi(bytes: &[u8]) -> bool {
    bytes.contains(&0x1b)
}

#[test]
fn piped_stderr_has_no_ansi() {
    // When stderr is not a tty (cargo test captures it via pipe),
    // diagnostics must not emit ANSI — even without NO_COLOR or TERM=dumb.
    // A syntax error guarantees diagnostic output on stderr.
    let (_out, err) = run_c("[", &[("TERM", "xterm-256color")]);
    assert!(!err.is_empty(), "expected a diagnostic on stderr");
    assert!(
        !has_ansi(&err),
        "non-tty stderr should not carry ANSI, got {err:?}",
    );
}
