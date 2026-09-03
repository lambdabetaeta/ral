//! Windows reaper: `RegisterWaitForSingleObject` per watched handle, no
//! polling thread and no stops — job control has no Windows counterpart.
//!
//! `watch` opens a handle for the pid and registers a one-shot wait whose
//! callback posts the exit. `reap`, and `Drop`, block on the same handle
//! themselves before unregistering: a process handle that has signalled
//! stays signalled, so by the time either calls `UnregisterWaitEx` the
//! thread-pool callback has already fired (or is finishing) and freed its
//! context — never cancelled unfired, which would leak it. Compile-checked
//! only (`just check-windows`); the four real-child tests in `reaper::unix`
//! are this plan's whole cross-platform question, and Windows has no stop
//! to test.

use std::io;
use std::sync::Mutex;
use std::sync::mpsc::Sender;

use windows_sys::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE, WAIT_OBJECT_0};
use windows_sys::Win32::Storage::FileSystem::SYNCHRONIZE;
use windows_sys::Win32::System::Threading::{
    GetExitCodeProcess, INFINITE, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    PROCESS_TERMINATE, RegisterWaitForSingleObject, TerminateProcess, UnregisterWaitEx,
    WT_EXECUTEONLYONCE, WaitForSingleObject,
};

use crate::process::outcome::{STAGE_KILL_EXIT_CODE, Signal, WaitOutcome, WaitPoll};

struct CallbackCtx<E: Send + 'static> {
    handle: HANDLE,
    tx: Sender<E>,
    f: Box<dyn Fn(WaitPoll) -> E + Send>,
}

/// The callback `RegisterWaitForSingleObject` invokes on a thread-pool
/// thread once the handle signals. Windows has no stop to report, so the
/// only event is the terminal one.
unsafe extern "system" fn wait_callback<E: Send + 'static>(
    context: *mut core::ffi::c_void,
    _timed_out: bool,
) {
    let ctx = unsafe { Box::from_raw(context.cast::<CallbackCtx<E>>()) };
    let mut code: u32 = 0;
    unsafe { GetExitCodeProcess(ctx.handle, &mut code) };
    let outcome =
        WaitOutcome::from_exit_status(std::os::windows::process::ExitStatusExt::from_raw(code));
    let _ = ctx.tx.send((ctx.f)(WaitPoll::Done(outcome)));
}

/// Process-wide capability to watch children for exit.
pub struct Reaper;

impl Reaper {
    pub fn global() -> &'static Self {
        static SINGLETON: Reaper = Reaper;
        &SINGLETON
    }

    /// Deliver `pid`'s exit to `tx`, shaped by `f`.
    pub fn watch<E: Send + 'static>(
        &self,
        pid: u32,
        tx: Sender<E>,
        f: impl Fn(WaitPoll) -> E + Send + 'static,
    ) -> Watch {
        let handle = unsafe {
            OpenProcess(
                PROCESS_TERMINATE | PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE,
                0,
                pid,
            )
        };
        assert!(!handle.is_null(), "OpenProcess for a watched pid must not fail");

        let ctx = Box::into_raw(Box::new(CallbackCtx { handle, tx, f: Box::new(f) }));
        let mut wait_handle: HANDLE = std::ptr::null_mut();
        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut wait_handle,
                handle,
                Some(wait_callback::<E>),
                ctx.cast(),
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };
        if ok == 0 {
            // The callback will never run to reclaim `ctx`; reclaim it here.
            drop(unsafe { Box::from_raw(ctx) });
            panic!("RegisterWaitForSingleObject for a watched pid must not fail");
        }

        Watch {
            handle,
            wait_handle: Mutex::new(Some(wait_handle)),
        }
    }
}

/// A subscription on one watched pid. Dropping it blocks for the child's
/// exit and unregisters the callback, same as [`Self::reap`] with its
/// outcome discarded.
#[must_use]
pub struct Watch {
    handle: HANDLE,
    wait_handle: Mutex<Option<HANDLE>>,
}

// SAFETY: `HANDLE` is an opaque kernel object reference; Windows itself
// requires no thread affinity to use one.
unsafe impl Send for Watch {}
unsafe impl Sync for Watch {}

impl Watch {
    /// The Windows counterpart of `signal`: no job control exists here, so
    /// the only sanctioned act is termination, with ral's own stage-kill
    /// exit code.
    ///
    /// # Errors
    /// Returns `Err` if `TerminateProcess` fails.
    pub fn signal(&self, _sig: Signal) -> io::Result<()> {
        let ok = unsafe { TerminateProcess(self.handle, STAGE_KILL_EXIT_CODE as u32) };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    /// Block for the child's exit, unregister the callback, and read the
    /// exit code.
    ///
    /// # Errors
    /// Returns `Err` if the wait or the exit-code read fails.
    pub fn reap(self) -> io::Result<WaitOutcome> {
        block_until_exit(self.handle)?;
        read_exit_code(self.handle)
        // `self` drops here: unregistering is then a formality, the
        // callback having already fired on the same signalled handle.
    }
}

/// A process handle that has signalled stays signalled, so this is safe to
/// call whether or not the child has already exited.
fn block_until_exit(handle: HANDLE) -> io::Result<()> {
    let ret = unsafe { WaitForSingleObject(handle, INFINITE) };
    if ret == WAIT_OBJECT_0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn read_exit_code(handle: HANDLE) -> io::Result<WaitOutcome> {
    let mut code: u32 = 0;
    let ok = unsafe { GetExitCodeProcess(handle, &mut code) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(WaitOutcome::from_exit_status(
        std::os::windows::process::ExitStatusExt::from_raw(code),
    ))
}

impl Drop for Watch {
    fn drop(&mut self) {
        let _ = block_until_exit(self.handle);
        if let Some(wh) = self
            .wait_handle
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
        {
            // `INVALID_HANDLE_VALUE` is the sentinel that makes this call
            // block until any in-flight callback finishes, rather than
            // taking a completion-event HANDLE. The wait having already
            // been satisfied above, this only waits one out — never cancels
            // one unfired, which would leak its `ctx`.
            unsafe { UnregisterWaitEx(wh, INVALID_HANDLE_VALUE) };
        }
        unsafe { CloseHandle(self.handle) };
    }
}
