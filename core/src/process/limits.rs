//! What a child spawned under an active grant may consume: no core dump, and a
//! budget of live processes, a property of its launch that each platform's
//! spawn applies before the child runs.  A budget that cannot be set is a
//! refusal, never a warning: the child must not run without one.

#[cfg(target_os = "macos")]
mod fork_brake;

use super::Launch;
use crate::error::Error;
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

/// Advice for a grant-confined command that exited with `code`: on macOS,
/// status 128 may be a fork the process limit refused. Other platforms
/// impose no such limit at launch, and have nothing to say.
#[cfg_attr(
    not(target_os = "macos"),
    allow(unused_variables, reason = "only macOS sets this limit at launch")
)]
pub(crate) fn failure_hint(code: i32) -> Option<String> {
    #[cfg(target_os = "macos")]
    if code == 128 {
        return Some(format!(
            "this command ran with a process limit: your process count at launch plus \
         {ACTIVE_PROCESS_CAP}, capped by the system maximum. Exit status 128 can accompany \
         a refused fork, but does not prove one. If the command reported that it could not \
         fork, reduce concurrent processes and retry; widening fs or exec grants does not \
         raise this limit."
        ));
    }
    None
}

#[cfg(unix)]
impl ChildLimits {
    pub(super) fn checks(
        evidence: &mut super::launch::setup::Evidence,
    ) -> [super::launch::setup::Check; 3] {
        use crate::fact::{PrepareStep, PrepareSubject, SessionLimit};
        [
            SessionLimit::OpenFiles,
            SessionLimit::CoreBytes,
            SessionLimit::StackBytes,
        ]
        .map(|limit| {
            evidence.check(super::launch::setup::Context::Prepare(
                PrepareStep::SessionLimit,
                PrepareSubject::SessionLimit(limit),
            ))
        })
    }

    /// Only getrlimit/setrlimit; no allocation or locks after fork.
    pub(super) fn apply_checked(
        mut self,
        checks: &[super::launch::setup::Check; 3],
    ) -> io::Result<()> {
        use rustix::process::{getrlimit, setrlimit};
        for (limit, check) in ChildLimit::ALL.into_iter().zip(checks) {
            if let Some(value) = *(limit.slot)(&mut self) {
                let mut current = getrlimit(limit.resource);
                current.current = Some(value);
                check.run(setrlimit(limit.resource, current).map_err(Into::into))?;
            }
        }
        Ok(())
    }
}

/// Mandatory grant limits, with the process budget measured in the parent.
#[cfg(unix)]
#[derive(Clone, Copy)]
pub(super) struct GrantLimits {
    #[cfg(target_os = "macos")]
    pub(super) forks: libc::rlim_t,
}

#[cfg(unix)]
impl GrantLimits {
    pub(super) fn checks(
        evidence: &mut super::launch::setup::Evidence,
    ) -> [super::launch::setup::Check; 2] {
        use crate::fact::{GrantLimit, PrepareStep, PrepareSubject};
        [GrantLimit::Processes, GrantLimit::CoreBytes].map(|limit| {
            evidence.check(super::launch::setup::Context::Prepare(
                PrepareStep::GrantLimit,
                PrepareSubject::GrantLimit(limit),
            ))
        })
    }

    /// Mandatory ceilings follow session limits.
    #[cfg_attr(
        not(target_os = "macos"),
        expect(
            clippy::unused_self,
            reason = "only macOS consumes the parent-measured fork budget"
        )
    )]
    pub(super) fn apply_checked(self, checks: &[super::launch::setup::Check; 2]) -> io::Result<()> {
        #[cfg(target_os = "macos")]
        checks[0].run(set_limit(Resource::Nproc, self.forks))?;
        checks[1].run(set_limit(Resource::Core, 0))
    }
}

#[cfg(unix)]
impl Launch {
    /// Describe session soft limits; installation always precedes grant limits.
    pub(crate) fn limit_session(&mut self, limits: ChildLimits) {
        self.setup.session_limits = limits;
    }

    /// Prepare mandatory limits: no core dumps and, on macOS, a fork brake.
    /// Darwin compares the user's process count with the *forking* process's
    /// own `RLIMIT_NPROC`, so this caps its descendants, not the parent's.
    /// Measure the budget here; only setrlimit runs in the child.
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
    pub(crate) fn limit_resources(&mut self) -> Result<(), Error> {
        self.setup.grant_limits = Some(GrantLimits {
            #[cfg(target_os = "macos")]
            forks: fork_brake::limit().map_err(|e| {
                super::launch::refused(crate::fact::LaunchFailure::Budget(
                    crate::fact::BudgetFailure::CountProcesses(e),
                ))
            })?,
        });
        Ok(())
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
    pub(crate) fn limit_resources(&mut self) -> Result<(), Error> {
        self.process_cap = true;
        Ok(())
    }
}
