//! Windows sandbox: the fork-bomb Job Object.
//!
//! Confinement proper lives in the submodules — `appcontainer` (profile
//! lifecycle and `LowBox` spawn capabilities), `dacl` (persistent
//! capability-ACE stamping), and `session`, which composes them: one profile
//! per distinct fs projection, and a token whose capability SIDs are exactly
//! the projection's stamped `(path, kind)` grants.

pub(crate) mod appcontainer;
pub(crate) mod dacl;
pub(crate) mod session;

use windows_sys::Win32::Foundation::CloseHandle;

/// Cap a standalone external's process tree at `ACTIVE_PROCESS_CAP` live
/// processes.  Post-spawn, so the child runs unconstrained for the window
/// between `CreateProcess` and `AssignProcessToJobObject`.
pub(super) fn apply_job_limits(child: &crate::process::ChildHandle) {
    use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        if !crate::process::set_active_process_limit(job, super::ACTIVE_PROCESS_CAP) {
            CloseHandle(job);
            return;
        }
        let proc_handle = child.raw_process_handle();
        if AssignProcessToJobObject(job, proc_handle) == 0 {
            // Commonly ral itself sitting in a job that forbids nesting.
            crate::dbg_trace!(
                "sandbox-win",
                "AssignProcessToJobObject failed, fork-bomb cap not applied: {}",
                std::io::Error::last_os_error()
            );
        }
        // The job outlives this handle: it dissolves only once its last member
        // has exited, so the cap holds for the child's whole tree.
        CloseHandle(job);
    }
}
