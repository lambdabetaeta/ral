//! Job control for the interactive shell (§18).
//!
//! Tracks background/suspended processes. Provides jobs/fg/bg/disown.
//! On shell exit, sends SIGTERM to remaining children, waits 5s, then SIGKILL.

#[cfg(unix)]
use std::collections::HashMap;

#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct Job {
    pub id: usize,
    pub pid: i32,
    pub cmd: String,
    pub state: JobState,
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum JobState {
    Running,
    Stopped,
}

#[cfg(unix)]
pub struct JobTable {
    jobs: HashMap<usize, Job>,
}

#[cfg(unix)]
impl JobTable {
    pub fn new() -> Self {
        JobTable {
            jobs: HashMap::new(),
        }
    }

    /// Mark a job as stopped (from SIGTSTP).
    pub fn stop(&mut self, pid: i32) {
        for job in self.jobs.values_mut() {
            if job.pid == pid {
                job.state = JobState::Stopped;
            }
        }
    }

    /// Remove a job (after it exits or is disowned).
    pub fn remove(&mut self, id: usize) -> Option<Job> {
        self.jobs.remove(&id)
    }

    /// List all jobs.
    pub fn list(&self) -> Vec<&Job> {
        let mut jobs: Vec<_> = self.jobs.values().collect();
        jobs.sort_by_key(|j| j.id);
        jobs
    }

    /// Get the most recent job.
    pub fn current(&self) -> Option<&Job> {
        self.jobs.values().max_by_key(|j| j.id)
    }

    /// Resume a stopped job (SIGCONT). Returns its pid.  fg/bg differ only
    /// in what the caller does with the terminal afterwards.
    pub fn resume(&mut self, id: usize) -> Option<i32> {
        let job = self.jobs.get_mut(&id)?;
        if job.state == JobState::Stopped {
            job.state = JobState::Running;
            unsafe { libc::kill(-job.pid, libc::SIGCONT) };
        }
        Some(job.pid)
    }

    /// Reap any children that have exited (non-blocking waitpid).
    pub fn reap(&mut self) {
        let pids: Vec<(usize, i32)> = self.jobs.iter().map(|(id, j)| (*id, j.pid)).collect();
        for (id, pid) in pids {
            let mut status: i32 = 0;
            let r = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG | libc::WUNTRACED) };
            if r == pid {
                if libc::WIFSTOPPED(status) {
                    self.stop(pid);
                } else {
                    // Exited or signaled — remove from table.
                    self.jobs.remove(&id);
                }
            }
        }
    }

    /// On shell exit: SIGTERM all, wait 5s, SIGKILL survivors (§13.3).
    pub fn cleanup(&mut self) {
        if self.jobs.is_empty() {
            return;
        }

        // Send SIGTERM to all remaining children.
        for job in self.jobs.values() {
            unsafe { libc::kill(job.pid, libc::SIGTERM) };
        }

        // Wait up to 5 seconds.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !self.jobs.is_empty() && std::time::Instant::now() < deadline {
            self.reap();
            if !self.jobs.is_empty() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }

        // SIGKILL any survivors.
        for job in self.jobs.values() {
            unsafe { libc::kill(job.pid, libc::SIGKILL) };
            let mut status: i32 = 0;
            unsafe { libc::waitpid(job.pid, &raw mut status, 0) };
        }
        self.jobs.clear();
    }
}

/// Wait for a foreground process, handling SIGTSTP (Ctrl-Z).
/// Returns (`exit_code`, `was_stopped`).
/// `stdin_tty` should be `shell.io.terminal.stdin_tty` at the call site.
#[cfg(unix)]
pub fn wait_foreground(pid: i32, stdin_tty: bool) -> (i32, bool) {
    let shell_pgid = unsafe { libc::getpgrp() };
    if stdin_tty {
        unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, pid) };
    }

    let mut status: i32 = 0;
    let result = loop {
        let r = unsafe { libc::waitpid(pid, &raw mut status, libc::WUNTRACED) };
        if r < 0 {
            break (1, false);
        }
        if libc::WIFSTOPPED(status) {
            break (0, true); // Ctrl-Z
        }
        if libc::WIFEXITED(status) {
            break (libc::WEXITSTATUS(status), false);
        }
        if libc::WIFSIGNALED(status) {
            break (128 + libc::WTERMSIG(status), false);
        }
    };

    if stdin_tty {
        unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, shell_pgid) };
    }

    result
}

/// Set default signal dispositions for the interactive shell.
/// SIGINT uses the relay handler (does nothing when no relay slots are active,
/// forwards to external pipeline groups when they are).  The evaluator hands
/// the terminal to all-external pipelines via tcsetpgrp; for mixed pipelines
/// it claims a relay slot so Ctrl+C reaches the external stages.
#[cfg(unix)]
pub fn setup_signals() {
    unsafe {
        // Install relay rather than SIG_IGN.  When no relay slots are active
        // it is a no-op, which is the right behaviour between commands.
        libc::signal(
            libc::SIGINT,
            ral_core::signal::relay_handler() as *const () as libc::sighandler_t,
        );
        // Ignore SIGTSTP in the shell — we handle it via waitpid.
        libc::signal(libc::SIGTSTP, libc::SIG_IGN);
        // Ignore SIGTTOU so we can manipulate the terminal.
        libc::signal(libc::SIGTTOU, libc::SIG_IGN);
    }
}
