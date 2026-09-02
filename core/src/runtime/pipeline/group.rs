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
//! signal and reports its number on its stdout, a pipe the parent polls.

use super::resolve::TerminalPlan;
#[cfg(unix)]
use crate::process::Signal;
use crate::process::{CancelCause, Pgid, PgidPolicy};
use crate::types::{Break, Mooring, Settled, Shell};

/// Pgid lifecycle for one pipeline: the anchor, the foreground guard and the
/// SIGINT relay slot, all released together on drop.  The Ctrl-Z gate stage
/// threads wait on lives beside this, on `PipeNode`, not here.
pub(super) struct PipelineGroup {
    terminal: TerminalPlan,
    leader: Pgid,
    foreground: Option<crate::process::ForegroundGuard>,
    #[cfg(unix)]
    relay: Option<crate::process::PipelineRelay>,
    /// `None` for a joining group.
    anchor: Option<AnchorProcess>,
    /// Set by `signal`, so `Drop` follows up with `kill` on a member that
    /// ignored it.
    cancelled: bool,
}

/// What the anchor saw happen to the group.
#[cfg(unix)]
pub(super) enum Witnessed {
    Stopped(Signal),
    Cancelled(CancelCause),
}

impl PipelineGroup {
    /// An owning group: the anchor, then the relay — safe to install now
    /// because the anchor is a member that no relayed signal can remove.
    pub(super) fn prepare(terminal: TerminalPlan, shell: &Shell) -> Settled<Self> {
        let anchor = AnchorProcess::spawn(shell)?;
        let leader = anchor.pgid;
        Ok(Self {
            terminal,
            leader,
            foreground: None,
            #[cfg(unix)]
            relay: crate::process::PipelineRelay::install(leader.as_raw()),
            anchor: Some(anchor),
            cancelled: false,
        })
    }

    /// A pipeline launched inside a stage thread joins `group` rather than
    /// owning one.
    pub(super) fn joining(group: Pgid) -> Self {
        Self {
            terminal: TerminalPlan::NoTerminal,
            leader: group,
            foreground: None,
            #[cfg(unix)]
            relay: None,
            anchor: None,
            cancelled: false,
        }
    }

    pub(super) fn owns_tty(&self) -> bool {
        self.terminal.owns_tty()
    }

    pub(super) fn owned(&self) -> bool {
        self.anchor.is_some()
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
        // `resolve_terminal_plan` already gated `owns_tty` on the lease;
        // re-borrowing it here is the proof `try_acquire` demands.
        if self.owned()
            && self.terminal.owns_tty()
            && self.foreground.is_none()
            && let Some(lease) = shell.terminal_lease(mooring)
        {
            self.foreground =
                crate::process::ForegroundGuard::try_acquire(self.leader.as_raw(), lease);
        }
    }

    /// The owner's cancel signal: `SIGINT` for `Interrupt`, else `SIGTERM`,
    /// then `SIGCONT` — a stopped member cannot act on either until it runs.
    /// Nothing on Windows, where the per-child ladder cancels.
    pub(super) fn signal(&mut self, cause: CancelCause) {
        if !self.owned() {
            return;
        }
        self.cancelled = true;
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

    fn kill(&self) {
        #[cfg(unix)]
        self.leader.signal_group(Signal::new(libc::SIGKILL));
        #[cfg(windows)]
        crate::process::kill_pipeline_group(self.leader);
    }

    /// Give the terminal back to the shell and drop the relay, keeping the
    /// anchor so the pgid stays joinable across a park.
    #[cfg(unix)]
    pub(super) fn release_foreground_and_relay(&mut self) {
        self.foreground = None;
        self.relay = None;
    }

    /// Non-blocking; `None` on a joining group, which has no anchor of its own.
    #[cfg(unix)]
    pub(super) fn witness(&mut self) -> Option<Witnessed> {
        self.anchor.as_mut()?.witness()
    }
}

impl Drop for PipelineGroup {
    fn drop(&mut self) {
        if self.cancelled {
            self.kill();
        }
        if let Some(anchor) = self.anchor.take() {
            anchor.finish();
        }
        #[cfg(windows)]
        if self.owned() {
            crate::process::release_win_group(self.leader.as_raw());
        }
    }
}

struct AnchorProcess {
    child: crate::process::ChildHandle,
    pgid: Pgid,
    /// The anchor reads this to EOF; closing it is how `finish` ends it.
    release: os_pipe::PipeWriter,
    /// The anchor's stdout: one byte per signal it swallowed, read
    /// non-blocking.
    #[cfg(unix)]
    report: os_pipe::PipeReader,
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
            rustix::fs::fcntl_setfl(&report, rustix::fs::OFlags::NONBLOCK).map_err(anchor_error)?;
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
            child,
            pgid,
            release,
            #[cfg(unix)]
            report,
        })
    }

    /// A reported signal outranks a stop.  An anchor that has died has cost
    /// the group its join target, so the pipeline is cancelled: by the cause
    /// of the signal that killed it — one landing in the window between its
    /// exec and its handler install, before which the disposition is the
    /// default — else as `Terminate`.
    #[cfg(unix)]
    fn witness(&mut self) -> Option<Witnessed> {
        use crate::process::WaitOutcome;
        use std::io::Read;
        let mut byte = [0u8; 1];
        if matches!((&self.report).read(&mut byte), Ok(1)) {
            return Some(Witnessed::Cancelled(cancel_cause(i32::from(byte[0]))));
        }
        match self
            .child
            .try_wait_handling_stop(true, crate::process::KillTarget::Pid)
        {
            Ok(Some(WaitOutcome::Stopped(sig))) => Some(Witnessed::Stopped(sig)),
            Ok(Some(WaitOutcome::Signaled(sig))) => {
                Some(Witnessed::Cancelled(cancel_cause(sig.number())))
            }
            Ok(Some(_)) => Some(Witnessed::Cancelled(CancelCause::Terminate)),
            Ok(None) | Err(_) => None,
        }
    }

    /// Close the release pipe and reap the anchor.
    ///
    /// A parked pipeline leaves the anchor stopped too, so a bare wait would
    /// block forever; `SIGCONT` goes to its pid alone — `-pgid` would wake
    /// the parked stages with it.  POSIX will not recycle a leader's pid while
    /// the group is non-empty, so the pgid stays addressable meanwhile.
    fn finish(self) {
        let Self {
            mut child,
            pgid,
            release,
            #[cfg(unix)]
            report,
        } = self;
        drop(release);
        #[cfg(unix)]
        let _ = rustix::process::kill_process(pgid.as_pid(), rustix::process::Signal::CONT);
        #[cfg(windows)]
        let _ = pgid;
        let _ = child.reap();
        // After the reap: the anchor has `SIGPIPE` at default and would die
        // of a report written into a closed pipe.
        #[cfg(unix)]
        drop(report);
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

    #[cfg(unix)]
    fn witness_within_2s(group: &mut PipelineGroup) -> Option<Witnessed> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if let Some(w) = group.witness() {
                return Some(w);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    /// A signal the group swallows is what the collector reads back — the
    /// shell's only way to hear a Ctrl-C delivered to a foreground it lent
    /// out.  Two signals in a row prove the anchor survived the first: a dead
    /// one has nothing left to report.
    #[cfg(unix)]
    #[test]
    fn a_signalled_anchor_reports_the_cause_and_lives_on() {
        let shell = Shell::default();
        let mut group = PipelineGroup::prepare(TerminalPlan::NoTerminal, &shell).expect("anchor spawns");
        assert!(group.witness().is_none());
        // Past the window between the anchor's exec and its handler install.
        std::thread::sleep(std::time::Duration::from_millis(300));
        for (signal, cause) in [
            (libc::SIGINT, CancelCause::Interrupt),
            (libc::SIGTERM, CancelCause::Terminate),
        ] {
            group.leader_pgid().signal_group(Signal::new(signal));
            match witness_within_2s(&mut group) {
                Some(Witnessed::Cancelled(c)) if c == cause => {}
                Some(Witnessed::Cancelled(c)) => panic!("signal {signal} witnessed as {c:?}"),
                Some(Witnessed::Stopped(_)) => panic!("signal {signal} witnessed as a stop"),
                None => panic!("signal {signal} was never reported"),
            }
        }
    }
}
