//! The binary actually running: `decide` is unit-tested in `main.rs`, but
//! only here does `dispatch` read a real argv0, exec a real `/bin/sh`, and
//! hand it the arguments it was given.
//!
//! Only the POSIX side: the `ral` target execs a sibling binary, and a
//! bare, tty-less invocation routes to `/bin/sh` anyway.  `HOME` is a fresh
//! empty directory throughout so no `~/.profile` a login shell sources can
//! write to the stdout being asserted on.

#![cfg(unix)]
// A test binary, not the ral shell: the clippy.toml invariants target
// ral-core's fs and process discipline, not a harness that spawns one.
#![allow(clippy::disallowed_methods)]

use std::os::unix::process::CommandExt;
use std::process::{Command, Output};

const BIN: &str = env!("CARGO_BIN_EXE_ral-sh");

/// `ral-sh` invoked with `argv0`, in a home directory holding nothing.
fn run(argv0: &str, args: &[&str]) -> Output {
    let home = std::env::temp_dir().join(format!("ral-sh-dispatch-{}", std::process::id()));
    std::fs::create_dir_all(&home).expect("an empty home");
    Command::new(BIN)
        .arg0(argv0)
        .args(args)
        .env("HOME", &home)
        .output()
        .expect("ral-sh runs")
}

/// `$SHELL -c 'scp …'` is the binary's whole reason to exist: the operand
/// must reach `/bin/sh` intact, not start an interactive shell.
#[test]
fn a_command_string_reaches_posix_sh_with_its_operand_intact() {
    let out = run("ral-sh", &["-c", "echo hi"]);
    assert!(out.status.success(), "got {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout), "hi\n");
}

/// The login convention survives the exec, so a chsh'd user's `/bin/sh`
/// still sources its login profile.
#[test]
fn a_login_invocation_hands_posix_sh_the_dash() {
    let out = run("-ral-sh", &["-c", r#"printf %s "$0""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "-sh");
}

/// And the dash is conditional: an ordinary invocation never invents one.
#[test]
fn an_ordinary_invocation_leaves_the_dash_off() {
    let out = run("ral-sh", &["-c", r#"printf %s "$0""#]);
    assert_eq!(String::from_utf8_lossy(&out.stdout), "/bin/sh");
}
