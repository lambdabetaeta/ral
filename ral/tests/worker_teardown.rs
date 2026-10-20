// A batch script that ends while a worker's external command is still running
// must take that command down with it — grandchildren included — rather than
// leaving it reparented to PID 1 to run to completion.
//
// The child must be *established* before the host exits: a host that exits
// while the spawn is still in flight looks clean whatever the teardown does.
// So establishment is a handshake and not an interval — the fixture opens the
// gate only once its tree is up and its pids are published, and the host
// blocks on the gate until then.  Nothing here may wait a fixed span instead:
// under CPU contention a freshly `exec`ed `/bin/sh` can sit a whole second
// before its first instruction, which is long enough for any wall-clock guard
// to expire while the tree it was guarding does not yet exist.

#![cfg(unix)]
#![allow(clippy::disallowed_methods)] // fixture and pid-file scaffolding, not a door

mod common;

use common::{Output, fresh_tmp_path, run_with_timeout};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Generous: it bounds a hang, and only a fixture that never reaches the gate
/// at all can reach it.
const GATE_TIMEOUT: Duration = Duration::from_secs(30);

/// Is `pid` still a live process?  Signal 0 is the existence probe; the orphan
/// we are looking for is init's child by then, never ours, so no zombie can
/// answer in its place.
fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Create the rendezvous the host blocks on.  A named pipe and not a file the
/// host polls: opening one end waits for the other, so neither side can miss
/// the moment the other arrives.
fn mkfifo(path: &Path) {
    use std::os::unix::ffi::OsStrExt;
    let raw = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert!(
        unsafe { libc::mkfifo(raw.as_ptr(), 0o600) } == 0,
        "mkfifo {}: {}",
        path.display(),
        std::io::Error::last_os_error()
    );
}

/// External work whose tree — a shell and a background `sleep` under it —
/// outlasts any test.  The fixture publishes both pids, then opens the gate.
struct Tree {
    pidfile: PathBuf,
    gate: PathBuf,
    fixture: PathBuf,
}

impl Tree {
    fn new() -> Self {
        let tree = Self {
            pidfile: fresh_tmp_path("ral_worker_teardown", "pids"),
            gate: fresh_tmp_path("ral_worker_teardown", "gate"),
            fixture: fresh_tmp_path("ral_worker_teardown", "sh"),
        };
        mkfifo(&tree.gate);
        std::fs::write(
            &tree.fixture,
            format!(
                "#!/bin/sh\nsleep 300 &\necho \"$$ $!\" > {}\n: > {}\nwait\n",
                tree.pidfile.display(),
                tree.gate.display()
            ),
        )
        .unwrap();
        std::fs::set_permissions(
            &tree.fixture,
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        tree
    }

    /// Assert `ral` exited cleanly with `out` and left nothing of the tree.
    fn assert_gone(self, out: Option<Output>) -> Output {
        // Read before removing, and remove whatever the run reached, so a
        // failure below leaves no fixture, gate, or pid file behind for the
        // next run.
        let recorded = std::fs::read_to_string(&self.pidfile);
        let _ = std::fs::remove_file(&self.fixture);
        let _ = std::fs::remove_file(&self.gate);
        let _ = std::fs::remove_file(&self.pidfile);
        let pids: Vec<i32> = recorded
            .iter()
            .flat_map(|text| text.split_whitespace())
            .map(|p| p.parse().expect("a pid"))
            .collect();
        // Nothing below can leave the tree running, whichever assertion fires.
        let sweep = || {
            for pid in &pids {
                unsafe { libc::kill(*pid, libc::SIGKILL) };
            }
        };

        let Some(out) = out else {
            sweep();
            panic!("ral never exited: the worker's child never opened the gate");
        };
        if out.status != 0 {
            sweep();
            panic!("ral exited {}, stderr: {}", out.status, out.stderr);
        }
        // The gate opened, so the fixture had already published both pids.
        assert!(
            recorded.is_ok(),
            "the gate opened without the pids being published"
        );

        // The drain runs before the host exits, so the tree is already gone;
        // the margin is for a loaded machine's scheduling, not for a second
        // teardown.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && pids.iter().copied().any(alive) {
            std::thread::sleep(Duration::from_millis(20));
        }
        let survivors: Vec<i32> = pids.iter().copied().filter(|p| alive(*p)).collect();
        sweep();
        assert!(
            survivors.is_empty(),
            "the host exited and left {survivors:?} running"
        );
        out
    }
}

/// Run `watch`ed external work, then `tail` (the rest of the script), and
/// assert nothing of its tree is left once `ral` has exited.
fn nothing_survives(tail: &str) {
    let tree = Tree::new();
    let out = run_with_timeout(
        "ral_worker_teardown",
        &[],
        &format!(
            "let job = watch \"tests\" {{ {} ; return `done }}\ncat {}\n{tail}\n",
            tree.fixture.display(),
            tree.gate.display()
        ),
        GATE_TIMEOUT,
    );
    tree.assert_gone(out);
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

/// `cancel` claims the handle, so an `await` after it fails at once, an
/// ordinary error rather than a stop.
#[test]
fn a_cancelled_workers_await_finds_it_claimed() {
    let out = run_with_timeout(
        "ral_worker_teardown",
        &[],
        "let h = spawn { /bin/sleep 30 }; cancel $h; await $h",
        GATE_TIMEOUT,
    )
    .expect("await of a cancelled worker hung");
    assert_eq!(out.status, 1, "stderr: {}", out.stderr);
    assert!(
        out.stderr
            .contains("await: this worker's result was already claimed"),
        "stderr: {}",
        out.stderr
    );
}

/// A worker's `exit` is the host's: it reaches the run that observes the
/// worker and ends it.
#[test]
fn an_exit_in_a_worker_ends_the_awaiting_run() {
    let out = run_with_timeout(
        "ral_worker_teardown",
        &[],
        "let h = spawn { exit 3 }; await $h; echo after",
        GATE_TIMEOUT,
    )
    .expect("await of an exited worker hung");
    assert_eq!(out.status, 3, "stderr: {}", out.stderr);
    assert!(!out.stdout.contains("after"), "stdout: {}", out.stdout);
}

#[test]
fn a_poll_of_an_exited_worker_ends_the_run() {
    let out = run_with_timeout(
        "ral_worker_teardown",
        &[],
        "let h = spawn { exit 3 }; /bin/sleep 0.5; poll $h; echo after",
        GATE_TIMEOUT,
    )
    .expect("poll of an exited worker hung");
    assert_eq!(out.status, 3, "stderr: {}", out.stderr);
    assert!(!out.stdout.contains("after"), "stdout: {}", out.stdout);
}

/// A `par` whose first item fails ends its sibling before the failure
/// reaches the `try`: the sibling's external tree dies with the call.  Item 0
/// fails only once item 1's tree is up, so the sibling is established.
#[test]
fn a_par_childs_external_dies_with_the_call() {
    let tree = Tree::new();
    let out = run_with_timeout(
        "ral_worker_teardown",
        &[],
        &format!(
            "try {{ par {{ |x| if $[$x == 0] {{ cat {}; fail [status: 1, message: x] }} \
             else {{ {} }} }} [0, 1] 2 }} {{ |_| echo caught; return [] }}\n",
            tree.gate.display(),
            tree.fixture.display()
        ),
        GATE_TIMEOUT,
    );
    let out = tree.assert_gone(out);
    assert_eq!(out.stdout, "caught\n");
}
