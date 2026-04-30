//! Non-Unix sandbox fallbacks.
//!
//! There is no kernel sandbox to enter on these targets.  Two things still
//! need to happen so the rest of the runtime stays portable:
//!
//! - `eval_grant` runs the body in-process and warns once when the policy
//!   asks for `net: false`, since no namespace API exists to honour it and
//!   ral has no in-process network primitives to gate.
//! - On Windows specifically, each external spawned inside a grant gets
//!   pinned to a Job Object capping its process tree at 512.  There's a
//!   brief window between `CreateProcess` and `AssignProcessToJobObject`
//!   during which the child is unconstrained — acceptable for fork-bomb
//!   mitigation, the only goal here.

use crate::types::{Shell, EvalSignal, Value};

/// Non-Unix `eval_grant`: run the body under the in-ral capability checks
/// alone, and warn once if `net: false` was requested but cannot be
/// enforced on this platform.
pub fn eval_grant(body: &Value, shell: &mut Shell) -> Result<Value, EvalSignal> {
    if let Some(projection) = shell.sandbox_projection()
        && !projection.net
    {
        warn_unenforceable_net_once();
    }
    crate::builtins::call_value(body, &[], shell)
}

fn warn_unenforceable_net_once() {
    use std::sync::atomic::{AtomicBool, Ordering};
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "ral: warning: grant 'net: false' is not enforced on this platform; \
         network access is unrestricted"
    );
}

#[cfg(windows)]
pub(super) fn apply_job_limits(child: &std::process::Child) {
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
        JOBOBJECT_BASIC_LIMIT_INFORMATION, JobObjectBasicLimitInformation,
        SetInformationJobObject,
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
        AssignProcessToJobObject(job, proc_handle);
        CloseHandle(job);
    }
}
