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

/// Encapsulates all Unix process-group lifecycle for an interactive pipeline.
///
/// On Unix, external pipeline stages share a process group so that SIGINT is
/// delivered to all of them together, and (for pure-external pipelines) the
/// terminal can be handed to them.  `PipelineGroup` concentrates all
/// `#[cfg(unix)]` process-group logic in one place; every method is a no-op
/// on non-Unix.
///
/// Foreground ownership is held in a `ForegroundGuard` whose `Drop` restores
/// the shell's pgid — that makes the restore unconditional regardless of
/// which path leaves `run_pipeline`.
pub(super) struct PipelineGroup {
    mode: PipelineMode,
    #[cfg(unix)]
    leader: Option<Pgid>,
    #[cfg(unix)]
    foreground: Option<crate::signal::ForegroundGuard>,
}

impl PipelineGroup {
    pub(super) fn new(mode: PipelineMode) -> Self {
        Self {
            mode,
            #[cfg(unix)]
            leader: None,
            #[cfg(unix)]
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
        #[cfg(unix)]
        {
            self.leader
        }
        #[cfg(not(unix))]
        None
    }

    /// The current pgid policy for the next external stage to spawn.
    ///
    /// First stage: `NewLeader` (creates the pgid).
    /// Later stages: `Join(leader)` (join the existing pgid).
    pub(super) fn next_pgid_policy(&self) -> PgidPolicy {
        #[cfg(unix)]
        {
            match self.leader {
                None => PgidPolicy::NewLeader,
                Some(leader) => PgidPolicy::Join(leader),
            }
        }
        #[cfg(not(unix))]
        PgidPolicy::Inherit
    }

    /// Install the `pre_exec` closure that resets signals and applies this
    /// stage's pgid policy.  Must be called before `cmd.spawn()`; call
    /// `register_child` after.
    pub(super) fn pre_exec_hook(&self, cmd: &mut std::process::Command) {
        #[cfg(unix)]
        {
            let policy = self.next_pgid_policy();
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(move || {
                    policy.apply();
                    crate::signal::reset_child_signals();
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        let _ = cmd;
    }

    /// Mirror the `pre_exec` `setpgid` from the parent to close the parent-
    /// vs-child race window: by the time `register_child` returns, the
    /// pgid is established regardless of which side won.
    pub(super) fn register_child(&mut self, child: &std::process::Child) {
        #[cfg(unix)]
        {
            let child_pid = child.id() as libc::pid_t;
            match self.leader {
                None => {
                    self.leader = Some(Pgid(child_pid));
                    unsafe { libc::setpgid(child_pid, child_pid) };
                }
                Some(Pgid(leader)) => {
                    unsafe { libc::setpgid(child_pid, leader) };
                }
            }
        }
        #[cfg(not(unix))]
        let _ = child;
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
    pub(super) fn claim_foreground(&mut self, shell: &Shell) {
        #[cfg(unix)]
        {
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
        #[cfg(not(unix))]
        let _ = shell;
    }

    /// Install SIGINT relay to the pipeline's process group.
    pub(super) fn install_relay(&self) -> Option<crate::signal::PipelineRelay> {
        #[cfg(unix)]
        if let Some(Pgid(leader)) = self.leader {
            return crate::signal::PipelineRelay::install(leader);
        }
        None
    }
}
