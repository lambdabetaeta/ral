//! Unix process-group lifecycle for process-staged pipelines.
//!
//! ## SIGINT/relay invariant
//!
//! The interactive shell's SIGINT relay is *only ever active when the
//! pipeline pgid contains at least one real (non-anchor) child*.  In
//! that state, killing the pgid is safe — it terminates the children
//! the user wants gone, and the still-spawning loop's per-stage
//! `signal::check` will surface the `interrupted` error before any
//! later stage attempts to join the (now possibly empty) group.
//!
//! Concretely: `prepare` spawns the anchor for a multi-stage pipeline
//! and records its pgid as `leader`, but does *not* claim a relay
//! slot.  `spawn` claims the relay slot on its first call — after
//! `spawn_with_pgid` returns, the first real child has already joined
//! the anchor pgid via `pre_exec`'s `setpgid`, so the relay forwarding
//! SIGINT to `-pgid` cannot leave the group with nothing in it.  On
//! non-Unix, and for single-stage pipelines, where `prepare` is a
//! no-op, the rule is the same: the relay is claimed once, on the
//! first `spawn` that establishes a `leader`.
//!
//! Between `prepare` and the first `spawn`, no relay is active, so a
//! SIGINT racing the launch sequence merely cancels the turn's
//! foreground scope.  The launch loop's per-stage `signal::check`
//! observes the scope and aborts cleanly before any stage spawns.

#[cfg(unix)]
use super::protocol::pipe_error;
use super::resolve::TerminalPlan;
use crate::process::{Pgid, PgidPolicy};
#[cfg(unix)]
use crate::types::Break;
use crate::types::{Settled, Shell};

/// Encapsulates pipeline-group lifecycle on every platform.
///
/// On Unix, process-staged pipelines own a stable pgid anchor and an
/// optional foreground guard.  On Windows the existing pgid abstraction
/// still stands in for the group; the anchor is Unix-only.
///
/// Start gating is owned by [`super::launch`]: helper stages wait for
/// their deferred job frames until every stage has spawned and any
/// foreground handoff has completed.  Direct external stages are admitted
/// only when that gate is unnecessary.  This type stays focused on pgid
/// lifecycle.
pub(super) struct PipelineGroup {
    terminal: TerminalPlan,
    leader: Option<Pgid>,
    foreground: Option<crate::process::ForegroundGuard>,
    /// SIGINT-forwarding relay slot, claimed on the *first* `spawn` —
    /// i.e. once a real child has joined the pgid.  Forwarding SIGINT
    /// to `-pgid` before any real child is in would leave only the
    /// anchor to receive it, and the anchor's death would in turn
    /// strand later children whose `setpgid` would no longer find an
    /// existing group.  Dropped with the group.
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
        // The anchor exists so a stage's `setpgid` join target cannot
        // die before a *later* stage joins.  A single-stage pipeline
        // has no later join: its only child establishes the group as
        // leader on `spawn`, exactly the non-Unix rule above.
        if stages < 2 {
            return Ok(());
        }
        let anchor = AnchorProcess::spawn(shell)?;
        let pgid = anchor.pgid();
        self.leader = Some(pgid);
        self.anchor = Some(anchor);
        // Relay install is *not* done here — see the SIGINT/relay
        // invariant note at the top of this file.  Between this point
        // and the first `spawn`, the launch loop's per-stage
        // `signal::check` is the abort path for racing SIGINTs.
        Ok(())
    }

    #[cfg(not(unix))]
    pub(super) fn prepare(&mut self, _shell: &Shell, _stages: usize) -> Settled<()> {
        Ok(())
    }

    pub(super) fn spawn(
        &mut self,
        cmd: &mut crate::process::Launch,
    ) -> std::io::Result<crate::process::ChildHandle> {
        let policy = match self.leader {
            Some(leader) => PgidPolicy::Join(leader),
            None => PgidPolicy::NewLeader,
        };
        let (child, leader) = cmd.spawn(policy)?;
        if self.leader.is_none() {
            self.leader = leader;
        }
        // Claim the relay slot now that a real child is in the pgid.
        // See the SIGINT/relay invariant note: the relay must never be
        // active over a child-less pgid.
        if self.relay.is_none()
            && let Some(Pgid(p)) = self.leader
        {
            self.relay = crate::process::PipelineRelay::install(p);
        }
        Ok(child)
    }

    pub(super) fn claim_foreground(&mut self, shell: &Shell) {
        // The terminal plan was already resolved against the lease, so a
        // `ForegroundExternalGroup` here implies the turn holds one; the
        // borrow is the unforgeable proof `try_acquire` now demands.
        if self.terminal.owns_tty()
            && self.foreground.is_none()
            && let Some(Pgid(leader)) = self.leader
            && let Some(lease) = shell.terminal_lease()
        {
            self.foreground = crate::process::ForegroundGuard::try_acquire(leader, lease);
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
        if let Some(Pgid(leader)) = self.leader {
            crate::process::release_win_group(leader);
        }
    }
}

#[cfg(unix)]
struct AnchorProcess {
    child: Option<crate::process::ChildHandle>,
    release: Option<std::os::unix::net::UnixStream>,
}

#[cfg(unix)]
impl AnchorProcess {
    fn spawn(shell: &Shell) -> Settled<Self> {
        let (parent, child_end) = super::protocol::create_value_pair()?;
        let mut cmd = super::helper::self_reexec(super::helper::ANCHOR_FLAG).map_err(pipe_error)?;
        cmd.stdin(crate::process::StdioSpec::null());
        cmd.stdout(crate::process::StdioSpec::null());
        cmd.stderr(crate::process::StdioSpec::null());
        // Mark the child end inheritable + stash its fd in env, the
        // same primitive the protocol's gate setup uses.
        super::protocol::pass(&mut cmd, super::helper::ANCHOR_FD_ENV, &child_end)?;
        let (mut child, leader) = cmd.spawn(PgidPolicy::NewLeader).map_err(|e| {
            Break::Error(crate::types::Error::new(format!("pipeline anchor: {e}"), 1))
        })?;
        if shell.has_active_capabilities() {
            crate::sandbox::apply_child_limits(&child);
        }
        let Some(_pgid) = leader else {
            // `std::process::Child::drop` neither kills nor reaps, so the
            // just-spawned anchor would leak as a running zombie. Kill and
            // reap it before unwinding. The child was just spawned and has
            // no pgid yet, so it cannot be SIGSTOP'd — a plain blocking
            // `wait` after SIGKILL reaps it without the WUNTRACED dance.
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
        #[allow(clippy::cast_possible_wrap, reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps")]
        let pid = self.child.as_ref().expect("anchor child").id() as libc::pid_t;
        Pgid(pid)
    }

    /// Close the release pipe and reap the anchor.  When the pipeline
    /// has been parked as a stopped job the anchor is in stopped state
    /// (it was in the foreground pgid that received SIGTSTP), so a bare
    /// `wait()` here would block forever.  Sending `SIGCONT` to just
    /// the anchor's pid (not `-pgid`) wakes the anchor without
    /// disturbing the parked sibling stages: the anchor resumes its
    /// `read` on the release fd, sees EOF, and exits cleanly.  The
    /// pgid number stays addressable afterwards because surviving
    /// stages keep it alive — POSIX won't recycle the leader's pid
    /// until the group is empty.
    fn finish(mut self) {
        let _ = self.release.take();
        if let Some(mut child) = self.child.take() {
            #[allow(clippy::cast_possible_wrap, reason = "child.id() is a live OS pid: positive and well below i32::MAX, so the u32→pid_t reinterpretation never wraps")]
            let pid = child.id() as libc::pid_t;
            unsafe {
                libc::kill(pid, libc::SIGCONT);
            }
            // Anchor was already SIGCONT'd above, so it's running again
            // (not stopped); a plain blocking wait suffices and the
            // WUNTRACED dance of ChildHandle would never observe a stop.
            let _ = child.reap();
        }
    }
}
