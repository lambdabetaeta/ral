//! Unix process-group lifecycle for process-staged pipelines.
//!
//! The SIGINT relay is claimed only once the pgid holds a real child:
//! `prepare` establishes the anchor and its pgid, and `spawn` takes the
//! relay slot after the first stage has joined.  A `kill(-pgid)` any
//! earlier would take out the anchor — the join target every later stage
//! needs — and nothing the user asked to stop; in that window a SIGINT
//! instead cancels the run's foreground scope, which the launch loop's
//! per-stage `process::check` observes before the next stage spawns.
//! Off Unix and for single-stage pipelines `prepare` is a no-op, and the
//! first `spawn` establishes the leader and claims the relay together.

#[cfg(unix)]
use super::protocol::pipe_error;
use super::resolve::TerminalPlan;
use crate::process::{Pgid, PgidPolicy};
#[cfg(unix)]
use crate::types::Break;
use crate::types::{Mooring, Settled, Shell};

/// Pgid lifecycle for one pipeline: the anchor, the foreground guard and
/// the SIGINT relay slot, all released together on drop.  Stage start
/// gating belongs to `super::launch`, not here.
pub(super) struct PipelineGroup {
    terminal: TerminalPlan,
    leader: Option<Pgid>,
    foreground: Option<crate::process::ForegroundGuard>,
    relay: Option<crate::process::PipelineRelay>,
    #[cfg(unix)]
    anchor: Option<AnchorProcess>,
}

impl PipelineGroup {
    pub(super) fn new(terminal: TerminalPlan) -> Self {
        Self {
            terminal,
            leader: None,
            foreground: None,
            relay: None,
            #[cfg(unix)]
            anchor: None,
        }
    }

    pub(super) fn owns_tty(&self) -> bool {
        self.terminal.owns_tty()
    }

    pub(super) fn leader_pgid(&self) -> Option<Pgid> {
        self.leader
    }

    #[cfg(unix)]
    pub(super) fn prepare(&mut self, shell: &Shell, stages: usize) -> Settled<()> {
        if self.leader.is_some() {
            return Ok(());
        }
        // The anchor keeps a stage's `setpgid` join target alive until
        // the last stage has joined.  One stage has no later join: its
        // own child leads the group.
        if stages < 2 {
            return Ok(());
        }
        let anchor = AnchorProcess::spawn(shell)?;
        let pgid = anchor.pgid();
        self.leader = Some(pgid);
        self.anchor = Some(anchor);
        Ok(())
    }

    #[cfg(not(unix))]
    #[allow(
        clippy::unused_self,
        clippy::needless_pass_by_ref_mut,
        clippy::unnecessary_wraps,
        reason = "the signature is the Unix arm's, where the anchor records itself in `self`, needs the shell to spawn, counts the stages, and may fail; the one caller is cross-platform and writes `group.prepare(shell, stages)?`"
    )]
    pub(super) fn prepare(&mut self, _shell: &Shell, _stages: usize) -> Settled<()> {
        Ok(())
    }

    pub(super) fn spawn(
        &mut self,
        cmd: &mut crate::process::Launch,
    ) -> std::io::Result<(
        crate::process::ChildHandle,
        Option<crate::process::jail::JailCgroup>,
    )> {
        let policy = match self.leader {
            Some(leader) => PgidPolicy::Join(leader),
            None => PgidPolicy::NewLeader,
        };
        let (child, leader, jail) = cmd.spawn(policy)?;
        if self.leader.is_none() {
            self.leader = leader;
        }
        // Only now, with a real child in the pgid, may SIGINT be relayed to it.
        if self.relay.is_none()
            && let Some(group) = self.leader
        {
            self.relay = crate::process::PipelineRelay::install(group.as_raw());
        }
        Ok((child, jail))
    }

    pub(super) fn claim_foreground(&mut self, shell: &Shell, mooring: &Mooring) {
        // `resolve_terminal_plan` already gated `owns_tty` on the lease;
        // re-borrowing it here is the proof `try_acquire` demands.
        if self.terminal.owns_tty()
            && self.foreground.is_none()
            && let Some(group) = self.leader
            && let Some(lease) = shell.terminal_lease(mooring)
        {
            self.foreground = crate::process::ForegroundGuard::try_acquire(group.as_raw(), lease);
        }
    }
}

impl Drop for PipelineGroup {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            if let Some(anchor) = self.anchor.take() {
                anchor.finish();
            }
        }
        #[cfg(windows)]
        if let Some(group) = self.leader {
            crate::process::release_win_group(group.as_raw());
        }
    }
}

#[cfg(unix)]
#[allow(
    clippy::cast_possible_wrap,
    reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps"
)]
fn child_pid(child: &crate::process::ChildHandle) -> libc::pid_t {
    child.id() as libc::pid_t
}

#[cfg(unix)]
struct AnchorProcess {
    child: Option<crate::process::ChildHandle>,
    release: Option<std::os::unix::net::UnixStream>,
}

#[cfg(unix)]
impl AnchorProcess {
    fn spawn(shell: &Shell) -> Settled<Self> {
        let (parent, child_end) = super::protocol::anchor_pair()?;
        let mut cmd = super::helper::self_reexec(super::helper::ANCHOR_FLAG).map_err(pipe_error)?;
        cmd.stdin(crate::process::StdioSpec::null());
        cmd.stdout(crate::process::StdioSpec::null());
        cmd.stderr(crate::process::StdioSpec::null());
        super::protocol::pass(&mut cmd, super::helper::ANCHOR_FD_ENV, &child_end)?;
        let (mut child, leader, _jail) = cmd.spawn(PgidPolicy::NewLeader).map_err(|e| {
            Break::Error(crate::types::Error::new(format!("pipeline anchor: {e}"), 1))
        })?;
        if shell.has_active_capabilities() {
            crate::sandbox::apply_child_limits(&child);
        }
        let Some(_pgid) = leader else {
            // `Child::drop` neither kills nor reaps, so the anchor would
            // outlive the unwind as a live process.
            let _ = child.kill();
            let _ = child.reap();
            return Err(Break::Error(crate::types::Error::new(
                "pipeline anchor failed to establish a process group",
                1,
            )));
        };
        drop(child_end);
        Ok(Self {
            child: Some(child),
            release: Some(parent),
        })
    }

    fn pgid(&self) -> Pgid {
        Pgid::from_raw(child_pid(self.child.as_ref().expect("anchor child")))
            .expect("a child pid is positive")
    }

    /// Close the release pipe and reap the anchor.
    ///
    /// A pipeline parked as a stopped job leaves the anchor stopped too —
    /// it shared the foreground pgid that took SIGTSTP — so a bare wait
    /// would block forever.  `SIGCONT` goes to the anchor's pid alone;
    /// `-pgid` would wake the parked stages with it.  The pgid stays
    /// addressable meanwhile: POSIX will not recycle a leader's pid while
    /// the group is non-empty.
    fn finish(mut self) {
        let _ = self.release.take();
        if let Some(mut child) = self.child.take() {
            let pid = child_pid(&child);
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(pid).unwrap(),
                rustix::process::Signal::CONT,
            );
            let _ = child.reap();
        }
    }
}
