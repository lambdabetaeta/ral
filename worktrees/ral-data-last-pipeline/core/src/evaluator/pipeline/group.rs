//! Unix process-group lifecycle for interactive pipelines.

use crate::signal::{Pgid, PgidPolicy};
use crate::types::Shell;

/// Whether a pipeline is composed entirely of external commands or has at
/// least one internal (evaluator-thread) stage mixed in.
///
/// Mixed pipelines must never claim the terminal foreground — internal
/// stages run inside ral itself, so backgrounding ral's pgid would deny
/// fd 0 to those threads — and must never inherit fd 0 into an external
/// stage (the external would SIGTTIN immediately on its first read).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum PipelineMode {
    PureExternal,
    Mixed,
}

/// Encapsulates pipeline-group lifecycle on every platform.
///
/// Unix: external pipeline stages share a POSIX pgid so SIGINT is
/// delivered to all of them together, and (for pure-external
/// pipelines) the terminal can be handed to them.
///
/// Windows: every external stage is its own console-group leader
/// (consoles can't be joined), but all stages live in the same Job
/// Object — see `signal::win_groups`.  Ctrl-C from ral's handler is
/// fanned out as Ctrl-Break to each member; abort-path cancellation
/// triggers `TerminateJobObject`.
///
/// Foreground ownership is held in a `ForegroundGuard` whose `Drop`
/// restores the shell's pgid (Unix) or is a no-op (Windows; see the
/// `signal::ForegroundGuard` docs for why).
pub(super) struct PipelineGroup {
    mode: PipelineMode,
    leader: Option<Pgid>,
    foreground: Option<crate::signal::ForegroundGuard>,
}

impl PipelineGroup {
    pub(super) fn new(mode: PipelineMode) -> Self {
        Self {
            mode,
            leader: None,
            foreground: None,
        }
    }

    pub(super) fn mode(&self) -> PipelineMode {
        self.mode
    }

    /// The pipeline's leader pgid, or `None` before the first stage has
    /// spawned.  Required by `wait_handling_stop` to tear the whole
    /// pgid down when a member is SIGTSTP'd.
    pub(super) fn leader_pgid(&self) -> Option<Pgid> {
        self.leader
    }

    /// The current pgid policy for the next external stage to spawn.
    ///
    /// First stage: `NewLeader` (creates the pgid / Job Object).
    /// Later stages: `Join(leader)` (join the existing pgid / Job).
    pub(super) fn next_pgid_policy(&self) -> PgidPolicy {
        match self.leader {
            None => PgidPolicy::NewLeader,
            Some(leader) => PgidPolicy::Join(leader),
        }
    }

    /// Spawn `cmd` for this group's next stage.
    ///
    /// Picks the pgid policy (NewLeader for the first stage, Join
    /// thereafter), routes through `signal::spawn_with_pgid` for the
    /// canonical platform-specific spawn discipline (pre_exec setpgid
    /// + signal reset on Unix; CREATE_NEW_PROCESS_GROUP + Job Object
    /// assignment on Windows), and records the leader on the first
    /// call so subsequent stages join it.
    pub(super) fn spawn(
        &mut self,
        cmd: &mut std::process::Command,
    ) -> std::io::Result<std::process::Child> {
        let policy = self.next_pgid_policy();
        let (child, leader) = crate::signal::spawn_with_pgid(cmd, policy)?;
        if self.leader.is_none() {
            self.leader = leader;
        }
        Ok(child)
    }

    /// Hand terminal foreground to the pipeline's process group.
    ///
    /// `PureExternal` pipelines always claim — every stage is an external
    /// process whose pgid is the pipeline pgid, so handing over the tty is
    /// the natural completion of the spawn.
    ///
    /// `Mixed` pipelines normally refuse: handing the tty to a pgid that
    /// excludes ral's threads would background those threads, and an
    /// internal stage that reads `fd 0 = /dev/tty` would SIGTTIN.  The one
    /// exception is when the pipeline runs inside a `_ed-tui` body — the
    /// editor is suspended, the main thread is parked in the pipeline
    /// collect loop, and the `_ed-tui` contract is precisely "give the
    /// body the terminal."  In that context, an external interactive tail
    /// (e.g. `fzf`) needs foreground so its first `tcsetattr` doesn't trip
    /// SIGTTOU and get reaped as exit 137.  Internal stages of such a
    /// pipeline are expected not to read `/dev/tty`; the common pattern
    /// (e.g. `to-lines $entries | fzf`) writes a value-typed argument to a
    /// pipe and never touches stdin.
    ///
    /// Idempotent: subsequent calls are no-ops once the guard is held.
    /// On Windows `try_acquire` is a no-op that always returns `None`
    /// (shared console; nothing to hand off), so the Mixed/in_tui
    /// gate is moot — but we still evaluate it for symmetry and to
    /// avoid touching the shell's repl state on the wrong platform.
    pub(super) fn claim_foreground(&mut self, shell: &Shell) {
        let permitted = match self.mode {
            PipelineMode::PureExternal => true,
            PipelineMode::Mixed => shell
                .repl
                .plugin_context
                .as_ref()
                .is_some_and(|pc| pc.in_tui),
        };
        if permitted
            && self.foreground.is_none()
            && let Some(Pgid(leader)) = self.leader
        {
            self.foreground = crate::signal::ForegroundGuard::try_acquire(leader, shell);
        }
    }

    /// Install SIGINT relay to the pipeline's process group.
    pub(super) fn install_relay(&self) -> Option<crate::signal::PipelineRelay> {
        let Pgid(leader) = self.leader?;
        crate::signal::PipelineRelay::install(leader)
    }
}

impl Drop for PipelineGroup {
    /// Release platform-specific group state.
    ///
    /// Unix: nothing to do.  The pgid is kernel state that
    /// disappears when its members are reaped, and `RunningChild::Drop`
    /// has already SIGKILL'd the pgid on the abort path.
    ///
    /// Windows: close the Job Object handle.  Created with
    /// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so the handle close
    /// terminates any still-alive members.  Idempotent — covers the
    /// case where launch fails partway and `install_relay` was never
    /// reached, in which case the relay's drop wouldn't have run.
    fn drop(&mut self) {
        #[cfg(windows)]
        if let Some(Pgid(leader)) = self.leader {
            crate::signal::release_win_group(leader);
        }
    }
}
