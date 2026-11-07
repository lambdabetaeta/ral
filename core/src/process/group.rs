//! Process-group placement: where a spawned child's group comes from, and who
//! signals it.
//!
//! `unix` and `windows` exist only on their own platform, so neither can be
//! linked from this page.  Windows has no pgid and no `kill(-pgid, …)`: a group
//! there is a Job Object plus a member-pid list, keyed by the leader's pid — the
//! value [`Pgid`] carries.

use std::num::NonZeroI32;

use super::cancel::{CancelCause, Scope};
#[cfg(unix)]
use super::outcome::Signal;
#[cfg(unix)]
use super::signal::KILL;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::{SpawnError, spawn_with_pgid};
#[cfg(unix)]
pub(crate) use unix::{Survivor, spawn_detached};

#[cfg(windows)]
mod windows;
#[cfg(all(windows, test))]
pub(crate) use windows::is_known_group;
#[cfg(windows)]
pub(crate) use windows::{
    PreparedGroup, close_prepared_group, prepare_group, prepared_job, register_prepared_group,
    release_win_group, set_active_process_limit,
};
#[cfg(windows)]
pub use windows::{ReapStatus, break_pipeline_group, try_reap_leader};

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
/// stages `Join`, an interactive background child `Inherit`, a headless child
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
    /// reaches the subtree.  Used by a headless child, which never takes a
    /// terminal, and by `detach`'s intermediate.
    NewSession,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}

/// Where a spawned child landed, and so what addresses it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Landing {
    /// A group of its own, or its envelope's payload's: signalled and killed
    /// whole.
    Owns(Pgid),
    /// A pipeline's, whose owner addresses it as a whole.
    Joins,
    /// ral's own, where only its pid addresses it.
    Shares,
}

impl Landing {
    /// Who signals the child: the group it owns, the `stage`'s it joined, or
    /// ral's own, where only its pid addresses it.
    pub(crate) fn group(self, stage: Option<&Membership>) -> Group {
        match (self, stage) {
            (Self::Owns(group), _) => Group::Owns(group),
            (Self::Joins, Some(stage)) => Group::Joins(stage.clone()),
            (Self::Shares, _) => Group::Shares,
            (Self::Joins, None) => unreachable!("a child joins a group only from a stage"),
        }
    }
}

/// A pipeline group joined, and the stage scope whose every cause the
/// group's owner delivers to the whole group.
#[derive(Clone, Debug)]
pub(crate) struct Membership {
    group: Pgid,
    stage: Scope,
}

impl Membership {
    pub(crate) const fn new(group: Pgid, stage: Scope) -> Self {
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
    /// ral's own: never signalled whole, so its children go by pid.
    Shares,
}

impl Group {
    /// The group signalled and killed whole, an owner's alone.
    pub(crate) const fn owned(&self) -> Option<Pgid> {
        match self {
            Self::Owns(group) => Some(*group),
            Self::Joins(_) | Self::Shares => None,
        }
    }

    /// Whether this holder's own teardown must open: always for an owner; for
    /// a member, what its stage does not hold; in ral's own group, all but the
    /// interrupt, the tty's Ctrl-C having reached that group whole.
    pub(crate) fn owes(&self, cause: CancelCause) -> bool {
        match self {
            Self::Owns(_) => true,
            Self::Joins(m) => m.owes(cause),
            Self::Shares => cause != CancelCause::Interrupted,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::cancel::DurableRoot;
    use strum::VariantArray as _;

    fn membership(stage: &Scope) -> Membership {
        Membership::new(Pgid::from_raw(1).expect("1 is positive"), stage.clone())
    }

    #[test]
    fn a_member_owes_only_what_its_stage_does_not_hold() {
        let stage = DurableRoot::default().child();
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
        let stage = DurableRoot::default().child();
        let own = stage.child();
        own.cancel(CancelCause::Terminated);
        stage.cancel(CancelCause::TimedOut);
        let member = membership(&stage);
        assert!(member.owes(CancelCause::Terminated));
        assert!(!member.owes(CancelCause::TimedOut));
    }

    /// A child's group is the one it owns, its stage's where it joined one,
    /// or ral's own.  Where it lands is its spawn's to say
    /// (`unix::tests::a_spawn_says_where_its_child_landed`).
    #[test]
    fn a_landing_names_who_signals_the_child() {
        let (seven, nine) = (
            Pgid::from_raw(7).expect("positive"),
            Pgid::from_raw(9).expect("positive"),
        );
        let stage = Membership::new(seven, Scope::clone(&DurableRoot::default().child()));
        assert!(matches!(
            Landing::Owns(nine).group(Some(&stage)),
            Group::Owns(g) if g == nine
        ));
        assert!(matches!(
            Landing::Joins.group(Some(&stage)),
            Group::Joins(m) if m.group() == seven
        ));
        assert!(matches!(Landing::Shares.group(None), Group::Shares));
    }

    #[test]
    fn an_owner_owes_every_cause() {
        let group = Group::Owns(Pgid::from_raw(1).expect("1 is positive"));
        assert!(CancelCause::VARIANTS.iter().all(|&c| group.owes(c)));
    }

    /// The tty's Ctrl-C reached ral's own group whole; every other cause is
    /// its teardown's to deliver.
    #[test]
    fn rals_own_group_owes_all_but_the_interrupt() {
        for &cause in CancelCause::VARIANTS {
            assert_eq!(
                Group::Shares.owes(cause),
                cause != CancelCause::Interrupted,
                "{cause:?}"
            );
        }
    }
}
