//! Process creation outside `std`'s spawn: a bare fork, for a child that runs
//! ral's own code, and the blocking wait that reaps a child by pid.

use super::site::Silent;
use crate::site::site;
use rustix::process::Pid;
use std::io;
use std::process::ExitStatus;

/// What [`fork`] made, as each side sees it.
pub(crate) enum Forked {
    Child,
    Parent(Pid),
}

/// Fork this process, outside the spawn lock: a caller in a spawn's hook
/// already holds it, and a single-threaded one needs none.
///
/// # Safety
/// As `fork(2)`: where other threads exist, the child may make only
/// async-signal-safe calls until it execs or exits.
pub(crate) unsafe fn fork() -> io::Result<Forked> {
    // SAFETY: the caller's.
    match unsafe { site!(Silent::Fork, libc::fork()) } {
        -1 => Err(io::Error::last_os_error()),
        0 => Ok(Forked::Child),
        pid => Ok(Forked::Parent(
            Pid::from_raw(pid).expect("fork returns a positive pid to its parent"),
        )),
    }
}

/// Block until `pid`, this process's child, ends, and consume its zombie; a
/// stop is no end.
pub(crate) fn reap(pid: Pid) -> io::Result<ExitStatus> {
    use rustix::process::{WaitOptions, waitpid};
    use std::os::unix::process::ExitStatusExt;
    let (_, status) = rustix::io::retry_on_intr(|| waitpid(Some(pid), WaitOptions::empty()))?
        .expect("a blocking wait returns its child");
    Ok(ExitStatus::from_raw(status.as_raw()))
}
