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
//! Ctrl-C or Ctrl-Z the tty delivers to it: the kernel stops the anchor with
//! the group (default `SIGTSTP`), and the anchor swallows every termination
//! signal and reports its number on its stdout — a pipe one dedicated thread
//! blocks reading, alongside another blocked `waitpid`-ing the anchor itself
//! for its own stop or death; both report [`Witnessed`] on the collector's
//! channel, never polled.

use super::resolve::TerminalPlan;
#[cfg(unix)]
use super::collect::Report;
#[cfg(unix)]
use crate::process::Signal;
use crate::process::{CancelCause, Pgid, PgidPolicy};
use crate::types::{Break, Mooring, Settled, Shell};
#[cfg(unix)]
use std::sync::mpsc::Sender;

/// What this group owns, and therefore what a stop of it means.  The fourth
/// combination the two booleans it replaces could spell — a joining group
/// that owns the terminal — does not exist: a nested pipeline is never
/// foreground.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum GroupRole {
    /// Owns the pgid and was launched into the terminal foreground, so a
    /// stop parks it as a job the REPL can resume.  Survives the park: the
    /// foreground guard is handed back on the way in, the role is not.
    Foreground,
    /// Owns the pgid with nothing to resume a stop — batch mode, a capture, a
    /// pipeline inside a `spawn` worker — so a stop cancels it.
    Background,
    /// Joined an enclosing stage's group: it may not signal, kill, park or
    /// escape on its own account, and forwards a stop to the owner.
    Joining,
}

/// Pgid lifecycle for one pipeline: the anchor, the foreground guard and the
/// SIGINT relay slot, all released together on drop.  The Ctrl-Z gate stage
/// threads wait on lives beside this, on `PipeNode`, not here.
pub(super) struct PipelineGroup {
    role: GroupRole,
    leader: Pgid,
    /// The terminal handoff *actually held*: `None` when `tcsetpgrp` failed,
    /// and `None` again once a park hands the terminal back.
    foreground: Option<crate::process::ForegroundGuard>,
    #[cfg(unix)]
    relay: Option<crate::process::PipelineRelay>,
    /// `Some` exactly when `role` is not `Joining`; taken by `Drop`.
    anchor: Option<AnchorProcess>,
}

/// What the anchor saw happen to the group.
#[derive(Clone, Copy)]
#[cfg(unix)]
pub(super) enum Witnessed {
    Stopped(Signal),
    /// The anchor resumed — reported so the collector can track the
    /// anchor's own stop as a level from its two edges, exactly as a
    /// member's is, rather than a one-shot cell nothing ever clears.
    Continued,
    Cancelled(CancelCause),
}

impl PipelineGroup {
    /// An owning group: the anchor, then the relay — safe to install now
    /// because the anchor is a member that no relayed signal can remove.
    pub(super) fn prepare(terminal: TerminalPlan, shell: &Shell) -> Settled<Self> {
        let anchor = AnchorProcess::spawn(shell)?;
        let leader = anchor.pgid;
        Ok(Self {
            role: match terminal {
                TerminalPlan::ForegroundExternalGroup => GroupRole::Foreground,
                TerminalPlan::NoTerminal => GroupRole::Background,
            },
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
            role: GroupRole::Joining,
            leader: group,
            foreground: None,
            #[cfg(unix)]
            relay: None,
            anchor: None,
        }
    }

    pub(super) fn role(&self) -> GroupRole {
        self.role
    }

    pub(super) fn owned(&self) -> bool {
        self.role != GroupRole::Joining
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

    pub(super) fn claim_foreground(&mut self, shell: &Shell, mooring: &Mooring) {
        // `resolve_terminal_plan` already gated the foreground plan on the
        // lease; re-borrowing it here is the proof `try_acquire` demands.
        if self.role == GroupRole::Foreground
            && self.foreground.is_none()
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
            let signal = if cause == CancelCause::Interrupt {
                libc::SIGINT
            } else {
                libc::SIGTERM
            };
            self.leader.signal_group(Signal::new(signal));
            self.leader.signal_group(Signal::new(libc::SIGCONT));
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

    /// Give the terminal back to the shell and drop the relay, keeping the
    /// anchor so the pgid stays joinable across a park.
    #[cfg(unix)]
    pub(super) fn release_foreground_and_relay(&mut self) {
        self.foreground = None;
        self.relay = None;
    }

    /// Start this group's two anchor-witness threads — a no-op on a joining
    /// group, which has no anchor of its own.  Called once the collector's
    /// channel exists, since the anchor spawns before it does
    /// ([`Self::prepare`] runs before [`super::collect::CollectState::new`]).
    #[cfg(unix)]
    pub(super) fn start_witness_threads(&mut self, tx: Sender<Report>) {
        if let Some(anchor) = self.anchor.as_mut() {
            anchor.start_witness_threads(tx);
        }
    }
}

impl Drop for PipelineGroup {
    /// The anchor last, after every stage handle has gone (`PipelineResources`
    /// and `PipeNode` both order their fields to guarantee it): a stage parked
    /// on its gate must be able to leave before the anchor is waited on.
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

struct AnchorProcess {
    /// `None` on Unix once [`AnchorProcess::start_witness_threads`] has moved
    /// it onto the waiter thread — the sole reaper of its pid from then on;
    /// always `Some` on Windows, which starts no such thread.
    #[cfg(unix)]
    child: Option<crate::process::ChildHandle>,
    #[cfg(windows)]
    child: crate::process::ChildHandle,
    pgid: Pgid,
    /// The anchor reads this to EOF; closing it is how `finish` ends it.
    release: os_pipe::PipeWriter,
    /// The anchor's stdout: one byte per signal it swallowed.  `None` once
    /// [`AnchorProcess::start_witness_threads`] has moved it onto the report
    /// reader thread.
    #[cfg(unix)]
    report: Option<os_pipe::PipeReader>,
    /// Blocks reading [`Self::report`] to EOF, one swallowed signal at a
    /// time.
    #[cfg(unix)]
    reporter: Option<std::thread::JoinHandle<()>>,
    /// Blocks `waitpid`-ing the anchor for its own stop or death — the sole
    /// owner of its wait once started, exactly as an external stage's own
    /// waiter owns its child's.
    #[cfg(unix)]
    waiter: Option<std::thread::JoinHandle<()>>,
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
            child: Some(child),
            #[cfg(windows)]
            child,
            pgid,
            release,
            #[cfg(unix)]
            report: Some(report),
            #[cfg(unix)]
            reporter: None,
            #[cfg(unix)]
            waiter: None,
        })
    }

    /// Start this anchor's two witness threads, once the collector's channel
    /// exists.  `child`/`report` are `Some` exactly once, so a second call
    /// (there is none) would be the bug `expect` catches.
    #[cfg(unix)]
    fn start_witness_threads(&mut self, tx: Sender<Report>) {
        let report = self.report.take().expect("started once");
        let report_tx = tx.clone();
        self.reporter = std::thread::Builder::new()
            .name("ral pipeline anchor report reader".to_string())
            .spawn(move || read_anchor_reports(report, &report_tx))
            .ok();
        let child = self.child.take().expect("started once");
        self.waiter = std::thread::Builder::new()
            .name("ral pipeline anchor waiter".to_string())
            .spawn(move || wait_anchor(child, &tx))
            .ok();
    }

    /// Close the release pipe and join the witness threads — the anchor's own
    /// reap is the waiter thread's, not this one's.
    ///
    /// A parked pipeline leaves the anchor stopped too, so a bare wait would
    /// block forever; `SIGCONT` goes to its pid alone — `-pgid` would wake
    /// the parked stages with it.  POSIX will not recycle a leader's pid while
    /// the group is non-empty, so the pgid stays addressable meanwhile.  The
    /// report reader's own read end closes only once it sees EOF — the
    /// anchor's own exit closing its write end — so it is never dropped
    /// while the anchor could still `SIGPIPE` on a stray write into it, with
    /// no manual ordering to get right.
    #[cfg(unix)]
    fn finish(self) {
        let Self {
            pgid,
            release,
            waiter,
            reporter,
            ..
        } = self;
        drop(release);
        let _ = rustix::process::kill_process(pgid.as_pid(), rustix::process::Signal::CONT);
        if let Some(waiter) = waiter {
            let _ = waiter.join();
        }
        if let Some(reporter) = reporter {
            let _ = reporter.join();
        }
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
/// time.  EOF means the anchor died; the waiter thread's own wait reports
/// that with the cause a bare EOF cannot carry, so this simply leaves —
/// including the ordinary case, `AnchorProcess::finish`'s own teardown,
/// where nothing is listening on `tx` any more anyway.
#[cfg(unix)]
fn read_anchor_reports(mut report: os_pipe::PipeReader, tx: &Sender<Report>) {
    use std::io::Read;
    let mut byte = [0u8; 1];
    loop {
        match report.read(&mut byte) {
            Ok(1) => {
                let cause = cancel_cause(i32::from(byte[0]));
                if tx.send(Report::Witnessed(Witnessed::Cancelled(cause))).is_err() {
                    return;
                }
            }
            _ => return,
        }
    }
}

/// Blocking `waitpid(WUNTRACED | WCONTINUED)` on the anchor, the sole reaper
/// of its pid: a stop reports `Witnessed::Stopped` and loops on, a resume
/// `Witnessed::Continued` and loops on — the anchor is a member of the group
/// like any other, so its own stop is a level tracked from its two edges,
/// never a cell nothing ever clears — and a death — by signal (attributed
/// exactly as a reported swallow would be), or however else it left —
/// reports `Witnessed::Cancelled` and ends the loop.  An anchor that
/// has died has cost the group its join target, so any death cancels the
/// pipeline, expected (`AnchorProcess::finish`'s own teardown, where `tx`
/// has nobody listening) or not.
#[cfg(unix)]
fn wait_anchor(mut child: crate::process::ChildHandle, tx: &Sender<Report>) {
    use crate::process::WaitOutcome;
    loop {
        let witnessed = match child.wait_tracking_stops() {
            Ok(WaitOutcome::Stopped(sig)) => Witnessed::Stopped(sig),
            Ok(WaitOutcome::Continued) => Witnessed::Continued,
            Ok(WaitOutcome::Signaled(sig)) => Witnessed::Cancelled(cancel_cause(sig.number())),
            Ok(_) | Err(_) => Witnessed::Cancelled(CancelCause::Terminate),
        };
        let terminal = !matches!(witnessed, Witnessed::Stopped(_) | Witnessed::Continued);
        if tx.send(Report::Witnessed(witnessed)).is_err() || terminal {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prepare_yields_a_leader_on_every_platform() {
        let shell = Shell::default();
        let group = PipelineGroup::prepare(TerminalPlan::NoTerminal, &shell).expect("anchor spawns");
        assert!(group.owned());
        assert!(group.leader_pgid().as_raw() > 0);
    }

    /// A default shell mints no terminal lease, so `claim_foreground` acquires
    /// nothing: the plan stands, the fact does not.
    #[test]
    fn a_group_that_never_acquired_the_terminal_does_not_claim_it() {
        let shell = Shell::default();
        let mut group = PipelineGroup::prepare(TerminalPlan::ForegroundExternalGroup, &shell)
            .expect("anchor spawns");
        group.claim_foreground(&shell, &Mooring::adrift());
        assert_eq!(group.role(), GroupRole::Foreground);
        assert!(!group.holds_terminal());
    }

    #[cfg(windows)]
    #[test]
    fn a_completed_group_releases_its_windows_job() {
        let shell = Shell::default();
        let group = PipelineGroup::prepare(TerminalPlan::NoTerminal, &shell).expect("anchor spawns");
        let leader = group.leader_pgid().as_raw();
        assert!(crate::process::is_known_group(leader));
        drop(group);
        assert!(!crate::process::is_known_group(leader));
    }

    #[cfg(unix)]
    fn witnessed_within_2s(rx: &std::sync::mpsc::Receiver<Report>) -> Option<Witnessed> {
        match rx.recv_timeout(std::time::Duration::from_secs(2)) {
            Ok(Report::Witnessed(w)) => Some(w),
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
        let mut group =
            PipelineGroup::prepare(TerminalPlan::NoTerminal, &shell).expect("anchor spawns");
        let (tx, rx) = std::sync::mpsc::channel();
        group.start_witness_threads(tx);
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
            group.leader_pgid().signal_group(Signal::new(signal));
            match witnessed_within_2s(&rx) {
                Some(Witnessed::Cancelled(c)) if c == cause => {}
                Some(Witnessed::Cancelled(c)) => panic!("signal {signal} witnessed as {c:?}"),
                Some(Witnessed::Stopped(_)) => panic!("signal {signal} witnessed as a stop"),
                Some(Witnessed::Continued) => panic!("signal {signal} witnessed as a resume"),
                None => panic!("signal {signal} was never reported"),
            }
        }
    }
}
