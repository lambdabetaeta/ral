#![allow(clippy::disallowed_methods)]

// Integration tests for the argv surface itself: how a script's positional
// arguments reach `$ARGS` / `$SCRIPT`, and how a script piped on stdin is
// run and located in diagnostics.  The unit tests in `ral/src/cli.rs` stop
// at the parsed `Mode`; these carry it through to evaluated output.

mod common;

use common::{Output, fresh_tmp_path, ral_command};
use std::io::Write;
use std::path::Path;
use std::process::Stdio;

/// Run `ral <path> [args…]` with an empty stdin.
fn run_script(path: &Path, args: &[&str]) -> Output {
    let out = ral_command()
        .arg(path)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .expect("spawn ral");
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

/// Run `ral [args…]` with `script` piped on stdin and no script positional.
fn run_stdin(args: &[&str], script: &str) -> Output {
    let mut child = ral_command()
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

#[test]
fn script_positionals_reach_args_and_script() {
    // `$ARGS` holds the trailing positionals and nothing else — the script
    // path is `$SCRIPT`, not `$ARGS[0]` — and flag-shaped arguments are
    // forwarded rather than eaten by clap (`--version` and `-n` are both
    // real ral flags).
    let tmp = fresh_tmp_path("ral_cli_args", "ral");
    std::fs::write(
        &tmp,
        "echo !{length $ARGS}\necho ...$ARGS\necho !{basename $SCRIPT}\n",
    )
    .unwrap();
    let name = tmp.file_name().unwrap().to_string_lossy().into_owned();

    let o = run_script(&tmp, &["--version", "-n", "alpha"]);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, format!("3\n--version -n alpha\n{name}\n"));

    let o = run_script(&tmp, &[]);
    assert_eq!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, format!("0\n\n{name}\n"));

    std::fs::remove_file(&tmp).ok();
}

#[test]
fn piped_stdin_runs_as_one_script() {
    // Both the bare `cmd | ral` form and the explicit `-s` must evaluate the
    // whole stdin as one script — not line by line at a prompt, and not
    // dropping its last line.
    for args in [&[][..], &["-s"][..]] {
        let o = run_stdin(args, "let x = 2\necho $[$x + 1]\n");
        assert_eq!(o.status, 0, "args {args:?}, stderr: {}", o.stderr);
        assert_eq!(o.stdout, "3\n", "args {args:?}");
    }
}

#[test]
fn stdin_script_errors_carry_a_stdin_location() {
    // The stdin source must be registered under a real name, so a runtime
    // error can point a caret at it.
    let o = run_stdin(&["-s"], "echo hi\n$nope\n");
    assert_ne!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "hi\n");
    assert!(
        o.stderr.contains("undefined variable: $nope"),
        "stderr: {}",
        o.stderr
    );
    assert!(o.stderr.contains("<stdin>:2:1"), "stderr: {}", o.stderr);
}
