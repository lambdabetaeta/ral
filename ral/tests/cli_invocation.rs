#![allow(clippy::disallowed_methods)]

// Integration tests for the argv surface itself: how a script's positional
// arguments reach `args` / `$SCRIPT`, and how a script piped on stdin is
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
fn run_c(code: &str) -> Output {
    let out = ral_command()
        .arg("-c")
        .arg(code)
        .stdin(Stdio::null())
        .output()
        .expect("spawn ral");
    Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

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
    // `args` holds the trailing positionals and nothing else — the script
    // path is `$SCRIPT`, not `!{args}[0]` — and flag-shaped arguments are
    // forwarded rather than eaten by clap (`--version` and `-n` are both
    // real ral flags).
    let tmp = fresh_tmp_path("ral_cli_args", "ral");
    std::fs::write(
        &tmp,
        "echo !{length !{args}}\necho ...!{args}\necho !{basename $SCRIPT}\n",
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
    let o = run_stdin(&["-s"], "echo hi\nfail [status: 1, message: nope]\n");
    assert_ne!(o.status, 0, "stderr: {}", o.stderr);
    assert_eq!(o.stdout, "hi\n");
    assert!(o.stderr.contains("nope"), "stderr: {}", o.stderr);
    assert!(o.stderr.contains("<stdin>:2:1"), "stderr: {}", o.stderr);
}

#[test]
fn exit_escapes_try_and_guard_cleanup_with_its_own_code() {
    let o = run_c("try { exit 7 } { |_e| return () }");
    assert_eq!(
        o.status, 7,
        "`try` must not swallow `exit`; stderr: {}",
        o.stderr
    );

    let o = run_c("guard { echo body } { exit 5 }");
    assert_eq!(
        o.status, 5,
        "a `guard` cleanup's `exit` must escape; stderr: {}",
        o.stderr
    );
}

#[test]
fn a_file_descriptor_prefix_on_input_redirects_is_a_parse_error() {
    for (source, message) in [
        ("audit { /bin/cat 1< f }", "always feeds standard input"),
        ("audit { /bin/cat 2< f }", "always feeds standard input"),
        ("/bin/cat 1< f", "always feeds standard input"),
        ("/bin/echo hi 0> f", "standard input cannot be written to"),
        ("/bin/echo hi 0>> f", "standard input cannot be written to"),
        ("/bin/echo hi 0>~ f", "standard input cannot be written to"),
        ("/bin/cat 1<< body", "drop the file-descriptor prefix"),
    ] {
        let o = run_c(source);
        assert_ne!(o.status, 0, "{source:?} must not run");
        assert!(o.stderr.contains(message), "{source:?} gave {}", o.stderr);
    }
}

// ── Bytes that are not text ────────────────────────────────────────────────

/// Run `ral -c <code> [extra…]` under `envs`, with an empty stdin.
#[cfg(unix)]
fn run_c_os(
    code: &str,
    extra: &[&std::ffi::OsStr],
    envs: &[(&str, &std::ffi::OsStr)],
) -> std::process::Output {
    let mut cmd = ral_command();
    cmd.arg("-c").arg(code).args(extra).stdin(Stdio::null());
    for (key, val) in envs {
        cmd.env(key, val);
    }
    cmd.output().expect("spawn ral")
}

/// `env` reads past a variable that is not UTF-8, leaving it out rather than
/// panicking or mangling it.
#[cfg(unix)]
#[test]
fn env_leaves_out_a_variable_that_is_not_utf8() {
    use std::os::unix::ffi::OsStrExt as _;
    let out = run_c_os(
        "echo !{has !{env} RAL_TEST_BYTES} !{has !{env} PATH}",
        &[],
        &[("RAL_TEST_BYTES", std::ffi::OsStr::from_bytes(b"\xff\xfe"))],
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "false true\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// A default fills only an absent variable: one the host binds in bytes that
/// are not UTF-8 reaches a child exactly as inherited.
#[cfg(unix)]
#[test]
fn a_variable_that_is_not_utf8_reaches_children_untouched() {
    use std::os::unix::ffi::OsStrExt as _;
    let out = run_c_os(
        "/usr/bin/printenv LANG",
        &[],
        &[("LANG", std::ffi::OsStr::from_bytes(b"C.\xff"))],
    );
    assert_eq!(
        out.stdout,
        b"C.\xff\n",
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// An argument that is not UTF-8 is refused, never mangled into one that is.
#[cfg(unix)]
#[test]
fn an_argument_that_is_not_utf8_is_refused() {
    use std::os::unix::ffi::OsStrExt as _;
    let out = run_c_os("echo ran", &[std::ffi::OsStr::from_bytes(b"caf\xe9")], &[]);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(2), "stderr: {stderr}");
    assert!(out.stdout.is_empty(), "nothing may run");
    assert!(
        stderr.contains("argument 3 is not UTF-8 text"),
        "the refusal names the argument: {stderr}"
    );
}
