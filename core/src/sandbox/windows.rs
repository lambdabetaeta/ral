//! Windows sandbox support.
//!
//! - [`apply_job_limits`] — post-spawn Job Object capping a single
//!   external command's process tree at 512 processes.  Used for plain
//!   externals inside a grant; gives fork-bomb mitigation, nothing more.
//!   There is a brief window between `CreateProcess` and
//!   `AssignProcessToJobObject` during which the child is unconstrained
//!   — acceptable for this use case.
//!
//! This module owns only the Job Object plumbing.

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};

#[cfg(windows)]
pub(super) fn apply_job_limits(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JobObjectBasicLimitInformation, SetInformationJobObject,
    };
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_BASIC_LIMIT_INFORMATION = std::mem::zeroed();
        info.LimitFlags = JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
        info.ActiveProcessLimit = 512;
        if SetInformationJobObject(
            job,
            JobObjectBasicLimitInformation,
            &raw const info as *const _,
            std::mem::size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() as u32,
        ) == 0
        {
            CloseHandle(job);
            return;
        }
        let proc_handle = child.as_raw_handle() as HANDLE;
        if AssignProcessToJobObject(job, proc_handle) == 0 {
            // The 512-process cap silently does not apply — commonly
            // when ral itself runs inside a job that disallows nesting.
            // This path returns (), so trace rather than propagate.
            crate::dbg_trace!(
                "sandbox-win",
                "AssignProcessToJobObject failed, fork-bomb cap not applied: {}",
                std::io::Error::last_os_error()
            );
        }
        CloseHandle(job);
    }
}
