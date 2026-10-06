//! The `ral` binary under the name `ral-sh`: the arguments reach `/bin/sh` or
//! ral as `bridge.rs` decides, with a real argv0 and a real exec. `HOME` is a
//! fresh empty directory throughout so no profile a login shell sources can
//! write to the stdout being asserted on.

#![cfg(unix)]
// A test binary, not the ral shell: the clippy.toml invariants target
// ral-core's fs and process discipline, not a harness that spawns one.
#![allow(clippy::disallowed_methods)]

use std::io::Write as _;
use std::os::unix::process::CommandExt as _;
use std::process::{Command, Output, Stdio};

const BIN: &str = env!("CARGO_BIN_EXE_ral");

/// `ral` invoked as `argv0` with `stdin` piped in, in a home holding nothing.
fn run(argv0: &str, args: &[&str], stdin: &str) -> Output {
    let home = tempfile::tempdir().expect("an empty home");
    let mut child = Command::new(BIN)
        .arg0(argv0)
        .args(args)
        .env("HOME", home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("ral runs");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(stdin.as_bytes())
        .expect("stdin is written");
    child.wait_with_output().expect("ral exits")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// `$SHELL -c 'scp …'` is the bridge's whole reason to exist: the operand
/// must reach `/bin/sh` intact, not start an interactive shell.
#[test]
fn a_command_string_reaches_posix_sh_with_its_operand_intact() {
    let out = run("ral-sh", &["-c", "echo hi"], "");
    assert!(out.status.success(), "got {out:?}");
    assert_eq!(stdout(&out), "hi\n");
}

/// The login convention survives the exec, so a chsh'd user's `/bin/sh`
/// still sources its login profile.
#[test]
fn a_login_invocation_hands_posix_sh_the_dash() {
    let out = run("-ral-sh", &["-c", r#"printf %s "$0""#], "");
    assert_eq!(stdout(&out), "-sh");
}

/// And the dash is conditional: an ordinary invocation never invents one.
#[test]
fn an_ordinary_invocation_leaves_the_dash_off() {
    let out = run("ral-sh", &["-c", r#"printf %s "$0""#], "");
    assert_eq!(stdout(&out), "/bin/sh");
}

/// `-l` alone is an interactive-session request, so ral reads the piped
/// source; `/bin/sh` would reject `$[`.
#[test]
fn a_bare_login_flag_reaches_ral() {
    let out = run("ral-sh", &["-l"], "echo $[2 + 2]\n");
    assert!(out.status.success(), "got {out:?}");
    assert_eq!(stdout(&out), "4\n");
}

/// `-l <script>` is a login `/bin/sh` running a script, not a ral session.
#[test]
fn a_login_flag_with_a_script_reaches_posix_sh() {
    let dir = tempfile::tempdir().expect("a script directory");
    let script = dir.path().join("two.sh");
    std::fs::write(&script, "echo $((1+1))\n").expect("the script is written");
    let out = run("ral-sh", &["-l", script.to_str().expect("UTF-8 path")], "");
    assert_eq!(stdout(&out), "2\n");
}

/// Bare with no terminal is a POSIX tool piping source in.
#[test]
fn a_bare_piped_invocation_reaches_posix_sh() {
    let out = run("ral-sh", &[], "echo $((1+1))\n");
    assert_eq!(stdout(&out), "2\n");
}

/// Without the name the bridge does not engage: `-c` is ral source.
#[test]
fn without_the_name_a_command_string_is_ral_source() {
    let out = run("ral", &["-c", "echo $[2 + 2]"], "");
    assert!(out.status.success(), "got {out:?}");
    assert_eq!(stdout(&out), "4\n");
}
