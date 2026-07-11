//! Job control for the interactive shell (§18).
//!
//! Tracks process groups *parked by the kernel* by their process-group
//! id: the live REPL path registers a job only when a foreground pipeline
//! is stopped by SIGTSTP (`Escape::Stopped`).  `Running` is a transient
//! state a group passes through while `bg`/`fg` resumes a stopped job; a
//! job leaves the table when its group terminates.  A `&`-backgrounded
//! pipeline is an in-process `spawn` handle (`CompKind::Background`, see
//! design/pipelines), not a pgid job, and is not tracked here.
//!
//! A single-process job places its child in its own pgid (set in the
//! `pre_exec` hook of every pipeline stage on Unix; via
//! `CREATE_NEW_PROCESS_GROUP` + a Job Object on Windows), so every
//! signal, wait, and foreground handoff in this module operates on the
//! whole group regardless of whether the job has one stage or many.
//! This is the structural prevention rule for the "is this a pid or a
//! pgid?" confusion: there is only the group here.
//!
//! Provides jobs/fg/bg/disown.  On shell exit, the group is taken down
//! gracefully first (`SIGTERM` to the pgid on Unix; `Ctrl-Break`
//! fan-out is already wired through the `SetConsoleCtrlHandler` on
//! Windows, with `TerminateJobObject` as the hard stop), then a 5-second
//! grace, then forcibly (`SIGKILL` / `TerminateJobObject`).
//!
//! Platform notes:
//!   * Unix exposes the full job-control surface: SIGTSTP parks a
//!     foreground job in the table, SIGCONT resumes it, `fg` reattaches
//!     the terminal.
//!   * Windows has no SIGTSTP analogue, so the `Stopped` state cannot
//!     be entered spontaneously — `Escape::Stopped` is a Unix-only
//!     evaluator escape.  The table still compiles and operates: `fg`
//!     blocks on the leader's process handle, `cleanup` is wired
//!     through the existing Job Object `KILL_ON_JOB_CLOSE` machinery, and
//!     `disown` clears that flag so members survive.  This keeps every
//!     downstream caller (and the pipeline lifecycle) cfg-free.

use ral_core::Shell;
use ral_core::types::Resident;
use std::collections::HashMap;

#[cfg(unix)]
use ral_core::process::waitpid_eintr;

#[derive(Debug, Clone)]
pub struct Job {
    pub id: usize,
    /// Process-group id of the whole pipeline.  All signal and wait
    /// operations on the job target the whole group so they reach every
    /// stage.
    pub pgid: i32,
    pub cmd: String,
    pub state: JobState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
}

/// The job-table chapter's resident signature
/// (`decisions/260705_session-ledger`, `core/src/types/resident.rs`): a
/// bare numeric designator (a fold brackets it uniformly, matching a
/// worker's `[wN]` at the listing layer without either chapter agreeing on
/// bracketing itself), a pgid-typed capability, and no lease at all —
/// a pgid job drifts toward reclamation only by the exit sweep, never an
/// idle-observation clock, so it is "human-owned" whether running or
/// stopped.
impl Resident for Job {
    fn designator(&self) -> String {
        self.id.to_string()
    }

    fn population(&self) -> &'static str {
        "job"
    }

    fn capability_kind(&self) -> &'static str {
        "pgid"
    }

    fn lease_row(&self) -> String {
        "none — human-owned".to_string()
    }

    fn state_label(&self) -> String {
        match self.state {
            JobState::Running => "running".to_string(),
            JobState::Stopped => "stopped".to_string(),
        }
    }

    /// The cooperative-signal analogue of a worker's cancel scope: `SIGTERM`
    /// to the whole pgid, the same first step [`JobTable::cleanup`] takes for
    /// every group at exit, letting the process decide how to wind down
    /// rather than reaching for `SIGKILL` directly.
    fn cancel(&self) {
        #[cfg(unix)]
        unsafe {
            libc::kill(-self.pgid, libc::SIGTERM);
        }
        #[cfg(windows)]
        {
            ral_core::process::kill_pipeline_group(ral_core::process::Pgid(self.pgid));
        }
    }
}

pub struct JobTable {
    jobs: HashMap<usize, Job>,
}

impl Default for JobTable {
    fn default() -> Self {
        Self::new()
    }
}

impl JobTable {
    pub fn new() -> Self {
        Self {
            jobs: HashMap::new(),
        }
    }

    /// Register a pipeline group with the table, returning the short job
    /// number shown by `jobs` and accepted by `fg` / `bg`.  The one live
    /// caller is the REPL's SIGTSTP path, which registers a just-frozen
    /// foreground pipeline as `Stopped` (Unix only); `state` remains a
    /// parameter because the unit tests build jobs in either state.
    ///
    /// Windows: only ever reached from those tests; the live REPL path
    /// that registers jobs is `cfg(unix)`-gated.
    #[cfg_attr(not(unix), allow(dead_code))]
    pub fn add(&mut self, pgid: i32, cmd: String, state: JobState) -> usize {
        let id = self.jobs.keys().max().map_or(1, |n| n + 1);
        self.jobs.insert(
            id,
            Job {
                id,
                pgid,
                cmd,
                state,
            },
        );
        id
    }

    /// Mark every job with this pgid as stopped (e.g. observed via
    /// `WIFSTOPPED` after a SIGTSTP delivered to the group on Unix).
    pub fn stop(&mut self, pgid: i32) {
        for job in self.jobs.values_mut() {
            if job.pgid == pgid {
                job.state = JobState::Stopped;
            }
        }
    }

    /// Mark every job with this pgid as running (observed via
    /// `WIFCONTINUED` when the group is resumed out-of-band — an external
    /// `kill -CONT` rather than this shell's `bg`/`fg`).  Without it a job
    /// continued behind the shell's back shows `stopped` forever.
    #[cfg(unix)]
    pub fn mark_running(&mut self, pgid: i32) {
        for job in self.jobs.values_mut() {
            if job.pgid == pgid {
                job.state = JobState::Running;
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

    /// Id of the most recently registered job, or `None` when the table is
    /// empty.  Ids are monotone (see [`Self::add`]), so the maximum is the
    /// "current job" SPEC §18 makes the default for a bare `fg`/`bg`/`disown`.
    pub fn most_recent_id(&self) -> Option<usize> {
        self.jobs.keys().max().copied()
    }

    /// Flip a stopped job's bookkeeping back to `Running` and return
    /// its pgid.  Does **not** send `SIGCONT` — the order matters for
    /// `fg`, where the controlling terminal must be handed to the
    /// group *before* `SIGCONT` (otherwise a tty read by the resumed
    /// members races to SIGTTIN and re-stops the group).
    /// [`wait_foreground`] does the SIGCONT after the handoff;
    /// [`Self::resume_in_background`] does it inline for `bg`.
    ///
    /// Idempotent for an already-running job: the state flip is gated
    /// on `Stopped`, the pgid lookup is not.
    pub fn resume(&mut self, id: usize) -> Option<i32> {
        let job = self.jobs.get_mut(&id)?;
        if job.state == JobState::Stopped {
            job.state = JobState::Running;
        }
        Some(job.pgid)
    }

    /// `bg`'s use of [`Self::resume`]: flip bookkeeping back to
    /// `Running` and `SIGCONT` the pgid so the job actually runs in
    /// the background.  Returns the pgid for parity with `resume`, or
    /// `None` when the id is unknown.
    ///
    /// Not for `fg`: there the SIGCONT must be sequenced after the
    /// tty handoff inside [`wait_foreground`].  Idempotent for an
    /// already-running job (the SIGCONT is a kernel no-op).
    pub fn resume_in_background(&mut self, id: usize) -> Option<i32> {
        let pgid = self.resume(id)?;
        #[cfg(unix)]
        unsafe {
            libc::kill(-pgid, libc::SIGCONT);
        }
        Some(pgid)
    }

    /// Reap any pipeline members that have exited or stopped (non-blocking).
    ///
    /// Unix: `waitpid(-pgid, …, WNOHANG | WUNTRACED)` drains ready events
    /// until the call would block (`r == 0`) or no members remain
    /// (`r < 0`, ECHILD), in which case the job is dropped from the
    /// table.
    ///
    /// Windows: poll the leader's duplicated process handle with a zero
    /// timeout via `try_reap_leader`.  An exit (or an unknown / closed
    /// group) drops the entry, and closing the Job handle via
    /// `release_win_group` takes any straggler members down through
    /// `KILL_ON_JOB_CLOSE`.  This collapses the whole pipeline as soon
    /// as the leader exits — strictly aggressive compared to Unix's
    /// "wait for every member to be reaped" semantics, but acceptable
    /// while the `JobTable` on Windows can only ever hold a single-stage
    /// foreground job (no SIGTSTP, no `&`-flow yet).  When `&` is wired
    /// through, this should grow into a Job-Object-`ACTIVE_PROCESS_ZERO`
    /// poll instead.
    pub fn reap(&mut self) {
        #[cfg(unix)]
        {
            let entries: Vec<(usize, i32)> =
                self.jobs.iter().map(|(id, j)| (*id, j.pgid)).collect();
            for (id, pgid) in entries {
                loop {
                    let (r, status) =
                        waitpid_eintr(-pgid, libc::WNOHANG | libc::WUNTRACED | libc::WCONTINUED);
                    if r == 0 {
                        break; // nothing pending — leave the entry alone
                    }
                    if r < 0 {
                        // ECHILD: every member is reaped, group is gone.
                        self.jobs.remove(&id);
                        break;
                    }
                    if libc::WIFSTOPPED(status) {
                        self.stop(pgid);
                    } else if libc::WIFCONTINUED(status) {
                        // Resumed out-of-band (external `kill -CONT`); without
                        // `WCONTINUED` the job would read `stopped` forever.
                        self.mark_running(pgid);
                    }
                    // Exit / signaled — keep draining.
                }
            }
        }
        #[cfg(windows)]
        {
            use ral_core::process::{Pgid, ReapStatus, release_win_group, try_reap_leader};
            let entries: Vec<(usize, i32)> =
                self.jobs.iter().map(|(id, j)| (*id, j.pgid)).collect();
            for (id, pgid) in entries {
                match try_reap_leader(Pgid(pgid)) {
                    ReapStatus::Running => {}
                    ReapStatus::Exited(_) | ReapStatus::Unknown => {
                        release_win_group(pgid);
                        self.jobs.remove(&id);
                    }
                }
            }
        }
    }

    /// On shell exit: take every group down gracefully, wait 5s, then
    /// kill survivors (§13.3).
    ///
    /// Unix: `SIGTERM` → grace → `SIGKILL`, reaping along the way to
    /// avoid zombies.  Windows: closing the Job handle with
    /// `KILL_ON_JOB_CLOSE` is already a hard kill, so the grace window
    /// only buys time for jobs to finish naturally; survivors get
    /// `TerminateJobObject` via `kill_pipeline_group`.
    pub fn cleanup(&mut self) {
        if self.jobs.is_empty() {
            return;
        }

        #[cfg(unix)]
        {
            for job in self.jobs.values() {
                unsafe { libc::kill(-job.pgid, libc::SIGTERM) };
                // SIGCONT a stopped group after the SIGTERM: a frozen job
                // cannot act on (or even observe) the termination request
                // until it runs again, so without this the grace window
                // elapses doing nothing and a suspended editor is SIGKILLed
                // with no chance to clean up.  POSIX shells do the same.
                unsafe { libc::kill(-job.pgid, libc::SIGCONT) };
            }

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !self.jobs.is_empty() && std::time::Instant::now() < deadline {
                self.reap();
                if !self.jobs.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }

            for job in self.jobs.values() {
                unsafe { libc::kill(-job.pgid, libc::SIGKILL) };
                // Drain until ECHILD; `waitpid_eintr` swallows EINTR so
                // we cannot leave a SIGKILL'd child unreaped just
                // because an unrelated signal landed during shutdown.
                while waitpid_eintr(-job.pgid, 0).0 > 0 {}
            }
            self.jobs.clear();
        }

        #[cfg(windows)]
        {
            use ral_core::process::{Pgid, kill_pipeline_group, release_win_group};

            // Polite first pass: a 5s grace where natural exits are
            // reaped.  No SIGTERM equivalent — the Ctrl-Break fan-out
            // happens through the SetConsoleCtrlHandler path on the
            // shell-exit signal; here we just give the children time.
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !self.jobs.is_empty() && std::time::Instant::now() < deadline {
                self.reap();
                if !self.jobs.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }

            // Hard stop: `TerminateJobObject` for survivors, then
            // release the Job handle.
            for job in self.jobs.values() {
                kill_pipeline_group(Pgid(job.pgid));
                release_win_group(job.pgid);
            }
            self.jobs.clear();
        }
    }
}

/// Result of waiting for a foreground job.
pub struct ForegroundWait {
    pub stopped_by: Option<ral_core::process::Signal>,
}

impl ForegroundWait {
    /// True when the job stopped rather than exiting.
    pub fn stopped(&self) -> bool {
        self.stopped_by.is_some()
    }
}

/// Hand the controlling terminal to `pgid`, SIGCONT the group, and
/// block until the whole pipeline exits or any member parks again.
///
/// Unix discipline:
///
///   1. acquire a [`ral_core::process::ForegroundGuard`] for `pgid` —
///      that snapshots the shell's foreground pgid and termios and
///      runs `tcsetpgrp(STDIN_FILENO, pgid)`.  Drop reverses both with
///      EINTR-retried syscalls, so every early return (including a
///      panic anywhere below) restores the tty;
///   2. `kill(-pgid, SIGCONT)` **after** the handoff — otherwise a
///      resumed member that immediately reads the tty hits SIGTTIN
///      (children have SIGTTIN at default disposition via
///      `reset_child_signals`) and re-stops the whole group before we
///      ever hand it the terminal.  Idempotent for a group that was
///      already running (this entrypoint handles both `fg <stopped>`
///      and `fg <running-bg>`);
///   3. `waitpid(-pgid, …, WUNTRACED)` loop with EINTR retry.  A
///      `WIFSTOPPED` on any member returns with `stopped_by` set; an
///      `ECHILD` drain returns `stopped_by = None`.  Misclassifying
///      EINTR as "group is gone" would let the caller remove a live
///      job from the table — `waitpid_eintr` rules that out;
///   4. on stop, SIGSTOP the whole `-pgid` so any sibling stages that
///      were still running park together — mirrors
///      `PipelineCollector::note_stop`, which handles the
///      initial-launch path.  Idempotent for the Ctrl-Z case where the
///      kernel already stopped every member through the terminal;
///   5. drop closes the loop: foreground pgid restored, then termios
///      with `TCSADRAIN` so the child's last buffered output drains
///      under its own settings before ours reapply.
///
/// Windows: blocks on the leader's duplicated process handle.  There
/// is nothing to hand the console to (it's shared across attached
/// processes), and no SIGTSTP analogue — `stopped_by` is therefore
/// always `None`; a stopped foreground job is unreachable on Windows.
pub fn wait_foreground(pgid: i32, shell: &Shell) -> ForegroundWait {
    #[cfg(unix)]
    {
        // RAII handoff: tcsetpgrp + termios snapshot on acquire,
        // restored on drop.  `None` when the turn holds no terminal
        // lease (e.g. a non-interactive resume) — exactly when there
        // is no tty handoff to do, so we still SIGCONT and wait but
        // skip the tty dance.
        let _fg_guard = shell.terminal_lease().and_then(|lease| {
            ral_core::process::ForegroundGuard::try_acquire(pgid as libc::pid_t, lease)
        });
        // SIGCONT after the tty handoff: the kernel-level race that
        // would otherwise drop the group back into Stopped is gone by
        // construction.  Sending to the whole pgid (not just the
        // leader) wakes pipeline siblings together.
        unsafe { libc::kill(-pgid, libc::SIGCONT) };

        loop {
            let (r, status) = waitpid_eintr(-pgid, libc::WUNTRACED);
            if r < 0 {
                // ECHILD: every member exited and was reaped.
                break ForegroundWait { stopped_by: None };
            }
            if libc::WIFSTOPPED(status) {
                // Park any still-running siblings before _fg_guard
                // hands the tty back: a partial-stop (programmatic
                // `kill -STOP` on one member) would otherwise leave
                // the rest running while the job table marks the pgid
                // Stopped.  For Ctrl-Z this is idempotent — the kernel
                // already stopped every member through the controlling
                // tty.
                unsafe { libc::kill(-pgid, libc::SIGSTOP) };
                break ForegroundWait {
                    stopped_by: Some(ral_core::process::Signal::new(libc::WSTOPSIG(status))),
                };
            }
            // Exit or signal: keep waiting for the rest of the group.
        }
        // _fg_guard drops here: tty pgid + termios restored.
    }
    #[cfg(windows)]
    {
        let _ = shell;
        let _ = ral_core::process::wait_leader_blocking(ral_core::process::Pgid(pgid));
        ForegroundWait { stopped_by: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(id: usize, pgid: i32, cmd: &str, state: JobState) -> Job {
        Job {
            id,
            pgid,
            cmd: cmd.into(),
            state,
        }
    }

    #[test]
    fn add_assigns_monotonic_ids() {
        let mut jt = JobTable::new();
        assert_eq!(jt.add(1001, "a".into(), JobState::Running), 1);
        assert_eq!(jt.add(1002, "b".into(), JobState::Stopped), 2);
        assert_eq!(jt.add(1003, "c".into(), JobState::Running), 3);
    }

    #[test]
    fn list_returns_jobs_in_id_order() {
        let mut jt = JobTable::new();
        jt.add(1001, "first".into(), JobState::Running);
        jt.add(1002, "second".into(), JobState::Running);
        let listed: Vec<_> = jt.list().iter().map(|j| j.cmd.clone()).collect();
        assert_eq!(listed, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn stop_marks_every_job_with_pgid() {
        let mut jt = JobTable::new();
        let _ = jt.add(1001, "a".into(), JobState::Running);
        let _ = jt.add(1001, "duplicate-pgid".into(), JobState::Running);
        let _ = jt.add(1002, "other".into(), JobState::Running);
        jt.stop(1001);
        for j in jt.list() {
            let want = if j.pgid == 1001 {
                JobState::Stopped
            } else {
                JobState::Running
            };
            assert_eq!(j.state, want, "job {} state wrong", j.id);
        }
    }

    #[test]
    fn remove_drops_the_entry() {
        let mut jt = JobTable::new();
        let id = jt.add(1001, "a".into(), JobState::Running);
        let removed = jt.remove(id).expect("removed");
        assert_eq!(removed, job(id, 1001, "a", JobState::Running));
        assert!(jt.list().is_empty());
    }

    #[test]
    fn resume_flips_stopped_to_running_and_returns_pgid() {
        // `resume` is pure bookkeeping — the SIGCONT lives at the
        // call site (in `bg`'s libc::kill, in `wait_foreground`'s
        // tcsetpgrp-then-SIGCONT dance for `fg`).  The fake pgid is
        // never signalled here.
        let mut jt = JobTable::new();
        let id = jt.add(99_999_991, "x".into(), JobState::Stopped);
        assert_eq!(jt.resume(id), Some(99_999_991));
        assert_eq!(jt.list()[0].state, JobState::Running);
    }

    #[test]
    fn most_recent_id_is_the_highest() {
        let mut jt = JobTable::new();
        assert_eq!(jt.most_recent_id(), None);
        jt.add(1001, "a".into(), JobState::Running);
        jt.add(1002, "b".into(), JobState::Stopped);
        assert_eq!(jt.most_recent_id(), Some(2));
        jt.remove(2);
        assert_eq!(jt.most_recent_id(), Some(1));
    }

    #[cfg(unix)]
    #[test]
    fn mark_running_flips_every_job_with_pgid() {
        let mut jt = JobTable::new();
        let _ = jt.add(2001, "a".into(), JobState::Stopped);
        let _ = jt.add(2001, "same-pgid".into(), JobState::Stopped);
        let _ = jt.add(2002, "other".into(), JobState::Stopped);
        jt.mark_running(2001);
        for j in jt.list() {
            let want = if j.pgid == 2001 {
                JobState::Running
            } else {
                JobState::Stopped
            };
            assert_eq!(j.state, want, "job {} state wrong", j.id);
        }
    }

    #[test]
    fn resume_on_running_job_is_a_lookup_only_noop() {
        // Idempotent for an already-running job: the state stays
        // `Running` and the pgid still comes back so `bg <running>` /
        // `fg <running>` keep working (SIGCONT to a running pgid is a
        // kernel no-op).
        let mut jt = JobTable::new();
        let id = jt.add(99_999_992, "y".into(), JobState::Running);
        assert_eq!(jt.resume(id), Some(99_999_992));
        assert_eq!(jt.list()[0].state, JobState::Running);
    }

    impl PartialEq for Job {
        fn eq(&self, other: &Self) -> bool {
            self.id == other.id
                && self.pgid == other.pgid
                && self.cmd == other.cmd
                && self.state == other.state
        }
    }

    /// A running job answers every [`Resident`] facet: designator,
    /// population, capability kind, state label, and lease row.
    #[test]
    fn resident_facets_for_a_running_job() {
        let j = job(4, 1234, "sleep 100", JobState::Running);
        assert_eq!(j.designator(), "4");
        assert_eq!(j.population(), "job");
        assert_eq!(j.capability_kind(), "pgid");
        assert_eq!(j.state_label(), "running");
        assert_eq!(j.lease_row(), "none — human-owned");
    }

    /// A stopped job's state label and lease row — the ADR's own degenerate
    /// example, "none; a human owns it (a stopped job)".
    #[test]
    fn resident_facets_for_a_stopped_job() {
        let j = job(5, 1234, "vim", JobState::Stopped);
        assert_eq!(j.state_label(), "stopped");
        assert_eq!(j.lease_row(), "none — human-owned");
    }
}
