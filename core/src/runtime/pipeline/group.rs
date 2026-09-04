//! Process-group lifecycle for a multi-stage pipeline, on every platform.
//!
//! An owning group ([`PipelineGroup::prepare`]) spawns the anchor — `ral
//! --ral-pipeline-anchor`, the one member that outlives every stage, so the
//! pgid is joinable for the pipeline's whole life — then installs the SIGINT
//! relay and, when the plan says so, claims the foreground.  Every external
//! spawned anywhere inside a stage joins this pgid.  A pipeline launched
//! inside a stage thread [`PipelineGroup::joining`]s the enclosing pgid
//! instead: no anchor, no relay, no foreground, and it may not signal or kill
//! the group.
//!
//! The anchor is also the group's witness.  The shell is not a member of the
//! pgid, so while the terminal belongs to the group the shell never hears a
//! Ctrl-C the tty delivers to it: the anchor swallows every termination
//! signal and reports its number on its stdout — a pipe one dedicated thread
//! blocks reading, posting [`Event::Cancelled`] on the collector's channel.
//! The anchor's own death comes through the reaper like any watched child's,
//! via the same [`crate::process::Watch`] its own `into_watch` mints, so no
//! dedicated thread waits on it. The anchor additionally ignores
//! `SIGTSTP`/`SIGTTIN`/`SIGTTOU` outright, so it rarely stops at all; on the
//! rare stop it does see (a bare `kill -STOP`, say) the reaper answers with
//! `SIGCONT` itself, the one rule applied to the anchor like any other
//! watched pid.

#[cfg(unix)]
use super::collect::Event;
use crate::process::{CancelCause, Pgid, PgidPolicy};
use crate::types::{Break, Mooring, Settled, Shell};
#[cfg(unix)]
use std::sync::mpsc::Sender;

/// Pgid lifecycle for one pipeline: the anchor, the foreground guard and the
/// SIGINT relay slot, all released together on drop.
pub(super) struct PipelineGroup {
    /// Whether this group owns the pgid (the anchor spawned it) or joined an
    /// enclosing stage's: a joining group may not signal, kill, or claim the
    /// foreground on its own account.
    owned: bool,
    leader: Pgid,
    /// The terminal handoff *actually held*: `None` when `tcsetpgrp` failed,
    /// or when this group never claimed it at all.
    foreground: Option<crate::process::ForegroundGuard>,
    /// Held for its `Drop` alone: nothing reads it back, but it must outlive
    /// every stage so a relayed Ctrl-C keeps reaching the group.
    #[cfg(unix)]
    #[allow(dead_code)]
    relay: Option<crate::process::PipelineRelay>,
    /// `Some` exactly when `owned`; taken by `Drop`.
    anchor: Option<AnchorProcess>,
}

impl PipelineGroup {
    /// An owning group: the anchor, then the relay — safe to install now
    /// because the anchor is a member that no relayed signal can remove.
    pub(super) fn prepare(shell: &Shell) -> Settled<Self> {
        let anchor = AnchorProcess::spawn(shell)?;
        let leader = anchor.pgid;
        Ok(Self {
            owned: true,
            leader,
            foreground: None,
            #[cfg(unix)]
            relay: crate::process::PipelineRelay::install(leader.as_raw()),
            anchor: Some(anchor),
        })
    }

    /// A pipeline launched inside a stage thread joins `group` rather than
    /// owning one.
    pub(super) fn joining(group: Pgid) -> Self {
        Self {
            owned: false,
            leader: group,
            foreground: None,
            #[cfg(unix)]
            relay: None,
            anchor: None,
        }
    }

    pub(super) fn owned(&self) -> bool {
        self.owned
    }

    /// Whether this group actually holds the controlling terminal — the guard
    /// it acquired, never the plan it was launched under.  `claim_foreground`
    /// runs before any stage exists, so this is settled by the time a stage's
    /// stdin or stdout is routed.
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
        if self.foreground.is_none()
            && let Some(lease) = shell.terminal_lease(mooring)
        {
            self.foreground =
                crate::process::ForegroundGuard::try_acquire(self.leader.as_raw(), lease);
        }
    }

    /// The owner's cause signal: `SIGINT` for `Interrupt`, else `SIGTERM`,
    /// then `SIGCONT` — a stopped member cannot act on either until it runs.
    /// Nothing on Windows, whose only group verb is [`Self::kill`].
    pub(super) fn signal(&self, cause: CancelCause) {
        if !self.owned() {
            return;
        }
        #[cfg(unix)]
        {
            self.leader
                .signal_group(crate::process::Signal::new(crate::process::cause_signal(cause)));
            self.leader.signal_group(crate::process::Signal::new(libc::SIGCONT));
        }
        #[cfg(windows)]
        let _ = cause;
    }

    /// SIGKILL the pgid — the Job Object's kill on Windows.  Idempotent, and
    /// nothing for a joining group, whose pgid is its owner's to end.
    ///
    /// After this returns, nothing in the group holds a pipe end open.  That
    /// is the precondition `cancel_all`'s observation rests on: a stage's
    /// own kill reaches its pid alone, so only the owner can make a pump's
    /// join terminate.
    pub(super) fn kill(&self) {
        if !self.owned() {
            return;
        }
        self.leader.kill();
    }

    /// Start this group's anchor witness — a no-op on a joining group, which
    /// has no anchor of its own.  Called once the collector's channel
    /// exists, since the anchor spawns before it does ([`Self::prepare`]
    /// runs before [`super::collect::CollectState::new`]).
    #[cfg(unix)]
    pub(super) fn start_witness(&mut self, tx: Sender<Event>) {
        if let Some(anchor) = self.anchor.as_mut() {
            anchor.start_witness(tx);
        }
    }
}

impl Drop for PipelineGroup {
    /// The anchor last, after every stage handle has gone (`PipelineResources`
    /// and `PipeNode` both order their fields to guarantee it).
    fn drop(&mut self) {
        // The Windows group release lives inside this arm, so it cannot be
        // guarded on an ownership fact this same statement has consumed.
        let Some(anchor) = self.anchor.take() else {
            return;
        };
        anchor.finish();
        #[cfg(windows)]
        crate::process::release_win_group(self.leader.as_raw());
    }
}

/// The anchor's own wait, on Unix: spawned and not yet witnessed, or handed
/// off to the reaper's `Watch` once [`AnchorProcess::start_witness`] runs.
#[cfg(unix)]
enum Anchored {
    Spawned(crate::process::ChildHandle),
    /// Held for its `Drop` alone, which reaps: nothing reads it back.
    Watched(#[allow(dead_code)] crate::process::Watch),
}

struct AnchorProcess {
    #[cfg(unix)]
    child: Option<Anchored>,
    #[cfg(windows)]
    child: crate::process::ChildHandle,
    pgid: Pgid,
    /// The anchor reads this to EOF; closing it is how `finish` ends it.
    release: os_pipe::PipeWriter,
    /// The anchor's stdout: one byte per signal it swallowed.  `None` once
    /// [`AnchorProcess::start_witness`] has moved it onto the report reader
    /// thread.
    #[cfg(unix)]
    report: Option<os_pipe::PipeReader>,
    /// Blocks reading [`Self::report`] to EOF, one swallowed signal at a
    /// time.
    #[cfg(unix)]
    reporter: Option<std::thread::JoinHandle<()>>,
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

impl AnchorProcess {
    fn spawn(shell: &Shell) -> Settled<Self> {
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
        Ok(Self {
            #[cfg(unix)]
            child: Some(Anchored::Spawned(child)),
            #[cfg(windows)]
            child,
            pgid,
            release,
            #[cfg(unix)]
            report: Some(report),
            #[cfg(unix)]
            reporter: None,
        })
    }

    /// Start this anchor's witness: the report reader thread, and a
    /// [`crate::process::Watch`] on the anchor's own pid, replacing its
    /// `Anchored::Spawned` handle.  `child`/`report` are `Some` exactly
    /// once, so a second call (there is none) would be the bug `expect`
    /// catches.
    #[cfg(unix)]
    fn start_witness(&mut self, tx: Sender<Event>) {
        let report = self.report.take().expect("started once");
        let report_tx = tx.clone();
        self.reporter = std::thread::Builder::new()
            .name("ral pipeline anchor report reader".to_string())
            .spawn(move || read_anchor_reports(report, &report_tx))
            .ok();
        let Some(Anchored::Spawned(child)) = self.child.take() else {
            panic!("AnchorProcess::start_witness called more than once");
        };
        let watch = child.into_watch(tx, |o| {
            Event::Cancelled(match o {
                crate::process::WaitOutcome::Signaled(sig) => cancel_cause(sig.number()),
                _ => CancelCause::Terminate,
            })
        });
        self.child = Some(Anchored::Watched(watch));
    }

    /// Close the release pipe, join the report reader, and drop the anchor's
    /// watch — its own `Drop` reaps.  An external `kill -STOP` on the group
    /// (unrelated to anything ral itself does) can still catch the anchor
    /// stopped, but the reaper answers that itself, the same rule applied to
    /// every watched pid; nothing here need resume it.  POSIX will not
    /// recycle a leader's pid while the group is non-empty, so the pgid
    /// stays addressable meanwhile.  The report reader's own read end closes
    /// only once it sees EOF — the anchor's own exit closing its write end —
    /// so it is never dropped while the anchor could still `SIGPIPE` on a
    /// stray write into it, with no manual ordering to get right.
    #[cfg(unix)]
    fn finish(self) {
        let Self {
            release,
            reporter,
            child,
            ..
        } = self;
        drop(release);
        if let Some(reporter) = reporter {
            let _ = reporter.join();
        }
        drop(child);
    }

    #[cfg(windows)]
    fn finish(self) {
        let Self {
            mut child, pgid, release, ..
        } = self;
        drop(release);
        let _ = pgid;
        let _ = child.reap();
    }
}

/// Block-read the anchor's report pipe to EOF, one swallowed signal at a
/// time.  EOF means the anchor died; the anchor's own watch reports that
/// with the cause a bare EOF cannot carry, so this simply leaves —
/// including the ordinary case, `AnchorProcess::finish`'s own teardown,
/// where nothing is listening on `tx` any more anyway.
#[cfg(unix)]
fn read_anchor_reports(mut report: os_pipe::PipeReader, tx: &Sender<Event>) {
    use std::io::Read;
    let mut byte = [0u8; 1];
    loop {
        match report.read(&mut byte) {
            Ok(1) => {
                let cause = cancel_cause(i32::from(byte[0]));
                if tx.send(Event::Cancelled(cause)).is_err() {
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

    #[test]
    fn prepare_yields_a_leader_on_every_platform() {
        let shell = Shell::default();
        let group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        assert!(group.owned());
        assert!(group.leader_pgid().as_raw() > 0);
    }

    /// A default shell mints no terminal lease, so `claim_foreground` acquires
    /// nothing even when called.
    #[test]
    fn a_group_that_never_acquired_the_terminal_does_not_claim_it() {
        let shell = Shell::default();
        let mut group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        group.claim_foreground(&shell, &Mooring::adrift());
        assert!(!group.holds_terminal());
    }

    #[cfg(windows)]
    #[test]
    fn a_completed_group_releases_its_windows_job() {
        let shell = Shell::default();
        let group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        let leader = group.leader_pgid().as_raw();
        assert!(crate::process::is_known_group(leader));
        drop(group);
        assert!(!crate::process::is_known_group(leader));
    }

    #[cfg(unix)]
    fn cancelled_within_2s(rx: &std::sync::mpsc::Receiver<Event>) -> Option<CancelCause> {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Event::Cancelled(cause)) => Some(cause),
            Ok(_) | Err(_) => None,
        }
    }

    /// A signal the group swallows is what the collector reads back over its
    /// own channel — the shell's only way to hear a Ctrl-C delivered to a
    /// foreground it lent out.  Two signals in a row prove the anchor
    /// survived the first: a dead one has nothing left to report.
    #[cfg(unix)]
    #[test]
    fn a_signalled_anchor_reports_the_cause_and_lives_on() {
        let shell = Shell::default();
        let mut group = PipelineGroup::prepare(&shell).expect("anchor spawns");
        let (tx, rx) = std::sync::mpsc::channel();
        group.start_witness(tx);
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
            match cancelled_within_2s(&rx) {
                Some(c) if c == cause => {}
                Some(c) => panic!("signal {signal} witnessed as {c:?}"),
                None => panic!("signal {signal} was never reported"),
            }
        }
    }
}
