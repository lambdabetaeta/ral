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
use std::time::Duration;

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

// ── batch honesty: `--check` sees exactly what `run` will run ────────────
//
// `--check`'s seed table is core plus `WATCH_BUILTIN` — exactly what the
// batch shell installs (`ral/src/batch.rs`), never the REPL-only editor
// surface (`ral::repl::plugin::ed_builtins::ED_BUILTINS`).  So `_ed-insert`
// resolves as an external command to the checker, not the real builtin's
// `String → F[∅] Unit` scheme: piping its result into `from-json` (whose
// input mode is ground `Bytes`) only typechecks under the external
// reading.  `--check` and a real `run` must therefore agree — both treat
// `_ed-insert` as unknown, and `run` fails only where an external
// resolution actually fails, at the PATH lookup, never with a static type
// error `--check` would have caught first.

const ED_INSERT_PIPED_TO_FROM_JSON: &str = r#"_ed-insert "hi" | from-json"#;

/// `--check` typechecks `_ed-insert` as external, not as the REPL's `_ed-*`
/// scheme — the mode that would clash with `from-json`'s ground `Bytes`
/// input never gets a chance to.
#[test]
fn check_typechecks_ed_insert_as_external_in_batch() {
    let r = common::run_with_timeout(
        "batch_check_ed_insert",
        &["--check"],
        ED_INSERT_PIPED_TO_FROM_JSON,
        Duration::from_secs(10),
    )
    .expect("ral --check must not hang");
    assert_eq!(
        r.status, 0,
        "expected `--check` to typecheck _ed-insert as an external command; stderr was:\n{}",
        r.stderr
    );
}

/// A real `run` of the same script agrees with `--check`: it clears
/// typechecking (no type error ever reaches stderr) and fails only at the
/// external-exec boundary, because the batch shell never installed
/// `_ed-insert` either.
#[test]
fn run_agrees_ed_insert_is_external_in_batch() {
    let r = common::run_with_timeout(
        "batch_run_ed_insert",
        &[],
        ED_INSERT_PIPED_TO_FROM_JSON,
        Duration::from_secs(10),
    )
    .expect("ral must not hang");
    assert_ne!(r.status, 0, "an unresolvable external command must fail");
    assert!(
        r.stderr.contains("_ed-insert: command not found"),
        "expected an external-exec failure, not a type error; stderr was:\n{}",
        r.stderr
    );
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
/// fed as the single REPL run after rc sourcing.
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
/// run happens).
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
    // The shell still booted: the piped run completed.
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
