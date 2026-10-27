use std::convert::Infallible;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use guest_abi::layout;
use rustix::process::{Pid, Signal, kill_process_group};
use rustix::system::{RebootCommand, reboot};

use super::fleet::{Engines, live};
use crate::engine;
use crate::reap::{self, Waking};

const GRACE: Duration = Duration::from_secs(5);
const PULSE: Duration = Duration::from_millis(50);
const SETTLE: Duration = Duration::from_millis(500);

/// Reap until the root exits or a stop signal arrives; [`reap::failure`] names fatal stops.
pub(super) fn attend(root: Pid, engines: &Engines) -> Result<String, String> {
    loop {
        let signal = reap::next_signal()
            .map_err(|err| format!("waiting for the guest's signals failed: {err}"))?;
        if signal != libc::SIGCHLD {
            return Ok(reap::failure()
                .unwrap_or_else(|| "the host asked for this machine to stop".to_string()));
        }
        // Drain under the spawn lock; tear cgroups down only after releasing it.
        let mut dead = Vec::new();
        let mut fleet = live(engines);
        loop {
            let waking = reap::poll_any()
                .map_err(|err| format!("reaping the guest's processes failed: {err}"))?;
            let (pid, death) = match waking {
                Waking::Reaped { pid, death } => (pid, death),
                Waking::Idle => break,
                Waking::Childless => {
                    return Ok(
                        "nothing is left running in the guest, not even the engine".to_string()
                    );
                }
            };
            if pid == root {
                return Ok(engine::epitaph(death));
            }
            if fleet.forget(pid) {
                eprintln!("ral-guest: engine pid {} {death}", pid.as_raw_nonzero());
                dead.push(pid);
            } else {
                eprintln!(
                    "ral-guest: reaped orphan pid {}, which {death}",
                    pid.as_raw_nonzero()
                );
            }
        }
        drop(fleet);
        for pid in dead {
            let tree =
                Path::new(layout::EXEC_CGROUP).join(format!("engine-{}", pid.as_raw_nonzero()));
            kill_cgroup(&tree);
            remove_cgroup(&tree, Instant::now() + SETTLE);
        }
    }
}

/// An absent cgroup means the engine never ran a command.
fn kill_cgroup(path: &Path) {
    match std::fs::write(path.join("cgroup.kill"), "1") {
        Err(err) if err.kind() != io::ErrorKind::NotFound => eprintln!(
            "ral-guest: could not kill the commands under {}: {err}",
            path.display()
        ),
        _ => {}
    }
}

/// Remove deepest first, retrying busy cgroups until `deadline`.
fn remove_cgroup(path: &Path, deadline: Instant) {
    for child in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if child.file_type().is_ok_and(|kind| kind.is_dir()) {
            remove_cgroup(&child.path(), deadline);
        }
    }
    loop {
        match std::fs::remove_dir(path) {
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) && Instant::now() < deadline => {
                std::thread::sleep(PULSE);
            }
            Err(err) if err.kind() != io::ErrorKind::NotFound => {
                return eprintln!("ral-guest: could not remove {}: {err}", path.display());
            }
            _ => return,
        }
    }
}

pub(super) fn halt(engines: &Engines, why: &str) -> Result<Infallible, String> {
    eprintln!("ral-guest: {why}");
    let all = live(engines).close();
    for &engine in &all {
        let _ = kill_process_group(engine, Signal::TERM);
    }
    // Reaped leaders cannot exit again; their process groups still need the sweep.
    let mut pending: Vec<Pid> = all
        .iter()
        .copied()
        .filter(|&engine| rustix::process::test_kill_process(engine).is_ok())
        .collect();
    let deadline = Instant::now() + GRACE;
    while !pending.is_empty() && Instant::now() < deadline {
        match reap::poll_any() {
            Ok(Waking::Reaped { pid, .. }) => pending.retain(|&p| p != pid),
            Ok(Waking::Idle) => std::thread::sleep(PULSE),
            // The KILL sweep is unconditional even when waiting fails.
            Ok(Waking::Childless) | Err(_) => break,
        }
    }
    for &engine in &all {
        let _ = kill_process_group(engine, Signal::KILL);
    }
    kill_cgroup(Path::new(layout::EXEC_CGROUP));
    // Flush the workspace before its export disappears with the machine.
    rustix::fs::sync();
    reboot(RebootCommand::PowerOff)
        .map_err(|err| format!("could not power the machine off: {err}"))?;
    Err("the kernel accepted the power-off and the machine is still running".to_string())
}
