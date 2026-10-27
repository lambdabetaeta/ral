//! PID 1 reaps every orphan; awaited signals have no handlers.
//! Block them before threads and consume them synchronously so no stop is lost or misdirected.
//! Wait-status vocabulary is shared with core without linking its runtime.

use std::io;
use std::sync::OnceLock;

use rustix::io::Errno;
use rustix::process::{Pid, Signal, WaitOptions, WaitStatus, getpid, kill_process, wait};

static FAILURE: OnceLock<String> = OnceLock::new();

/// First failure wins; a process-directed signal reaches init's waiting thread.
pub(crate) fn fail(why: String) {
    let _ = FAILURE.set(why);
    let _ = kill_process(getpid(), Signal::TERM);
}

pub(crate) fn failure() -> Option<String> {
    FAILURE.get().cloned()
}

/// `CadOff` converts Ctrl-Alt-Del to SIGINT; SIGTERM and ACPI SIGPWR also request shutdown.
const STOP_SIGNALS: [libc::c_int; 3] = [libc::SIGTERM, libc::SIGINT, libc::SIGPWR];

fn awaited() -> libc::sigset_t {
    // SAFETY: `sigemptyset` initialises `set` before `sigaddset` touches it.
    unsafe {
        let mut set = std::mem::zeroed();
        libc::sigemptyset(&raw mut set);
        for signal in STOP_SIGNALS.into_iter().chain([libc::SIGCHLD]) {
            libc::sigaddset(&raw mut set, signal);
        }
        set
    }
}

pub(crate) fn block() -> io::Result<()> {
    mask(libc::SIG_BLOCK)
}

/// Async-signal-safe so `pre_exec` can undo the inherited block.
pub(crate) fn unblock() -> io::Result<()> {
    mask(libc::SIG_UNBLOCK)
}

fn mask(how: libc::c_int) -> io::Result<()> {
    let set = awaited();
    // SAFETY: `set` is initialised; only the calling thread's mask changes.
    match unsafe { libc::pthread_sigmask(how, &raw const set, std::ptr::null_mut()) } {
        0 => Ok(()),
        err => Err(io::Error::from_raw_os_error(err)),
    }
}

/// Every thread must block the awaited signals before this synchronous wait.
pub(crate) fn next_signal() -> io::Result<libc::c_int> {
    let set = awaited();
    loop {
        // SAFETY: `set` is initialised, and a null `siginfo_t` is permitted.
        let signal = unsafe { libc::sigwaitinfo(&raw const set, std::ptr::null_mut()) };
        if signal >= 0 {
            return Ok(signal);
        }
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Preserve any unrecognized kernel wait status instead of asserting it cannot occur.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Death {
    Exited(i32),
    Signalled(i32),
    /// Keep raw numbers to avoid duplicating the shell's signal table.
    Unclassified(i32),
}

impl Death {
    fn read(status: WaitStatus) -> Self {
        if let Some(code) = status.exit_status() {
            return Self::Exited(code);
        }
        if let Some(signal) = status.terminating_signal() {
            return Self::Signalled(signal);
        }
        Self::Unclassified(status.as_raw())
    }
}

impl std::fmt::Display for Death {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Exited(0) => f.write_str("exited cleanly"),
            Self::Exited(code) => write!(f, "exited with status {code}"),
            Self::Signalled(signal) => write!(f, "was killed by signal {signal}"),
            Self::Unclassified(raw) => write!(
                f,
                "left the wait status {raw:#x}, which is neither an exit nor a signal"
            ),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Waking {
    Reaped {
        pid: Pid,
        death: Death,
    },
    Idle,
    /// No children means the entire guest userland is gone.
    Childless,
}

pub(crate) fn poll_any() -> Result<Waking, Errno> {
    classify(wait(WaitOptions::NOHANG))
}

fn classify(result: Result<Option<(Pid, WaitStatus)>, Errno>) -> Result<Waking, Errno> {
    match result {
        Ok(Some((pid, status))) => Ok(Waking::Reaped {
            pid,
            death: Death::read(status),
        }),
        Ok(None) => Ok(Waking::Idle),
        Err(Errno::CHILD) => Ok(Waking::Childless),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_children_left_is_an_answer_not_an_error() {
        assert_eq!(classify(Err(Errno::CHILD)), Ok(Waking::Childless));
    }

    #[test]
    fn nothing_reapable_is_idle() {
        assert_eq!(classify(Ok(None)), Ok(Waking::Idle));
    }

    #[test]
    fn any_other_errno_reaches_the_caller() {
        assert_eq!(classify(Err(Errno::INVAL)), Err(Errno::INVAL));
    }

    #[test]
    fn a_death_reads_as_a_sentence() {
        assert_eq!(Death::Exited(0).to_string(), "exited cleanly");
        assert_eq!(Death::Exited(2).to_string(), "exited with status 2");
        assert_eq!(Death::Signalled(9).to_string(), "was killed by signal 9");
        assert!(Death::Unclassified(0x57f).to_string().contains("0x57f"));
    }
}
