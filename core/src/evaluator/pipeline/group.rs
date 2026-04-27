//! Unix process-group lifecycle for interactive pipelines.

use crate::types::Shell;

/// Encapsulates all Unix process-group lifecycle for an interactive pipeline.
///
/// On Unix, external pipeline stages share a process group so that SIGINT is
/// delivered to all of them together, and the terminal can be handed to them
/// (and reclaimed afterwards).  `PipelineGroup` concentrates all
/// `#[cfg(unix)]` process-group logic in one place; every method is a no-op
/// on non-Unix.
pub(super) struct PipelineGroup {
    #[cfg(unix)]
    pgid: libc::pid_t,
    #[cfg(unix)]
    handed_foreground: bool,
    #[cfg(unix)]
    interactive: bool,
    #[cfg(unix)]
    stdin_tty: bool,
}

impl PipelineGroup {
    #[cfg_attr(not(unix), allow(unused_variables))]
    pub(super) fn new(shell: &Shell) -> Self {
        Self {
            #[cfg(unix)]
            pgid: 0,
            #[cfg(unix)]
            handed_foreground: false,
            #[cfg(unix)]
            interactive: shell.io.interactive,
            #[cfg(unix)]
            stdin_tty: shell.io.terminal.stdin_tty,
        }
    }

    /// Install the `pre_exec` closure that resets signals and joins the process
    /// group.  Must be called before `cmd.spawn()`; call `register_child` after.
    pub(super) fn pre_exec_hook(&self, cmd: &mut std::process::Command) {
        #[cfg(unix)]
        {
            let pgid_for_child = self.pgid;
            unsafe {
                use std::os::unix::process::CommandExt;
                cmd.pre_exec(move || {
                    libc::setpgid(
                        0,
                        if pgid_for_child == 0 {
                            0
                        } else {
                            pgid_for_child
                        },
                    );
                    libc::signal(libc::SIGINT, libc::SIG_DFL);
                    libc::signal(libc::SIGQUIT, libc::SIG_DFL);
                    libc::signal(libc::SIGTSTP, libc::SIG_DFL);
                    libc::signal(libc::SIGPIPE, libc::SIG_DFL);
                    Ok(())
                });
            }
        }
        #[cfg(not(unix))]
        let _ = cmd;
    }

    /// Place `child` in the pipeline's process group (or make it the leader).
    pub(super) fn register_child(&mut self, child: &std::process::Child) {
        #[cfg(unix)]
        {
            let child_pid = child.id() as libc::pid_t;
            if self.pgid == 0 {
                self.pgid = child_pid;
                unsafe { libc::setpgid(child_pid, child_pid) };
            } else {
                unsafe { libc::setpgid(child_pid, self.pgid) };
            }
        }
        #[cfg(not(unix))]
        let _ = child;
    }

    /// Hand terminal foreground to the pipeline group; idempotent.
    pub(super) fn take_foreground(&mut self) {
        #[cfg(unix)]
        if self.interactive && self.stdin_tty && self.pgid != 0 && !self.handed_foreground {
            unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, self.pgid) };
            self.handed_foreground = true;
        }
    }

    /// Install SIGINT relay to the pipeline's process group.
    pub(super) fn install_relay(&self) -> Option<crate::signal::PipelineRelay> {
        #[cfg(unix)]
        if self.pgid != 0 {
            return crate::signal::PipelineRelay::install(self.pgid);
        }
        None
    }

    /// Restore terminal foreground to the shell's own process group.
    pub(super) fn restore_foreground(&self) {
        #[cfg(unix)]
        if self.interactive && self.stdin_tty && self.pgid != 0 && self.handed_foreground {
            let shell_pgid = unsafe { libc::getpgrp() };
            unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, shell_pgid) };
        }
    }
}
