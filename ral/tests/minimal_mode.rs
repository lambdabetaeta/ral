// Regression tests for the capability-gated interactive frontend.
//
// Covers the non-REPL paths (NO_COLOR, TERM=dumb) that produce visible
// output via `diagnostic::use_color`.  The REPL-side gating of the
// highlighter / hinter / CPR is covered by unit tests in core/src/io.rs
// on `TerminalState`; driving a full PTY from a cargo test is possible
// but fragile, so those paths are exercised manually.  See
// TODO(interactive) below.

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
fn no_color_strips_ansi_from_error() {
    // A syntax error guarantees diagnostic output on stderr.
    let (_out, err) = run_c("[", &[("NO_COLOR", "1"), ("TERM", "xterm-256color")]);
    assert!(!err.is_empty(), "expected a diagnostic on stderr");
    assert!(
        !has_ansi(&err),
        "NO_COLOR=1 should suppress ANSI in diagnostics, got {err:?}",
    );
}

#[test]
fn term_dumb_strips_ansi_from_error() {
    let (_out, err) = run_c("[", &[("TERM", "dumb")]);
    assert!(!err.is_empty(), "expected a diagnostic on stderr");
    assert!(
        !has_ansi(&err),
        "TERM=dumb should suppress ANSI in diagnostics, got {err:?}",
    );
}

#[test]
fn piped_stderr_has_no_ansi() {
    // When stderr is not a tty (cargo test captures it via pipe),
    // diagnostics must not emit ANSI — even without NO_COLOR or TERM=dumb.
    let (_out, err) = run_c("[", &[("TERM", "xterm-256color")]);
    assert!(!err.is_empty(), "expected a diagnostic on stderr");
    assert!(
        !has_ansi(&err),
        "non-tty stderr should not carry ANSI, got {err:?}",
    );
}

// TODO(interactive): add regression tests for RAL_INTERACTIVE_MODE=minimal
// driven through a real PTY (no CPR ESC[6n emitted, no highlight ANSI during
// editing) once we settle on a PTY dependency for the test harness.
