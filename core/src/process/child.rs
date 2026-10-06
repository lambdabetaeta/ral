//! A spawned child, and the one door from it to the reaper.

use super::outcome::WaitOutcome;
use super::reaper::{Watch, watch};

// ── Child handle ───────────────────────────────────────────────────────────
//
// Bare `Child::wait` / `try_wait` omit `WUNTRACED`: a SIGSTOP'd child reads as
// "still running" and `wait` blocks forever.  `clippy.toml` disallows the raw
// pair outside `ChildHandle`; the few exceptions carry an `#[allow]`.

/// A spawned child.
///
/// [`Self::into_watch`] is the door to the reaper, which owns the wait on
/// both platforms; [`Self::reap`] is the blocking wait after a confirmed
/// kill, and [`Self::try_reap`] the non-blocking peer `seed/hatch.rs`'s table
/// polls directly.
pub struct ChildHandle(ChildRepr);

enum ChildRepr {
    #[cfg_attr(not(unix), allow(dead_code))]
    Std(std::process::Child),
    #[cfg(windows)]
    RawWindows(crate::process::launch::RawChild),
}

impl ChildHandle {
    #[cfg_attr(not(unix), allow(dead_code))]
    pub(crate) fn from_std(child: std::process::Child) -> Self {
        Self(ChildRepr::Std(child))
    }

    #[cfg(windows)]
    pub(crate) fn from_windows_raw(child: crate::process::launch::RawChild) -> Self {
        Self(ChildRepr::RawWindows(child))
    }

    pub fn id(&self) -> u32 {
        match &self.0 {
            ChildRepr::Std(child) => child.id(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.id(),
        }
    }

    /// # Errors
    /// Returns `Err` if the platform kill — SIGKILL on Unix,
    /// `TerminateProcess` on Windows — fails.
    pub(crate) fn kill(&mut self) -> std::io::Result<()> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.kill(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.kill(),
        }
    }

    pub(crate) fn take_stdout(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.0 {
            ChildRepr::Std(child) => child
                .stdout
                .take()
                .map(|stdout| Box::new(stdout) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stdout(),
        }
    }

    pub(crate) fn take_stderr(&mut self) -> Option<Box<dyn std::io::Read + Send>> {
        match &mut self.0 {
            ChildRepr::Std(child) => child
                .stderr
                .take()
                .map(|stderr| Box::new(stderr) as Box<dyn std::io::Read + Send>),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.take_stderr(),
        }
    }

    #[cfg(windows)]
    pub(crate) fn raw_process_handle(&self) -> windows_sys::Win32::Foundation::HANDLE {
        match &self.0 {
            ChildRepr::Std(child) => {
                use std::os::windows::io::AsRawHandle;
                child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE
            }
            ChildRepr::RawWindows(child) => child.raw_process_handle(),
        }
    }

    /// Non-blocking reap: `Ok(None)` when nothing has exited yet.  Plain
    /// `try_wait`, no `WUNTRACED`, so a stopped child reads as still
    /// running.  Accepted exception to the one SIGCONT rule: `seed/hatch.rs`'s
    /// table is swept on demand rather than watched, so a `kill -STOP` on a
    /// hatched child is never answered and the child stays stopped.
    ///
    /// # Errors
    /// Returns `Err` if the poll fails.
    #[cfg(unix)]
    #[allow(
        clippy::disallowed_methods,
        reason = "reaper: this is `ChildHandle::try_reap`, the door the ban names, around the one raw `try_wait`"
    )]
    pub(crate) fn try_reap(&mut self) -> std::io::Result<Option<WaitOutcome>> {
        let ChildRepr::Std(child) = &mut self.0;
        child
            .try_wait()
            .map(|opt| opt.map(WaitOutcome::from_exit_status))
    }

    /// Blocking reap after a confirmed SIGKILL, which terminates even a stopped
    /// process — so the one raw `Child::wait` inside `ChildHandle` lives here.
    ///
    /// # Errors
    /// Returns `Err` if the wait fails.
    #[allow(
        clippy::disallowed_methods,
        reason = "reaper: this is `ChildHandle::reap`, the blocking reap after a confirmed kill, around the one raw `wait`"
    )]
    pub(crate) fn reap(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match &mut self.0 {
            ChildRepr::Std(child) => child.wait(),
            #[cfg(windows)]
            ChildRepr::RawWindows(child) => child.reap(),
        }
    }

    /// The one door from a spawned child to a [`Watch`]: consumes the
    /// handle, so no later `wait` on this pid is writable behind the
    /// reaper's back.  The stdio ends must already be taken.
    pub(crate) fn into_watch<E: Send + 'static>(
        self,
        tx: std::sync::mpsc::Sender<E>,
        f: impl FnOnce(WaitOutcome) -> E + Send + 'static,
    ) -> Watch {
        let pid = self.id();
        let watch = watch(pid, tx, f);
        // Dropping a `std::process::Child` neither kills nor reaps on Unix,
        // and on Windows only closes std's own handle — the watch opened its
        // own while registering.
        drop(self);
        watch
    }
}
