#![allow(clippy::disallowed_methods)]

//! Integration tests for `ral --capabilities a.ral[,b.ral...]`.
//!
//! The flag loads each `.ral` capability profile, left-to-right `meet`s
//! them, freezes once, and pushes the result as a permanent session
//! frame above `Capabilities::root()`.  Verified end-to-end by spawning
//! the built binary against tempfile profiles.

mod common;

use std::process::{Command, Stdio};

fn ral(args: &[&str]) -> common::Output {
    let child = Command::new(common::ral_bin())
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    let out = child.wait_with_output().unwrap();
    common::Output {
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        status: out.status.code().unwrap_or(1),
    }
}

fn write_profile(suffix: &str, body: &str) -> std::path::PathBuf {
    let path = common::fresh_tmp_path(&format!("caps_{suffix}"), "ral");
    std::fs::write(&path, body).unwrap();
    path
}

#[test]
fn single_file_deny_blocks_command() {
    let prof = write_profile("single_deny", "return [exec: [ls: 'deny']]\n");
    let out = ral(&["--capabilities", prof.to_str().unwrap(), "-c", "ls /tmp"]);
    std::fs::remove_file(&prof).ok();
    assert_ne!(
        out.status, 0,
        "ls should be denied; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stderr.contains("denied by active grant") || out.stderr.contains("'ls'"),
        "expected grant-denied diagnostic; got:\n{}",
        out.stderr
    );
}

#[test]
fn missing_profile_file_errors_clean() {
    let out = ral(&[
        "--capabilities",
        "/no/such/profile/exists.ral",
        "-c",
        "echo never",
    ]);
    assert_ne!(out.status, 0);
    assert!(
        out.stderr.contains("file not found") || out.stderr.contains("does not exist"),
        "expected file-not-found error; got:\n{}",
        out.stderr
    );
}

/// Two profiles compose by left-to-right meet — both denies survive.
/// The user can layer narrower restrictions across multiple files
/// without one file having to know about the other's vetoes.
///
/// Both denied commands here resolve as external (`arm=External` in
/// ral's dispatch trace) — `echo` is a ral builtin so an `'echo': 'deny'`
/// would do nothing; we use `ls` and `cat` which both fall through to
/// the system PATH.
#[test]
fn comma_separated_profiles_meet_both_denies() {
    let a = write_profile("meet_a", "return [exec: [ls: 'deny']]\n");
    let b = write_profile("meet_b", "return [exec: [cat: 'deny']]\n");
    let arg = format!("{},{}", a.display(), b.display());

    let out_ls = ral(&["--capabilities", &arg, "-c", "ls /tmp"]);
    assert_ne!(out_ls.status, 0, "ls denied by file a should fire");

    let out_cat = ral(&["--capabilities", &arg, "-c", "cat /etc/hostname"]);
    assert_ne!(out_cat.status, 0, "cat denied by file b should fire");

    std::fs::remove_file(&a).ok();
    std::fs::remove_file(&b).ok();
}

/// Without `--capabilities`, ambient root authority lets a normal
/// command through — pins the negative case so a regression making the
/// flag load even when absent gets caught.
#[test]
fn no_flag_leaves_session_unrestricted() {
    let out = ral(&["-c", "echo hello"]);
    assert_eq!(
        out.status, 0,
        "echo without --capabilities should succeed; stderr:\n{}",
        out.stderr
    );
    assert!(
        out.stdout.contains("hello"),
        "stdout missing 'hello':\n{}",
        out.stdout
    );
}
