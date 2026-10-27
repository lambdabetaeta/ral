//! What a child spawned under an active grant may consume: no core dump, and a
//! budget of live processes.  A brake that cannot be armed is an `Err`, never a
//! warning: the child must not run unbraked.

#[cfg(target_os = "macos")]
mod fork_brake;

#[cfg(unix)]
use super::Launch;
use super::{ChildHandle, Pgid};
#[cfg(unix)]
use rustix::process::Resource;
use std::io;

/// Soft resource ceilings inherited by every child in a session.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildLimits {
    pub open_files: Option<u64>,
    pub core_bytes: Option<u64>,
    pub stack_bytes: Option<u64>,
}

/// An rc limit's name, resource and storage.
#[cfg(unix)]
#[derive(Clone, Copy)]
pub struct ChildLimit {
    pub name: &'static str,
    pub resource: Resource,
    pub slot: fn(&mut ChildLimits) -> &mut Option<u64>,
}

#[cfg(unix)]
impl ChildLimit {
    pub const ALL: [Self; 3] = [
        Self {
            name: "open-files",
            resource: Resource::Nofile,
            slot: |limits| &mut limits.open_files,
        },
        Self {
            name: "core-bytes",
            resource: Resource::Core,
            slot: |limits| &mut limits.core_bytes,
        },
        Self {
            name: "stack-bytes",
            resource: Resource::Stack,
            slot: |limits| &mut limits.stack_bytes,
        },
    ];
}

/// How many live processes a grant-confined child may add: the fork-bomb cap.
/// A Job Object limit on Windows; on macOS, headroom over the user's count at launch.
#[cfg(any(windows, target_os = "macos"))]
const ACTIVE_PROCESS_CAP: u32 = 512;

#[cfg(target_os = "macos")]
pub(crate) fn failure_hint(code: i32) -> Option<String> {
    (code == 128).then(|| {
        format!(
            "this command ran with a process limit: your process count at launch plus \
         {ACTIVE_PROCESS_CAP}, capped by the system maximum. Exit status 128 can accompany \
         a refused fork, but does not prove one. If the command reported that it could not \
         fork, reduce concurrent processes and retry; widening fs or exec grants does not \
         raise this limit."
        )
    })
}

#[cfg(unix)]
impl Launch {
    /// Set session soft limits, preserving the child's hard limits.
    pub(crate) fn limit_session(&mut self, mut limits: ChildLimits) {
        use rustix::process::{getrlimit, setrlimit};
        use std::os::unix::process::CommandExt;
        let limits = ChildLimit::ALL.map(|limit| (limit.resource, *(limit.slot)(&mut limits)));
        if limits.iter().all(|(_, value)| value.is_none()) {
            return;
        }
        // SAFETY: getrlimit and setrlimit are async-signal-safe.
        unsafe {
            self.cmd.pre_exec(move || {
                for (resource, value) in limits {
                    if let Some(value) = value {
                        let mut limit = getrlimit(resource);
                        limit.current = Some(value);
                        setrlimit(resource, limit)?;
                    }
                }
                Ok(())
            });
        }
    }

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
        #[cfg(target_os = "macos")]
        let nproc = fork_brake::limit()
            .map_err(|e| io::Error::other(format!("cannot count your processes: {e}")))?;
        #[cfg(target_os = "macos")]
        self.limit_forks(nproc);
        use std::os::unix::process::CommandExt;
        unsafe {
            self.cmd
                .pre_exec(|| set_limit(rustix::process::Resource::Core, 0));
        }
        Ok(())
    }

    #[cfg(target_os = "macos")]
    fn limit_forks(&mut self, limit: libc::rlim_t) {
        use std::os::unix::process::CommandExt;
        unsafe {
            self.cmd
                .pre_exec(move || set_limit(rustix::process::Resource::Nproc, limit));
        }
    }
}

#[cfg(unix)]
fn set_limit(resource: rustix::process::Resource, limit: u64) -> io::Result<()> {
    rustix::process::setrlimit(
        resource,
        rustix::process::Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        },
    )
    .map_err(Into::into)
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
