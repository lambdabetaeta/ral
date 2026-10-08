//! What a child spawned under an active grant may consume: no core dump, and a
//! budget of live processes.  A brake that cannot be armed is an `Err`, never a
//! warning: the child must not run unbraked.

#[cfg(target_os = "macos")]
mod fork_brake;

#[cfg(unix)]
use super::Launch;
use super::{ChildHandle, Pgid};
use std::io;

/// How many live processes a grant-confined child may add: the fork-bomb cap.
/// A Job Object limit on Windows; on macOS, headroom over the user's count at launch.
#[cfg(any(windows, target_os = "macos"))]
const ACTIVE_PROCESS_CAP: u32 = 512;

#[cfg(unix)]
impl Launch {
    /// Limit the child's resources in one `pre_exec`: no core dumps and, on
    /// macOS, a fork brake.  Darwin counts processes per real UID but compares
    /// the count with the *forking* process's own `RLIMIT_NPROC`, so lowering
    /// it in the child refuses its descendants' forks and no other process of
    /// the user's.  The budget is measured here, in the parent: the hook itself
    /// only calls `setrlimit`.
    ///
    /// # Errors
    /// Returns `Err` when the budget cannot be measured.
    #[allow(
        clippy::unnecessary_wraps,
        reason = "only macOS can fail: it measures the fork-brake budget"
    )]
    pub(crate) fn limit_resources(&mut self) -> io::Result<()> {
        use std::os::unix::process::CommandExt;
        #[cfg(target_os = "macos")]
        let nproc = fork_brake::limit()
            .map_err(|e| io::Error::other(format!("cannot count your processes: {e}")))?;
        unsafe {
            self.cmd.pre_exec(move || {
                let zero = libc::rlimit {
                    rlim_cur: 0,
                    rlim_max: 0,
                };
                libc::setrlimit(libc::RLIMIT_CORE, &raw const zero);
                #[cfg(target_os = "macos")]
                {
                    let cap = libc::rlimit {
                        rlim_cur: nproc,
                        rlim_max: nproc,
                    };
                    libc::setrlimit(libc::RLIMIT_NPROC, &raw const cap);
                }
                Ok(())
            });
        }
        Ok(())
    }
}

impl ChildHandle {
    /// Cap the child's process tree after the spawn.  Unix did so before
    /// `exec` ([`Launch::limit_resources`]); on Windows a Job Object does it,
    /// the pipeline's own when the child sits in `group`.  Post-spawn, so the
    /// child runs unconstrained between `CreateProcess` and the assignment.
    ///
    /// # Errors
    /// Returns `Err` when the cap could not be installed.
    #[cfg(unix)]
    #[allow(
        clippy::unnecessary_wraps,
        clippy::unused_self,
        reason = "Unix capped the tree before exec; the Windows arm reads both and can fail"
    )]
    pub(crate) fn limit_processes(&self, _group: Option<Pgid>) -> io::Result<()> {
        Ok(())
    }

    /// See the Unix arm.
    ///
    /// # Errors
    /// Returns `Err` when the cap could not be installed.
    #[cfg(windows)]
    pub(crate) fn limit_processes(&self, group: Option<Pgid>) -> io::Result<()> {
        use super::group::{
            apply_group_active_process_limit, is_known_group, set_active_process_limit,
        };
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::JobObjects::{AssignProcessToJobObject, CreateJobObjectW};

        // A child inside a pipeline cannot join a second job: the cap lands on
        // the one it is already in.
        if let Some(leader) = group.filter(|g| is_known_group(g.as_raw())) {
            return apply_group_active_process_limit(leader.as_raw(), ACTIVE_PROCESS_CAP)
                .then_some(())
                .ok_or_else(|| io::Error::other("cannot cap the pipeline's Job Object"));
        }
        unsafe {
            let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
            if job.is_null() {
                return Err(io::Error::last_os_error());
            }
            let armed = if !set_active_process_limit(job, ACTIVE_PROCESS_CAP) {
                Err(io::Error::other("cannot cap the child's Job Object"))
            } else if AssignProcessToJobObject(job, self.raw_process_handle()) == 0 {
                Err(io::Error::other(format!(
                    "cannot place the child in a capped Job Object ({}); ral itself may be in a job that forbids nesting",
                    io::Error::last_os_error()
                )))
            } else {
                Ok(())
            };
            // The job outlives this handle: it dissolves only once its last
            // member has exited, so the cap holds for the child's whole tree.
            CloseHandle(job);
            armed
        }
    }
}
