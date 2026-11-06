#![allow(clippy::disallowed_methods, reason = "test code")]
#![cfg(unix)]

mod common;

use common::{Output, diagnostics, ral_command, wait_with_timeout};
use std::process::Stdio;
use std::time::{Duration, Instant};

fn run(code: &str) -> Output {
    let child = ral_command()
        .args(["-c", code])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn ral");
    wait_with_timeout(child, Duration::from_secs(20)).expect("script hung")
}

#[test]
fn a_returning_script_waits_for_its_loose_worker() {
    let out = run("let h = spawn \"late\" { ^sleep 1; echo late }; echo end");
    assert_eq!(out.status, 0, "{out:?}");
    assert_eq!(out.stdout, "end\nlate\n");
    assert_eq!(diagnostics(&out.stderr), "", "{out:?}");
}

#[test]
fn a_failing_loose_worker_leaves_no_trace() {
    let out = run("let h = spawn \"fails\" { ^false }; echo end");
    assert_eq!(out.status, 0, "{out:?}");
    assert_eq!(out.stdout, "end\n");
    assert_eq!(diagnostics(&out.stderr), "", "{out:?}");
}

#[test]
fn an_exit_takes_a_running_loose_worker_down() {
    let start = Instant::now();
    let out = run("let h = spawn \"t\" { ^sleep 30; echo never }; exit 3");
    assert!(start.elapsed() < Duration::from_secs(10));
    assert_eq!(out.status, 3, "{out:?}");
    assert_eq!(out.stdout, "");
    assert_eq!(diagnostics(&out.stderr), "", "{out:?}");
}
