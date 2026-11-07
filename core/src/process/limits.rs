//! What a child spawned under an active grant may consume: no core dump, and a
//! budget of live processes, a property of its launch that each platform's
//! spawn applies before the child runs.  A budget that cannot be set is a
//! refusal, never a warning: the child must not run without one.

#[cfg(target_os = "macos")]
mod fork_brake;

use super::Launch;
#[cfg(any(windows, target_os = "macos"))]
use crate::authority::sandbox::Refusal;
#[cfg(unix)]
use rustix::process::Resource;
#[cfg(unix)]
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
pub(super) const ACTIVE_PROCESS_CAP: u32 = 512;

/// Why a child under a grant was not started: `why` its budget could not be set.
#[cfg(any(windows, target_os = "macos"))]
pub(super) fn unbudgeted(why: impl std::fmt::Display) -> Refusal {
    Refusal::Unavailable(format!(
        "{why}, so this command would run with no process budget"
    ))
}

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
    /// macOS: the budget cannot be measured.
    #[cfg_attr(
        not(target_os = "macos"),
        allow(
            clippy::unnecessary_wraps,
            reason = "only macOS can fail: it measures the fork-brake budget"
        )
    )]
    pub(crate) fn limit_resources(&mut self) -> Result<(), crate::authority::sandbox::Refusal> {
        #[cfg(target_os = "macos")]
        self.limit_forks(
            fork_brake::limit()
                .map_err(|e| unbudgeted(format_args!("cannot count your processes: {e}")))?,
        );
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

/// Windows: the budget is set on the child's Job Object while the child is
/// still suspended, so it never runs without one.
#[cfg(windows)]
impl Launch {
    #[allow(
        clippy::unnecessary_wraps,
        reason = "the Unix arms can fail; on Windows a budget that cannot be set refuses the spawn"
    )]
    pub(crate) fn limit_resources(&mut self) -> Result<(), Refusal> {
        self.process_cap = true;
        Ok(())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::failure_hint;

    #[test]
    fn only_exit_128_carries_the_fork_brake_hint() {
        assert!(failure_hint(128).is_some());
        for code in [0, 1, 127, 129, 255, -1] {
            assert_eq!(failure_hint(code), None, "{code}");
        }
    }
}
