//! PID 1's standing obligation: every orphan in the guest ends up here.
//!
//! When a process dies its children are reparented to PID 1, and their exit
//! statuses stay in the kernel's process table until somebody waits for
//! them.  A guest whose init does not wait leaks a zombie per orphan until
//! the PID table is full.  So the daemon's main loop *is* a wait loop, and
//! everything else it does — supervising the engine, honouring a stop
//! request — is expressed as an answer to "what did the last wait return?".
//!
//! ## The wait vocabulary
//!
//! ral already typed its Unix wait funnel (`core/src/process/signal/unix.rs`,
//! decision `260720_total-wait-status`), and this module speaks the same
//! language rather than a second dialect of `waitpid`: rustix's transparent
//! [`WaitStatus`] read through its bit-test accessors, blocking and polling
//! as two distinct doors so an impossible idle result cannot leak into a
//! blocking caller, and no `WIF*` decoding anywhere.  It does not *import*
//! `ral_core::process`, and deliberately: PID 1 is a static binary in a boot
//! artifact, and linking the shell's process layer into it would pull the
//! whole evaluator — serde, clap, the search engine — into a program whose
//! job is `mount`, `fork`, and `wait`.  The vocabulary is shared; the
//! dependency is not.
//!
//! One rule *is* inverted, for a reason the shell does not have: `EINTR` is
//! not retried away.  In the shell a signal arriving during a wait is noise
//! to be transparently absorbed.  In an init it is the message — the host's
//! stop request reaches this loop as a signal and nothing else — so it
//! surfaces as [`Waking::Interrupted`] rather than being swallowed.

use rustix::io::Errno;
use rustix::process::{Pid, WaitOptions, WaitStatus, wait};

/// How a child left the world.
///
/// A total reading of a [`WaitStatus`]: exited, terminated by a signal, or —
/// the case a reaper must accept rather than assert about — a status this
/// vocabulary has no case for, kept in its raw kernel spelling.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Death {
    /// The process called `exit` with this status.
    Exited(i32),
    /// The process was terminated by this signal number.
    Signalled(i32),
    /// The kernel reported something else.  Signal numbers are left raw:
    /// naming them would mean a second copy of the shell's signal table, and
    /// a number the host can look up beats a name that might drift.
    Unclassified(i32),
}

impl Death {
    /// Read a kernel wait status.  The whole of this module's contact with
    /// the raw status word.
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

/// What one turn of the wait loop found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Waking {
    /// A child was reaped.
    Reaped {
        /// Which child.  The daemon compares this against the engine's pid;
        /// anything else was an orphan, and reaping it was the whole point.
        pid: Pid,
        /// How it died.
        death: Death,
    },
    /// A signal arrived while waiting.  In an init this is not noise: it is
    /// how the host asks for the machine to stop.
    Interrupted,
    /// Nothing is reapable right now.  Only [`poll_any`] can return this;
    /// [`wait_any`] blocks instead, so the impossible "idle" answer cannot
    /// reach a caller that asked to block.
    Idle,
    /// No children remain at all.  For PID 1 this means the guest's entire
    /// userland is gone.
    Childless,
}

/// Wait for any child to die, blocking until one does or a signal arrives.
///
/// # Errors
/// Returns the kernel's error for anything other than the interruption and
/// no-children cases, which are answers rather than failures.
pub fn wait_any() -> Result<Waking, Errno> {
    classify(wait(WaitOptions::empty()))
}

/// Ask whether any child is reapable, without blocking.
///
/// The polling twin of [`wait_any`], used while a shutdown's grace period
/// runs down.
///
/// # Errors
/// As [`wait_any`].
pub fn poll_any() -> Result<Waking, Errno> {
    classify(wait(WaitOptions::NOHANG))
}

/// Turn a wait result into the loop's vocabulary.
///
/// The classification, kept separate from the syscall so it can be tested
/// without one.
fn classify(result: Result<Option<(Pid, WaitStatus)>, Errno>) -> Result<Waking, Errno> {
    match result {
        Ok(Some((pid, status))) => Ok(Waking::Reaped {
            pid,
            death: Death::read(status),
        }),
        Ok(None) => Ok(Waking::Idle),
        Err(Errno::INTR) => Ok(Waking::Interrupted),
        Err(Errno::CHILD) => Ok(Waking::Childless),
        Err(err) => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A signal delivered during the wait is the loop's wakeup, not an
    /// error and not something to retry away.
    #[test]
    fn an_interrupted_wait_is_a_waking() {
        assert_eq!(classify(Err(Errno::INTR)), Ok(Waking::Interrupted));
    }

    /// `ECHILD` is the answer "there is nobody left", which for PID 1 is a
    /// fact about the guest rather than a failure.
    #[test]
    fn no_children_left_is_an_answer_not_an_error() {
        assert_eq!(classify(Err(Errno::CHILD)), Ok(Waking::Childless));
    }

    /// A `NOHANG` wait that found nothing is idle.
    #[test]
    fn nothing_reapable_is_idle() {
        assert_eq!(classify(Ok(None)), Ok(Waking::Idle));
    }

    /// Everything else is a real failure and reaches the caller as one.
    #[test]
    fn any_other_errno_reaches_the_caller() {
        assert_eq!(classify(Err(Errno::INVAL)), Err(Errno::INVAL));
    }

    /// Each death reads as a sentence the host's log can be read by a
    /// person, and a clean exit does not say "status 0".
    #[test]
    fn a_death_reads_as_a_sentence() {
        assert_eq!(Death::Exited(0).to_string(), "exited cleanly");
        assert_eq!(Death::Exited(2).to_string(), "exited with status 2");
        assert_eq!(Death::Signalled(9).to_string(), "was killed by signal 9");
        assert!(Death::Unclassified(0x57f).to_string().contains("0x57f"));
    }
}
