// A batch script that ends while a worker's external command is still running
// must take that command down with it — grandchildren included — rather than
// leaving it reparented to PID 1 to run to completion.
//
// The child must be *established* before the host exits: a script that exits
// immediately after spawning races the spawn itself and looks clean whatever
// the teardown does, which is why every script here sleeps first.

#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // fixture and pid-file scaffolding, not a door

mod common;

use common::{fresh_tmp_path, run};
use std::time::{Duration, Instant};

/// Is `pid` still a live process?  Signal 0 is the existence probe; the orphan
/// we are looking for is init's child by then, never ours, so no zombie can
/// answer in its place.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Run `watch`ed external work whose tree — a shell and a background `sleep`
/// under it — outlasts any test, then `tail` (the rest of the script), and
/// assert nothing of that tree is left once `ral` has exited.
fn nothing_survives(tail: &str) {
    let pidfile = fresh_tmp_path("ral_worker_teardown", "pids");
    let fixture = fresh_tmp_path("ral_worker_teardown", "sh");
    std::fs::write(
        &fixture,
        format!(
            "#!/bin/sh\nsleep 300 &\necho \"$$ $!\" > {}\nwait\n",
            pidfile.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(
        &fixture,
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();

    let out = run(
        "ral_worker_teardown",
        &format!(
            "let job = watch \"tests\" {{ {} ; return `done }}\nsleep 1\n{tail}\n",
            fixture.display()
        ),
    );
    assert_eq!(out.status, 0, "stderr: {}", out.stderr);

    let recorded = std::fs::read_to_string(&pidfile).expect("the worker's child must have run");
    let pids: Vec<i32> = recorded
        .split_whitespace()
        .map(|p| p.parse().expect("a pid"))
        .collect();
    let _ = std::fs::remove_file(&fixture);
    let _ = std::fs::remove_file(&pidfile);

    // The drain runs before the host exits, so the tree is already gone; the
    // margin is for a loaded machine's scheduling, not for a second teardown.
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && pids.iter().copied().any(alive) {
        std::thread::sleep(Duration::from_millis(20));
    }
    let survivors: Vec<i32> = pids.iter().copied().filter(|p| alive(*p)).collect();
    for pid in &survivors {
        unsafe { libc::kill(*pid, libc::SIGKILL) };
    }
    assert!(
        survivors.is_empty(),
        "the host exited and left {survivors:?} running"
    );
}

/// Falling off the end of the script is the plain case: the session's shell
/// drops with the worker still live.
#[test]
fn a_workers_external_child_dies_with_the_batch_host() {
    nothing_survives("echo hi");
}

/// `cancel` is the sharper one: it takes the entry out of the registry the
/// instant it signals, so the roster is empty by the time the host tears down
/// and only the worker thread itself still attests to a child being killed.
#[test]
fn a_cancel_the_host_immediately_outruns_still_reaps_the_child() {
    nothing_survives("cancel $job");
}
