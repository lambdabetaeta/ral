//! Process-group lifecycle for a multi-stage pipeline, on every platform.
//!
//! An owning group spawns the anchor — `ral --ral-pipeline-anchor`, the one
//! member outliving every stage, so the pgid stays joinable; a group joining
//! an enclosing stage's pgid has no anchor.  The shell is not a member, so a
//! signal the tty delivers to the group reaches it only as the anchor's
//! [`Event::Heard`] — the kernel having already given that signal to every
//! other member — which only the group's [`TerminalLoan`] reads as a key.

use super::collect::Event;
#[cfg(unix)]
use crate::process::CancelCause;
use crate::process::{CancelScope, Group, Membership, Pgid, PgidPolicy, TerminalLoan};
use crate::types::{Break, Mooring, Settled, Shell};
use std::sync::mpsc::Sender;

/// Pgid lifecycle for one pipeline: the anchor, finished before the fold or on
/// drop.
pub(super) struct PipelineGroup {
    group: Group,
    /// `Some` exactly for an owning group until [`Self::end_anchor`].
    anchor: Option<AnchorProcess>,
}

impl PipelineGroup {
    /// An owning group: the anchor, spawned and witnessed at once, so no
    /// signal it swallows can land before someone is reading for it.
    pub(super) fn prepare(shell: &Shell, tx: Sender<Event>) -> Settled<Self> {
        let anchor = AnchorProcess::spawn(shell, tx)?;
        Ok(Self {
            group: Group::Owns(anchor.pgid),
            anchor: Some(anchor),
        })
    }

    /// A pipeline launched inside a stage thread joins the enclosing
    /// pipeline's group rather than owning one.
    pub(super) fn joining(membership: Membership) -> Self {
        Self {
            group: Group::Joins(membership),
            anchor: None,
        }
    }

    /// What a stage running under `stage` joins: an owner's leader paired
    /// with that scope, or a joiner's own membership, so a nested stage
    /// answers to the outermost owner's stage scope.
    pub(super) fn membership(&self, stage: &CancelScope) -> Membership {
        match &self.group {
            Group::Owns(leader) => Membership::new(*leader, stage.clone()),
            Group::Joins(membership) => membership.clone(),
        }
    }

    pub(super) fn group(&self) -> Group {
        self.group.clone()
    }

    pub(super) fn leader_pgid(&self) -> Pgid {
        self.group.leader()
    }

    /// Lend the terminal to the group this pipeline owns, for the run under
    /// `mooring`; a joined group is its owner's to lend.
    pub(super) fn lend(&self, shell: &Shell, mooring: &Mooring) -> Option<TerminalLoan> {
        let Group::Owns(leader) = &self.group else {
            return None;
        };
        // `resolve_terminal_plan` already gated the foreground plan on the
        // lease; re-borrowing it here is the proof `try_acquire` demands.
        TerminalLoan::try_acquire(
            leader.as_raw(),
            shell.terminal_lease(mooring)?,
            &mooring.cancel,
        )
    }

    /// Finish the anchor, after which every report it made is on the channel.
    pub(super) fn end_anchor(&mut self) {
        if let Some(anchor) = self.anchor.take() {
            anchor.finish();
        }
    }
}

impl Drop for PipelineGroup {
    /// The anchor last, after every stage handle has gone — `PipeNode`'s field
    /// order guarantees it.
    fn drop(&mut self) {
        self.end_anchor();
        #[cfg(windows)]
        if let Group::Owns(leader) = &self.group {
            crate::process::release_win_group(leader.as_raw());
        }
    }
}

struct AnchorProcess {
    /// Held for its `Drop` alone, which reaps: nothing reads it back.
    #[cfg(unix)]
    child: crate::process::Watch,
    #[cfg(windows)]
    child: crate::process::ChildHandle,
    pgid: Pgid,
    /// The anchor reads this to EOF; closing it is how `finish` ends it.
    release: os_pipe::PipeWriter,
    /// Blocks reading the anchor's stdout — one byte per swallowed signal —
    /// to EOF.
    #[cfg(unix)]
    reporter: std::thread::JoinHandle<()>,
}

fn anchor_error(e: impl std::fmt::Display) -> Break {
    Break::Error(crate::types::Error::new(format!("pipeline anchor: {e}"), 1))
}

/// The anchor's own death cancels the pipeline: nothing else in the group has
/// heard of it.
#[cfg(unix)]
fn anchor_death(_: crate::process::WaitOutcome) -> Event {
    Event::Cancelled(CancelCause::Terminate)
}

impl AnchorProcess {
    /// Spawn the anchor and start its witness in one act: the report reader
    /// and the reaper's `Watch` on the anchor's pid are both live on `tx`
    /// before the pgid is handed to any stage.
    // Taken by value because the Unix arm below moves it into the watch; the
    // Windows arm has no reader thread to give it to.
    #[cfg_attr(windows, allow(clippy::needless_pass_by_value))]
    fn spawn(shell: &Shell, tx: Sender<Event>) -> Settled<Self> {
        #[cfg(windows)]
        let _ = tx;
        let (reader, release) = crate::process::cloexec_pipe().map_err(anchor_error)?;
        let mut cmd =
            super::helper::self_reexec(super::helper::ANCHOR_FLAG).map_err(anchor_error)?;
        cmd.stdin(crate::process::StdioSpec::from_pipe_reader(reader));
        cmd.stderr(crate::process::StdioSpec::null());
        #[cfg(unix)]
        let report = {
            let (report, writer) = crate::process::cloexec_pipe().map_err(anchor_error)?;
            cmd.stdout(crate::process::StdioSpec::from_pipe_writer(writer));
            report
        };
        #[cfg(windows)]
        cmd.stdout(crate::process::StdioSpec::null());
        let (mut child, leader, _jail) = cmd.spawn(PgidPolicy::NewLeader).map_err(anchor_error)?;
        if shell.has_active_capabilities() {
            crate::sandbox::apply_child_limits(&child);
        }
        let Some(pgid) = leader else {
            // `Child::drop` neither kills nor reaps.
            let _ = child.kill();
            let _ = child.reap();
            return Err(anchor_error("failed to establish a process group"));
        };
        #[cfg(unix)]
        let (child, reporter) = {
            let report_tx = tx.clone();
            let reporter = std::thread::Builder::new()
                .name("ral pipeline anchor report reader".to_string())
                .spawn(move || read_anchor_reports(report, &report_tx))
                .map_err(anchor_error)?;
            (child.into_watch(tx, anchor_death), reporter)
        };
        Ok(Self {
            child,
            pgid,
            release,
            #[cfg(unix)]
            reporter,
        })
    }

    /// The reader leaves only on EOF, the anchor's own exit closing the write
    /// end, so the anchor can never `SIGPIPE` on a report.
    #[cfg(unix)]
    fn finish(self) {
        let Self {
            release,
            reporter,
            child,
            ..
        } = self;
        drop(release);
        let _ = reporter.join();
        drop(child);
    }

    #[cfg(windows)]
    fn finish(self) {
        let Self {
            mut child, release, ..
        } = self;
        drop(release);
        let _ = child.reap();
    }
}

/// One byte per swallowed signal — one the kernel already delivered to every
/// other member — sent raw: only the loan knows whether it was a key.  EOF
/// means the anchor exited, which its own watch reports instead.
#[cfg(unix)]
fn read_anchor_reports(mut report: os_pipe::PipeReader, tx: &Sender<Event>) {
    use std::io::Read;
    let mut byte = [0u8; 1];
    while report.read_exact(&mut byte).is_ok() {
        let signal = crate::process::Signal::new(i32::from(byte[0]));
        if tx.send(Event::Heard(signal)).is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An owning group whose channel nobody reads — enough for every test
    /// that only wants a real pgid.
    fn prepared(shell: &Shell) -> PipelineGroup {
        PipelineGroup::prepare(shell, std::sync::mpsc::channel().0).expect("anchor spawns")
    }

    #[test]
    fn prepare_yields_a_leader_on_every_platform() {
        let group = prepared(&Shell::default());
        assert!(matches!(group.group(), Group::Owns(_)));
        assert!(group.leader_pgid().as_raw() > 0);
    }

    /// An owner pairs its leader with the scope it is given; a joiner hands
    /// back its own membership whatever scope it is given.
    #[test]
    fn a_nested_stage_answers_to_the_outermost_owners_stage() {
        let owner = prepared(&Shell::default());
        let outer = CancelScope::root();
        let membership = owner.membership(&outer);
        assert_eq!(membership.group(), owner.leader_pgid());

        let joiner = PipelineGroup::joining(membership);
        let inner = CancelScope::root();
        let nested = joiner.membership(&inner);
        assert_eq!(nested.group(), owner.leader_pgid());
        #[cfg(unix)]
        {
            outer.cancel(CancelCause::Interrupt);
            assert!(
                !nested.owes(CancelCause::Interrupt),
                "a nested stage's membership must read the outer stage's scope"
            );
            inner.cancel(CancelCause::Deadline);
            assert!(
                nested.owes(CancelCause::Deadline),
                "the inner scope must not stand in for the outer one"
            );
        }
    }

    /// A default shell mints no terminal lease, so `lend` lends nothing.
    #[test]
    fn a_group_without_a_lease_lends_nothing() {
        let shell = Shell::default();
        let group = prepared(&shell);
        assert!(group.lend(&shell, &Mooring::adrift()).is_none());
    }

    #[cfg(windows)]
    #[test]
    fn a_completed_group_releases_its_windows_job() {
        let group = prepared(&Shell::default());
        let leader = group.leader_pgid().as_raw();
        assert!(crate::process::is_known_group(leader));
        drop(group);
        assert!(!crate::process::is_known_group(leader));
    }

    #[cfg(unix)]
    fn heard_within_2s(rx: &std::sync::mpsc::Receiver<Event>) -> Option<i32> {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Event::Heard(signal)) => Some(signal.number()),
            Ok(_) | Err(_) => None,
        }
    }

    /// A signal the group swallows comes back raw as `Heard`, key or not; two
    /// in a row prove the anchor survived the first.
    #[cfg(unix)]
    #[test]
    fn a_signalled_anchor_reports_the_signal_and_lives_on() {
        let shell = Shell::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let group = PipelineGroup::prepare(&shell, tx).expect("anchor spawns");
        assert!(
            rx.recv_timeout(std::time::Duration::from_millis(200))
                .is_err(),
            "an unsignalled anchor must report nothing"
        );
        // Past the window between the anchor's exec and its handler install.
        std::thread::sleep(std::time::Duration::from_millis(300));
        for signal in [libc::SIGINT, libc::SIGTERM] {
            group
                .leader_pgid()
                .signal_group(crate::process::Signal::new(signal));
            match heard_within_2s(&rx) {
                Some(n) if n == signal => {}
                Some(n) => panic!("signal {signal} heard as {n}"),
                None => panic!("signal {signal} was never reported"),
            }
        }
    }
}
