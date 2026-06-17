//! Windows sandbox support.
//!
//! Two distinct Job Object paths live here:
//!
//! - [`apply_job_limits`] — post-spawn Job Object capping a single
//!   external command's process tree at 512 processes.  Used for plain
//!   externals inside a grant; gives fork-bomb mitigation, nothing more.
//!   There is a brief window between `CreateProcess` and
//!   `AssignProcessToJobObject` during which the child is unconstrained
//!   — acceptable for this use case.
//!
//! - [`build_confined_job`] — pre-spawn Job Object for the
//!   restricted-token-confined eval child.  Adds `KILL_ON_JOB_CLOSE` so
//!   the parent crashing reaps the child, and the assignment happens
//!   before the suspended child's first instruction (see
//!   [`crate::sandbox::windows_restricted_token::spawn_confined`]).
//!
//! Real fs/net confinement lives in
//! [`crate::sandbox::windows_restricted_token`]; this module owns only
//! the Job Object plumbing.

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

/// RAII Job Object owned by the confined-eval transport.
///
/// `Drop` closes the handle, which fires `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
/// on every still-running process in the job — the child is reaped
/// even if the parent panics or is killed.
#[cfg(windows)]
pub(super) struct JobHandle {
    pub handle: HANDLE,
}

#[cfg(windows)]
impl JobHandle {
    /// Build a Job Object for the confined-eval child.  Sets
    /// `KILL_ON_JOB_CLOSE` (parent crash reaps the child) and caps
    /// `ActiveProcessLimit` at 512 (mirrors the Unix `RLIMIT_NPROC` cap).
    ///
    /// Returns `Err` carrying a human-readable reason when any Win32
    /// step fails; callers surface this through `Break::Error` in the
    /// runner.
    pub(super) fn build_confined() -> Result<Self, String> {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
            JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectExtendedLimitInformation,
            SetInformationJobObject,
        };
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(format!(
                    "CreateJobObjectW failed: error {}",
                    std::io::Error::last_os_error()
                ));
            }
            let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
            info.BasicLimitInformation.LimitFlags =
                JOB_OBJECT_LIMIT_ACTIVE_PROCESS | JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            info.BasicLimitInformation.ActiveProcessLimit = 512;
            if SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &raw const info as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            ) == 0
            {
                let err = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(format!("SetInformationJobObject failed: {err}"));
            }
            // Belt-and-suspenders UI restrictions on the same job: deny
            // handle inheritance from outside processes, clipboard
            // read/write, desktop switches, ExitWindows, and global
            // SystemParametersInfo writes.  These cost one extra
            // SetInformationJobObject call per spawn.
            use windows_sys::Win32::System::JobObjects::{
                JOB_OBJECT_UILIMIT_DESKTOP, JOB_OBJECT_UILIMIT_EXITWINDOWS,
                JOB_OBJECT_UILIMIT_HANDLES, JOB_OBJECT_UILIMIT_READCLIPBOARD,
                JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS, JOB_OBJECT_UILIMIT_WRITECLIPBOARD,
                JOBOBJECT_BASIC_UI_RESTRICTIONS, JobObjectBasicUIRestrictions,
            };
            let ui = JOBOBJECT_BASIC_UI_RESTRICTIONS {
                UIRestrictionsClass: JOB_OBJECT_UILIMIT_HANDLES
                    | JOB_OBJECT_UILIMIT_READCLIPBOARD
                    | JOB_OBJECT_UILIMIT_WRITECLIPBOARD
                    | JOB_OBJECT_UILIMIT_DESKTOP
                    | JOB_OBJECT_UILIMIT_EXITWINDOWS
                    | JOB_OBJECT_UILIMIT_SYSTEMPARAMETERS,
            };
            if SetInformationJobObject(
                job,
                JobObjectBasicUIRestrictions,
                &raw const ui as *const _,
                std::mem::size_of::<JOBOBJECT_BASIC_UI_RESTRICTIONS>() as u32,
            ) == 0
            {
                let err = std::io::Error::last_os_error();
                CloseHandle(job);
                return Err(format!(
                    "SetInformationJobObject(JobObjectBasicUIRestrictions) failed: {err}"
                ));
            }
            Ok(JobHandle { handle: job })
        }
    }

    /// Assign `process_handle` (typically a still-suspended child) to
    /// this job.  Caller must `ResumeThread` after the assignment so
    /// the limits apply before the first user instruction runs.
    pub(super) fn assign(&self, process_handle: HANDLE) -> Result<(), String> {
        use windows_sys::Win32::System::JobObjects::AssignProcessToJobObject;
        unsafe {
            if AssignProcessToJobObject(self.handle, process_handle) == 0 {
                return Err(format!(
                    "AssignProcessToJobObject failed: {}",
                    std::io::Error::last_os_error()
                ));
            }
        }
        Ok(())
    }
}

#[cfg(windows)]
impl Drop for JobHandle {
    fn drop(&mut self) {
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
