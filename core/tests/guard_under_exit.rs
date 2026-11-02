#![allow(clippy::disallowed_methods, reason = "test code")]

//! `guard` under an `exit`: the cleanup runs, the exit follows, and a failing
//! cleanup pre-empts the exit.

mod common;

use common::{fresh_shell, top_level};

use ral_core::shell::runtime::{Break, Escape};

/// A cleanup runs under an `exit`, and the `exit` follows it.
#[test]
fn a_guard_cleanup_runs_under_an_exit_and_the_exit_follows() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mark = dir.path().join("cleaned");
    let src = format!(
        "guard {{ exit 3 }} {{ echo cleaned > '{}' }}",
        mark.display()
    );
    match top_level(&mut fresh_shell(), &src) {
        Err(Break::Escape(Escape::Exit(3))) => {}
        other => panic!("expected exit 3, got {other:?}"),
    }
    assert!(mark.exists(), "the cleanup ran");
}

/// A cleanup's own failure pre-empts the halt it ran under.
#[test]
fn a_cleanups_fail_pre_empts_an_exit() {
    match top_level(
        &mut fresh_shell(),
        "guard { exit 3 } { fail [status: 7, message: x] }",
    ) {
        Err(Break::Error(e)) => assert_eq!(e.code(), 7),
        other => panic!("expected an error with code 7, got {other:?}"),
    }
}
