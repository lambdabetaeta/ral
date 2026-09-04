//! The process reaper — one process-wide watcher over `waitid`/
//! `RegisterWaitForSingleObject`, so a subscriber never blocks a thread on
//! its own child.
//!
//! [`Reaper::watch`] delivers a pid's exit to the subscriber's own channel,
//! shaped by the subscriber's own closure — the pipeline collector receives
//! its own event enum, [`crate::process::ChildHandle`]'s future caller
//! whatever it likes.  A stop never reaches a subscriber: the reaper answers
//! it with `SIGCONT` itself, the one rule in one place.  The reaper never
//! reaps on its own: an exit is observed with `WNOWAIT` (Unix) and the pid
//! stays a zombie — or the handle stays open (Windows) — until
//! [`Watch::reap`].  That is what closes pid reuse structurally: a [`Watch`]
//! is the only way to signal the child, and it holds the zombie open until
//! its owner says otherwise.
//!
//! [`crate::process::ChildHandle::into_watch`] is the one door from a spawned
//! child to a watch.  Standalone as of this module: nothing outside its own
//! tests calls it yet.  `RunningChild::wait`, the pipeline collector, and
//! `hatch.rs` still run their own poll loops.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Reaper, Watch};
#[cfg(unix)]
pub(crate) use unix::{ensure_installed, kick};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Reaper, Watch};

/// No self-pipe on Windows: the console handler already runs on an ordinary
/// thread, so a kick simply scans the cancel table itself.
#[cfg(windows)]
pub(crate) fn kick() {
    super::cancel::scan_cancels();
}
