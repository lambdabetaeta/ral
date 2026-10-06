//! Process-group placement: where a spawned child's group comes from, and who
//! signals it.
//!
//! `unix` and `windows` exist only on their own platform, so neither can be
//! linked from this page.  Windows has no pgid and no `kill(-pgid, …)`: a group
//! there is a Job Object plus a member-pid list, keyed by the leader's pid — the
//! value [`Pgid`] carries.

use std::num::NonZeroI32;

use super::cancel::{CancelCause, CancelScope};
#[cfg(unix)]
use super::outcome::Signal;
#[cfg(unix)]
use super::signal::KILL;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::spawn_detached;
#[cfg(unix)]
pub use unix::{spawn_with_pgid, spawn_with_pgid_after};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::{
    PreparedGroup, apply_group_active_process_limit, close_prepared_group, is_known_group,
    prepare_group, prepared_job, register_prepared_group, release_win_group,
    set_active_process_limit, wait_leader_blocking,
};
#[cfg(windows)]
pub use windows::{ReapStatus, break_pipeline_group, disown_pipeline_group, try_reap_leader};

/// A process-group identifier.
///
/// On Unix a POSIX pgid — the leader's pid, addressable as `kill(-pgid, sig)`
/// to reach every member.  Windows console groups cannot be joined post-spawn,
/// so every external stage leads its own and this carries the *first* stage's
/// pid: the key under which the `windows` module registers the member list and
/// the Job Object that `TerminateJobObject` takes down as one.
///
/// Positive by construction; "no pgid" is `Option<Pgid>`, never a sentinel.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Pgid(NonZeroI32);

impl Pgid {
    #[cfg_attr(not(any(target_os = "linux", windows, test)), allow(dead_code))]
    pub(crate) fn from_raw(raw: i32) -> Option<Self> {
        NonZeroI32::new(raw)
            .filter(|raw| raw.is_positive())
            .map(Self)
    }

    pub(crate) const fn as_raw(self) -> i32 {
        self.0.get()
    }

    #[cfg(unix)]
    pub(crate) const fn from_pid(pid: rustix::process::Pid) -> Self {
        Self(pid.as_raw_nonzero())
    }

    #[cfg(unix)]
    pub(crate) const fn as_pid(self) -> rustix::process::Pid {
        // SAFETY: every `Pgid` constructor admits only positive integers.
        unsafe { rustix::process::Pid::from_raw_unchecked(self.as_raw()) }
    }

    /// `SIGKILL` every member — the Job Object's kill on Windows.  Idempotent,
    /// and harmless on a group that has already left.
    pub(crate) fn kill(self) {
        #[cfg(unix)]
        self.signal_group(KILL);
        #[cfg(windows)]
        windows::kill_pipeline_group(self);
    }
}

impl std::fmt::Display for Pgid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(unix)]
impl Pgid {
    /// Send `signal` to every process in this group via `kill(-pgid, sig)`.
    ///
    /// Async-signal-safe: one libc call, no allocation, no locking.  Failure is
    /// ignored — the pipeline-abort and Ctrl-Z callers have no recovery, and
    /// `ESRCH` on an already-empty group is the outcome they wanted anyway.
    pub(crate) fn signal_group(self, signal: Signal) {
        unsafe {
            libc::kill(-self.as_raw(), signal.number());
        }
    }
}

/// Where a spawned child's process group comes from.
///
/// Foreground job leaders and pipeline first stages take `NewLeader`, later
/// stages `Join`, an interactive background child `Inherit`, a detached worker
/// `NewSession`.  Unix applies the choice in `pre_exec` before `execve`;
/// Windows maps it onto a Job Object at the launch boundary.
#[derive(Clone, Copy, Debug)]
pub enum PgidPolicy {
    /// Inherit the parent's pgid — no `setpgid` call.
    Inherit,
    /// Lead a fresh group (`setpgid(0, 0)`), keeping the parent's session and
    /// controlling terminal.
    NewLeader,
    /// Lead a fresh *session* (`setsid`): with no controlling terminal, the
    /// child cannot signal — through the shared tty or `tcgetpgrp` — whatever
    /// owns one.  Its pgid still equals its pid, so `kill(-pgid, …)` still
    /// reaches the subtree.
    NewSession,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}

impl PgidPolicy {
    /// Whether the child leads the group it lands in.
    pub(crate) const fn leads(self) -> bool {
        matches!(self, Self::NewLeader | Self::NewSession)
    }
}

/// A pipeline group joined, and the stage scope whose every cause the
/// group's owner delivers to the whole group.
#[derive(Clone, Debug)]
pub(crate) struct Membership {
    group: Pgid,
    stage: CancelScope,
}

impl Membership {
    pub(crate) const fn new(group: Pgid, stage: CancelScope) -> Self {
        Self { group, stage }
    }

    pub(crate) const fn group(&self) -> Pgid {
        self.group
    }

    /// Only what struck beneath the stage is the member's own to open.
    pub(crate) fn owes(&self, cause: CancelCause) -> bool {
        self.stage.cause() < Some(cause)
    }
}

/// The process group a child or pipeline landed in, and so who signals it.
#[derive(Clone, Debug)]
pub(crate) enum Group {
    /// Its own: signalled and killed whole.
    Owns(Pgid),
    /// A pipeline's, whose owner delivers what the stage holds.
    Joins(Membership),
}

impl Group {
    pub(crate) const fn leader(&self) -> Pgid {
        match self {
            Self::Owns(g) => *g,
            Self::Joins(m) => m.group,
        }
    }

    /// Whether this holder's own teardown must open: always for an owner.
    pub(crate) fn owes(&self, cause: CancelCause) -> bool {
        match self {
            Self::Owns(_) => true,
            Self::Joins(m) => m.owes(cause),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray as _;

    fn membership(stage: &CancelScope) -> Membership {
        Membership::new(Pgid::from_raw(1).expect("1 is positive"), stage.clone())
    }

    #[test]
    fn a_member_owes_only_what_its_stage_does_not_hold() {
        let stage = CancelScope::root();
        let member = membership(&stage);
        assert!(
            CancelCause::VARIANTS.iter().all(|&c| member.owes(c)),
            "an unstruck stage leaves every cause to the member"
        );

        stage.cancel(CancelCause::TimedOut);
        for cause in [
            CancelCause::ReaderGone,
            CancelCause::Interrupted,
            CancelCause::Cancelled,
            CancelCause::TimedOut,
        ] {
            assert!(!member.owes(cause), "{cause:?} is the owner's to deliver");
        }

        let own = stage.child();
        own.cancel(CancelCause::Terminated);
        let cause = own.cause().expect("struck");
        assert!(
            member.owes(cause),
            "a cause beyond the stage's is the member's"
        );
    }

    #[test]
    fn strike_order_does_not_change_what_a_member_owes() {
        let stage = CancelScope::root();
        let own = stage.child();
        own.cancel(CancelCause::Terminated);
        stage.cancel(CancelCause::TimedOut);
        let member = membership(&stage);
        assert!(member.owes(CancelCause::Terminated));
        assert!(!member.owes(CancelCause::TimedOut));
    }

    #[test]
    fn an_owner_owes_every_cause() {
        let group = Group::Owns(Pgid::from_raw(1).expect("1 is positive"));
        assert!(CancelCause::VARIANTS.iter().all(|&c| group.owes(c)));
    }
}
