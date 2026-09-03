//! The process reaper — one process-wide watcher over `waitid`/
//! `RegisterWaitForSingleObject`, so a subscriber never blocks a thread on
//! its own child.
//!
//! [`Reaper::watch`] delivers a pid's events to the subscriber's own channel,
//! shaped by the subscriber's own closure — the pipeline collector receives
//! its own event enum, [`crate::process::ChildHandle`]'s future caller
//! whatever it likes.  The reaper never reaps on its own: an exit is
//! observed with `WNOWAIT` (Unix) and the pid stays a zombie — or the handle
//! stays open (Windows) — until [`Watch::reap`].  That is what closes pid
//! reuse structurally: a [`Watch`] is the only way to signal the child, and
//! it holds the zombie open until its owner says otherwise.
//!
//! Standalone as of this module: nothing in the tree calls [`Reaper::watch`]
//! yet.  `RunningChild::wait`, the pipeline collector, and `hatch.rs` still
//! run their own poll loops.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{Reaper, Watch};
#[cfg(unix)]
pub(crate) use unix::ensure_installed;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Reaper, Watch};
