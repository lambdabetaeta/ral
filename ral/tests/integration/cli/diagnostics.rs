//! Diagnostics on stderr: batch ones carry no ANSI when stderr is not a tty,
//! and a runtime error says where it happened, even when the value that raised
//! it was compiled by an earlier run.
//!
//! A handler, a `use`d function, a lambda bound at the prompt: each is a
//! live value in the next run, carrying spans the run before it minted.  The
//! source registry only grows, so those spans still resolve — and the caret
//! is drawn wherever they point, which is the whole answer when the text is
//! not the text the user just typed.

use crate::common::{Scratch, rc_home, repl};
use std::process::Command;

/// `line` fed to a `-i` session whose config is isolated (an empty rc, or the
/// given one), with `args` ahead of it.  Returns stderr.
fn repl_stderr(rc: &str, args: &[&str], line: &str) -> String {
    let (_dir, env) = rc_home(rc);
    repl(&[args, &["-i"]].concat(), &env, line).stderr
}

/// `boom` at the prompt is one command, but the fault is inside the rc,
/// so the caret must point there rather than at the compact one-liner.
#[test]
fn an_rc_handler_faults_against_the_rc_file() {
    let rc = "[\n  handlers: [:, boom: { |args| fail 'nope' } ]\n]\n";
    let stderr = repl_stderr(rc, &[], "boom\n");
    assert!(
        stderr.contains("ral/rc:2:") && stderr.contains("fail 'nope' }"),
        "the caret must name the rc file, at the handler body's line; stderr was:\n{stderr}"
    );
}

/// A lambda bound on one prompt line and failing on a later one is drawn
/// at its definition, not at the call.
#[test]
fn a_lambda_bound_at_the_prompt_faults_at_its_definition() {
    let stderr = repl_stderr("", &["--norc"], "let boom = { |x| fail 'nope' }\nboom 1\n");
    assert!(
        stderr.contains("<stdin>:1:18") && stderr.contains("fail 'nope' }"),
        "the caret must name the definition (1:18), not the call; stderr was:\n{stderr}"
    );
}

/// A `use`d file's function, called at the prompt, names that file — not
/// the prompt line, and not nothing.
#[test]
fn a_used_function_faults_against_its_own_file() {
    let dir = Scratch::new("ral_used_fault");
    let module = dir.join("mod.ral");
    std::fs::write(&module, "let boom = { |x| fail 'nope' }\n").unwrap();

    let stderr = repl_stderr(
        "",
        &["--norc"],
        &format!("let m = use '{}'\n$m[boom] 1\n", module.display()),
    );
    assert!(
        stderr.contains("mod.ral:1:") && stderr.contains("fail 'nope' }"),
        "the caret must name the loaded file; stderr was:\n{stderr}"
    );
}

/// A bare external command that is not found is still reported.
#[test]
fn a_bare_missing_command_is_reported() {
    let stderr = repl_stderr("", &["--norc"], "no-such-command-xyz\n");
    assert!(
        stderr.contains("no-such-command-xyz: command not found"),
        "the failure must still be reported; stderr was:\n{stderr}"
    );
}

/// A `use`d module's runtime error is drawn against the module's source,
/// at the failing line, and never against the caller's text.
#[test]
fn a_used_module_runtime_error_points_into_the_module() {
    let dir = Scratch::new("ral_use_runtime_err");
    let module = dir.join("mod.ral");
    std::fs::write(&module, "let a = 1\nlet z = 0\nreturn $[$a / $z]\n").unwrap();

    let out = crate::common::ral_command()
        .arg("-c")
        .arg(format!("use '{}'", module.display()))
        .output()
        .expect("spawn ral");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("division by zero"), "stderr was:\n{stderr}");
    assert!(
        stderr.contains("mod.ral:3:"),
        "the caret must name the module's line 3; stderr was:\n{stderr}"
    );
    assert!(
        !stderr.contains("-c:1"),
        "the caller's text must not be the caret's home; stderr was:\n{stderr}"
    );
}

/// stderr of `script` run from a directory holding a `mod.ral` that fails on
/// its second line.  `{mod}` in `script` is the module's path.
fn stderr_of_failing_module(tag: &str, script: &str) -> String {
    let dir = Scratch::new(tag);
    let module = dir.join("mod.ral");
    std::fs::write(&module, "let x = 1\nfail 'inner boom'\n").unwrap();
    let out = crate::common::ral_command()
        .arg("-c")
        .arg(script.replace("{mod}", &module.display().to_string()))
        .output()
        .expect("spawn ral");
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// A file a worker `use`s after its fork is unknown to the parent that
/// awaits it: the failure crosses resolved, not as a span the parent would
/// misread or refuse.
#[test]
fn a_worker_failing_in_a_module_it_used_names_that_module() {
    let stderr = stderr_of_failing_module(
        "ral_worker_use_fault",
        "await !{spawn { use '{mod}'; return 1 }}",
    );
    assert!(
        stderr.contains("mod.ral:2:1") && !stderr.contains("panicked"),
        "the failure must name the module's line 2; stderr was:\n{stderr}"
    );
}

/// The same for a pipeline's thread stage.
#[test]
fn a_stage_failing_in_a_module_it_used_names_that_module() {
    let stderr = stderr_of_failing_module(
        "ral_stage_use_fault",
        "let f = { |_| use '{mod}'; cat }\necho x | f ()",
    );
    assert!(
        stderr.contains("mod.ral:2:1") && !stderr.contains("panicked"),
        "the failure must name the module's line 2; stderr was:\n{stderr}"
    );
}

/// Run `ral -c <code>` with the given env overrides and return (stdout, stderr).
fn run_c(code: &str, env: &[(&str, &str)]) -> (Vec<u8>, Vec<u8>) {
    let mut cmd = Command::new(crate::common::ral_bin());
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
