//! Process-group lifecycle for a multi-stage pipeline, on every platform.
//!
//! An owning group spawns the anchor — `ral --ral-pipeline-anchor`, the one
//! member outliving every stage, so the pgid stays joinable; a group joining
//! an enclosing stage's pgid has no anchor and may neither signal nor kill it.
//! The shell is not a member, so a signal the tty delivers to the group
//! reaches it only as the anchor's [`Event::Witnessed`] — the kernel having
//! already given that signal to every other member.

use super::collect::Event;
#[cfg(unix)]
use crate::process::CancelCause;
use crate::process::{Pgid, PgidPolicy};
use crate::types::{Break, Mooring, Settled, Shell};
use std::sync::mpsc::Sender;

/// Pgid lifecycle for one pipeline: the anchor and the foreground guard,
/// released together on drop.
pub(super) struct PipelineGroup {
    leader: Pgid,
    /// The terminal handoff *actually held*: `None` when `tcsetpgrp` failed,
    /// or when this group never claimed it at all.
    foreground: Option<crate::process::ForegroundGuard>,
    /// `Some` exactly for an owning group; taken by `Drop`.
    anchor: Option<AnchorProcess>,
}

impl PipelineGroup {
    /// An owning group: the anchor, spawned and witnessed at once, so no
    /// signal it swallows can land before someone is reading for it.
    pub(super) fn prepare(shell: &Shell, tx: Sender<Event>) -> Settled<Self> {
        let anchor = AnchorProcess::spawn(shell, tx)?;
        Ok(Self {
            leader: anchor.pgid,
            foreground: None,
            anchor: Some(anchor),
        })
    }

    /// A pipeline launched inside a stage thread joins `group` rather than
    /// owning one.
    pub(super) fn joining(group: Pgid) -> Self {
        Self {
            leader: group,
            foreground: None,
            anchor: None,
        }
    }

    /// Owned iff the anchor is this group's own: a joining group may not
    /// signal, kill, or claim the foreground on its own account.
    pub(super) fn owned(&self) -> bool {
        self.anchor.is_some()
    }

    /// Whether this group actually holds the controlling terminal — the guard
    /// it acquired, never the plan it was launched under.  Settled before any
    /// stage exists, so a stage's stdin or stdout may be routed against it.
    pub(super) fn holds_terminal(&self) -> bool {
        self.foreground.is_some()
    }

    pub(super) fn leader_pgid(&self) -> Pgid {
        self.leader
    }

    pub(super) fn spawn(
        &self,
        cmd: &mut crate::process::Launch,
    ) -> std::io::Result<(
        crate::process::ChildHandle,
        Option<crate::process::jail::JailCgroup>,
    )> {
        let (child, _leader, jail) = cmd.spawn(PgidPolicy::Join(self.leader))?;
        Ok((child, jail))
    }

    /// Called only when the pipeline's frozen `TerminalPlan` wants foreground
    /// — `launch::PipelineBuild::new`'s own gate, not repeated here.
    pub(super) fn claim_foreground(&mut self, shell: &Shell, mooring: &Mooring) {
        // `resolve_terminal_plan` already gated the foreground plan on the
        // lease; re-borrowing it here is the proof `try_acquire` demands.
        if let Some(lease) = shell.terminal_lease(mooring) {
            self.foreground =
                crate::process::ForegroundGuard::try_acquire(self.leader.as_raw(), lease);
        }
    }

    /// Send `signal` to the whole pgid, then `SIGCONT` — a stopped member
    /// cannot act on the first until it runs.  Nothing for a joining group,
    /// whose pgid is its owner's to address.
    #[cfg(unix)]
    pub(super) fn signal(&self, signal: crate::process::Signal) {
        if !self.owned() {
            return;
        }
        self.leader.signal_group(signal);
        self.leader
            .signal_group(crate::process::Signal::new(libc::SIGCONT));
    }

    /// SIGKILL the pgid — the Job Object's kill on Windows.  Idempotent, and
    /// nothing for a joining group, whose pgid is its owner's to end.
    ///
    /// After this returns nothing in the group holds a pipe end open: a
    /// stage's own kill reaches its pid alone, so only the owner can make a
    /// pump's join terminate.
    pub(super) fn kill(&self) {
        if !self.owned() {
            return;
        }
        self.leader.kill();
    }
}

impl Drop for PipelineGroup {
    /// The anchor last, after every stage handle has gone (`PipelineBuild`
    /// and `PipeNode` both order their fields to guarantee it).
    fn drop(&mut self) {
        // The Windows release sits inside this arm: the ownership fact it
        // would otherwise be guarded on is what `take` has just consumed.
        let Some(anchor) = self.anchor.take() else {
            return;
        };
        anchor.finish();
        #[cfg(windows)]
        crate::process::release_win_group(self.leader.as_raw());
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

/// The cause the shell's own handler for `signal` would apply.
#[cfg(unix)]
fn cancel_cause(signal: i32) -> CancelCause {
    match signal {
        libc::SIGINT => CancelCause::Interrupt,
        libc::SIGQUIT => CancelCause::RootAbort,
        _ => CancelCause::Terminate,
    }
}

/// The anchor's own death cancels the pipeline: nothing else in the group has
/// heard of it.
#[cfg(unix)]
fn anchor_death(outcome: crate::process::WaitOutcome) -> Event {
    Event::Cancelled(match outcome {
        crate::process::WaitOutcome::Signaled(sig) => cancel_cause(sig.number()),
        _ => CancelCause::Terminate,
    })
}

impl AnchorProcess {
    /// Spawn the anchor and start its witness in one act: the report reader
    /// and the reaper's `Watch` on the anchor's pid are both live on `tx`
    /// before the pgid is handed to any stage.
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

    /// Close the release pipe, join the report reader, drop the watch — whose
    /// own `Drop` reaps.  The reader leaves only on EOF, the anchor's own exit
    /// closing the write end, so the anchor can never `SIGPIPE` on a report.
    /// POSIX will not recycle a leader's pid while the group is non-empty, so
    /// the pgid stays addressable meanwhile.
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

/// Block-read the anchor's report pipe to EOF, one swallowed signal at a
/// time.  Each is a signal the kernel already delivered to every other member,
/// so the collector tears down without re-sending it.  EOF means the anchor
/// died, which its own watch reports with the cause a bare EOF cannot carry.
#[cfg(unix)]
fn read_anchor_reports(mut report: os_pipe::PipeReader, tx: &Sender<Event>) {
    use std::io::Read;
    let mut byte = [0u8; 1];
    loop {
        match report.read(&mut byte) {
            Ok(1) => {
                let cause = cancel_cause(i32::from(byte[0]));
                if tx.send(Event::Witnessed(cause)).is_err() {
                    return;
                }
            }
            _ => return,
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
        assert!(group.owned());
        assert!(group.leader_pgid().as_raw() > 0);
    }

    /// A default shell mints no terminal lease, so `claim_foreground` acquires
    /// nothing even when called.
    #[test]
    fn a_group_that_never_acquired_the_terminal_does_not_claim_it() {
        let shell = Shell::default();
        let mut group = prepared(&shell);
        group.claim_foreground(&shell, &Mooring::adrift());
        assert!(!group.holds_terminal());
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
    fn witnessed_within_2s(rx: &std::sync::mpsc::Receiver<Event>) -> Option<CancelCause> {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Event::Witnessed(cause)) => Some(cause),
            Ok(_) | Err(_) => None,
        }
    }

    /// A signal the group swallows comes back as `Witnessed`; two in a row
    /// prove the anchor survived the first.
    #[cfg(unix)]
    #[test]
    fn a_signalled_anchor_reports_the_cause_and_lives_on() {
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
        for (signal, cause) in [
            (libc::SIGINT, CancelCause::Interrupt),
            (libc::SIGTERM, CancelCause::Terminate),
        ] {
            group
                .leader_pgid()
                .signal_group(crate::process::Signal::new(signal));
            match witnessed_within_2s(&rx) {
                Some(c) if c == cause => {}
                Some(c) => panic!("signal {signal} witnessed as {c:?}"),
                None => panic!("signal {signal} was never reported"),
            }
        }
    }
}
