//! Type errors block evaluation, exercised at the CLI tier.
//!
//! The inference pass is unconditional — it writes the evaluator's mode
//! wires — and any type error is fatal: a value-type clash and a mode
//! mismatch alike report and refuse to evaluate.  These tests pin that
//! verdict at the binary boundary, where the in-process checker tests
//! cannot see the process exit code or the rc-skip-but-boot policy.

#![allow(clippy::disallowed_methods)]

mod common;

use std::io::Write;
use std::process::{Command, Stdio};

/// Captured output of `ral -c <code>`.
struct Run {
    stdout: String,
    stderr: String,
    status: i32,
}

/// Run `ral -c <code>` with stdin from /dev/null.
fn run_c(code: &str) -> Run {
    let child = Command::new(common::ral_bin())
        .arg("-c")
        .arg(code)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    let out = child.wait_with_output().unwrap();
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// A value-type clash blocks evaluation: the process exits nonzero and the
/// error renders as an Error.
#[test]
fn value_type_error_blocks() {
    let r = run_c("let x = hello\nreturn $[$x + 1]");
    assert_ne!(r.status, 0, "a value-type error must block");
    assert!(
        r.stderr.contains("Error") && r.stderr.contains("couldn't match"),
        "the type clash must render as an Error; stderr was:\n{}",
        r.stderr
    );
}

/// A `∅`-into-`Bytes` pipeline adjacency (`echo foo | length`) is the mode
/// fragment: it is fatal, since the evaluator has no runnable wires for it.
#[test]
fn mode_error_blocks() {
    let r = run_c("echo foo | length");
    assert_ne!(r.status, 0, "a mode error must block");
    assert!(
        r.stderr.contains("T0012") && r.stderr.contains("Error"),
        "a mode error must render as an Error (T0012); stderr was:\n{}",
        r.stderr
    );
}

/// A clean pipeline runs with wires: the pipeline connects byte stage to
/// byte stage, so it works.
// Unix-only: drives the pipeline through the absolute external
// `/bin/cat`, which does not exist on Windows.
#[cfg(unix)]
#[test]
fn clean_pipeline_runs_with_wires() {
    let r = run_c("echo hi | /bin/cat");
    assert_eq!(
        r.status, 0,
        "a clean pipeline must run; stderr was:\n{}",
        r.stderr
    );
    assert_eq!(r.stdout, "hi\n", "the pipeline must produce its output");
}

// ── rc files at boot ──────────────────────────────────────────────────────
//
// An rc check error is reported and the boot survives — never fatal.  The
// rc file is found via `$XDG_CONFIG_HOME/ral/rc`; isolating both
// `XDG_CONFIG_HOME` and `HOME` to a temp dir keeps the test off the
// developer's real rc and history.  The shell boots interactive (`-i`) with
// a one-line piped stdin that runs after rc sourcing, so its output reports
// what the rc left behind.

/// Boot `ral -i` with `rc_body` written to the isolated rc file and `line`
/// fed as the single REPL turn after rc sourcing.
fn boot_with_rc(rc_body: &str, line: &str) -> Run {
    let dir = common::fresh_tmp_path("ral_rc_home", "d");
    std::fs::create_dir_all(dir.join("ral")).unwrap();
    std::fs::write(dir.join("ral").join("rc"), rc_body).unwrap();

    let mut child = Command::new(common::ral_bin())
        .arg("-i")
        .env("HOME", &dir)
        .env("XDG_CONFIG_HOME", &dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(line.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    std::fs::remove_dir_all(&dir).ok();
    Run {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// An rc file with a value-type error is skipped — the error is fatal to the
/// file, so `marker` never installs — but the shell still boots (the next
/// turn runs).
#[test]
fn rc_value_type_error_skips_file_but_boots() {
    let rc = "let marker = 'applied'\nlet bad = [a: 1]\nreturn $bad[b]\n";
    let r = boot_with_rc(rc, "return marker-absent\n");
    assert!(
        r.stderr.contains("skipped due to type errors"),
        "the rc file must be skipped; stderr was:\n{}",
        r.stderr
    );
    assert!(
        !r.stdout.contains("applied"),
        "a skipped rc file must not install its bindings; stdout was:\n{}",
        r.stdout
    );
    // The shell still booted: the piped turn ran.
    assert!(
        r.stdout.contains("marker-absent"),
        "the shell must boot even when the rc file is skipped; stdout was:\n{}",
        r.stdout
    );
}

/// An rc file with a *mode* error is skipped (it has no runnable wires) but
/// the shell boots.
#[test]
fn rc_mode_error_skips_file_but_boots() {
    let rc = "let marker = 'applied'\necho foo | length\n";
    let r = boot_with_rc(rc, "return booted\n");
    assert!(
        r.stderr.contains("T0012") && r.stderr.contains("skipped due to type errors"),
        "an rc mode error must skip the file; stderr was:\n{}",
        r.stderr
    );
    assert!(
        r.stdout.contains("booted"),
        "the shell must boot despite the rc mode error; stdout was:\n{}",
        r.stdout
    );
}
