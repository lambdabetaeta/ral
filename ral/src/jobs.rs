//! Job control for the interactive shell.
//!
//! Tracks process groups *parked by the kernel* by their process-group
//! id: the live REPL path registers a job only when a foreground pipeline
//! is stopped by SIGTSTP (`Escape::Stopped`).  `Running` is a transient
//! state a group passes through while `bg`/`fg` resumes a stopped job; a
//! job leaves the table when its group terminates.  A `spawn`ed pipeline is
//! an in-process handle, not a pgid job, and is not tracked here.
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
//! gracefully first (`SIGTERM` to the pgid on Unix; `CTRL_BREAK_EVENT`
//! to every member on Windows, via the same escalation-ladder primitive
//! `SetConsoleCtrlHandler` uses), then a 5-second grace during which
//! natural exits are reaped, then forcibly (`SIGKILL` /
//! `TerminateJobObject`).
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

use ral_core::process::Pgid;
use ral_core::types::{Mooring, Resident};
use ral_core::{PendingWrite, Shell};
use std::collections::HashMap;

#[cfg(unix)]
use ral_core::process::{try_waitpgid_eintr, waitpgid_eintr};
#[cfg(unix)]
use rustix::io::Errno;
#[cfg(unix)]
use rustix::process::WaitOptions;

#[derive(Debug, Clone)]
pub struct Job {
    pub id: usize,
    /// Process-group id of the whole pipeline.  All signal and wait
    /// operations on the job target the whole group so they reach every
    /// stage.
    pub pgid: Pgid,
    pub cmd: String,
    pub state: JobState,
    /// Atomic writes the job staged but has not finished: its members are
    /// still holding the temps open.  How the job ends decides every one of
    /// them, which is why they are parked here and nowhere shorter-lived —
    /// see [`JobTable::settle`].
    pub pending: Vec<PendingWrite>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    Running,
    Stopped,
}

/// The job-table chapter's answer to [`Resident`], alongside core's own for
/// `WorkerEntry`: a bare numeric designator, which the listing fold brackets
/// just as it brackets a worker's `wN`, so neither chapter brackets for
/// itself; and no lease, a pgid job drifting toward reclamation only by the
/// exit sweep and never an idle-observation clock, running or stopped.
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
        {
            let _ = rustix::process::kill_process_group(
                self.pgid.as_pid(),
                rustix::process::Signal::TERM,
            );
        }
        #[cfg(windows)]
        {
            ral_core::process::kill_pipeline_group(self.pgid);
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
    pub fn add(
        &mut self,
        pgid: i32,
        cmd: String,
        state: JobState,
        pending: Vec<PendingWrite>,
    ) -> usize {
        let pgid = Pgid::from_raw(pgid).expect("a job pgid is positive");
        let id = self.jobs.keys().max().map_or(1, |n| n + 1);
        self.jobs.insert(
            id,
            Job {
                id,
                pgid,
                cmd,
                state,
                pending,
            },
        );
        id
    }

    /// Finish job `id`'s staged writes and drop it from the table.
    ///
    /// `completed` says whether the job reached its own end successfully: on
    /// success each temp is renamed onto its target, and on anything else —
    /// a non-zero exit, a signal, `disown`, the exit sweep — the temp is
    /// unlinked and the target keeps what it had.  This is the whole of the
    /// atomic-write invariant, stated once, for the lifetime that owns it.
    pub fn settle(&mut self, id: usize, completed: bool) {
        let Some(job) = self.jobs.remove(&id) else {
            return;
        };
        for write in job.pending {
            if completed {
                let target = write.target.clone();
                if let Err(e) = write.commit() {
                    eprintln!(
                        "ral: job [{id}] finished, but its write to {} could not land: {e} \
                         — the file still holds what it did before the job started.",
                        target.display()
                    );
                }
            } else {
                write.abandon();
            }
        }
    }

    /// Empty the table after the kill pass: whatever is still here was taken
    /// down rather than allowed to finish, so its staged writes go unfinished
    /// too.
    fn discard_survivors(&mut self) {
        for (_, job) in self.jobs.drain() {
            for write in job.pending {
                write.abandon();
            }
        }
    }

    /// Detach a job from the shell, handing back the pgid it had.
    ///
    /// Its staged writes go unfinished: once the row is gone nothing is left
    /// to rename them, so each target keeps exactly what it had.
    pub fn disown(&mut self, id: usize) -> Option<Pgid> {
        let pgid = self.jobs.get(&id)?.pgid;
        self.settle(id, false);
        Some(pgid)
    }

    /// Mark every job with this pgid as stopped (e.g. observed through a
    /// stopped wait status after SIGTSTP reaches the group on Unix).
    pub fn stop(&mut self, pgid: Pgid) {
        for job in self.jobs.values_mut() {
            if job.pgid == pgid {
                job.state = JobState::Stopped;
            }
        }
    }

    /// Mark every job with this pgid as running (observed through a
    /// continued wait status when the group is resumed out-of-band — an
    /// external `kill -CONT` rather than this shell's `bg`/`fg`). Without it
    /// a job continued behind the shell's back shows `stopped` forever.
    #[cfg(unix)]
    pub fn mark_running(&mut self, pgid: Pgid) {
        for job in self.jobs.values_mut() {
            if job.pgid == pgid {
                job.state = JobState::Running;
            }
        }
    }

    /// List all jobs.
    pub fn list(&self) -> Vec<&Job> {
        let mut jobs: Vec<_> = self.jobs.values().collect();
        jobs.sort_by_key(|j| j.id);
        jobs
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
    pub fn resume(&mut self, id: usize) -> Option<Pgid> {
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
    pub fn resume_in_background(&mut self, id: usize) -> Option<Pgid> {
        let pgid = self.resume(id)?;
        #[cfg(unix)]
        {
            let _ =
                rustix::process::kill_process_group(pgid.as_pid(), rustix::process::Signal::CONT);
        }
        Some(pgid)
    }

    /// Reap any pipeline members that have exited or stopped (non-blocking).
    ///
    /// Unix: `waitpgid_eintr` with `NOHANG | UNTRACED` drains ready events;
    /// ECHILD drops the job from the table. Windows: poll the leader via
    /// `try_reap_leader`; an exit (or unknown group) drops the entry and
    /// closes the Job handle, killing stragglers through `KILL_ON_JOB_CLOSE`.
    /// No live path populates `JobTable` on Windows — the only live
    /// [`Self::add`] caller is the Unix `Break::Stopped` arm in `repl/exec.rs`
    /// (`&` registers `Worker`s, never `JobTable` jobs) — so the Windows arm
    /// here is exercised only by unit-test fixtures; revisit if that changes.
    pub fn reap(&mut self) {
        #[cfg(unix)]
        {
            let entries: Vec<(usize, Pgid)> =
                self.jobs.iter().map(|(id, j)| (*id, j.pgid)).collect();
            for (id, pgid) in entries {
                // The leader's exit status is what settles the job's staged
                // writes, so it is kept rather than drained past.
                let mut leader_completed = false;
                loop {
                    match try_waitpgid_eintr(pgid, WaitOptions::UNTRACED | WaitOptions::CONTINUED) {
                        Ok(None) => break,
                        Err(Errno::CHILD) => {
                            // ECHILD: every member is reaped, group is gone.
                            self.settle(id, leader_completed);
                            break;
                        }
                        // The trace is the error's only reader, and a release
                        // build has no trace.
                        #[cfg_attr(not(debug_assertions), allow(unused_variables))]
                        Err(err) => {
                            ral_core::dbg_trace!(
                                "jobs",
                                "waitpgid({pgid}) failed: {err}; keeping job"
                            );
                            break;
                        }
                        Ok(Some((pid, status))) => {
                            if status.stopped() {
                                self.stop(pgid);
                            } else if status.continued() {
                                // Resumed out-of-band (external `kill -CONT`);
                                // observing continued statuses keeps the job
                                // from reading `stopped` forever.
                                self.mark_running(pgid);
                            } else if pid == pgid.as_pid() {
                                leader_completed = status.exit_status() == Some(0);
                            }
                            // Keep draining the rest of the group.
                        }
                    }
                }
            }
        }
        #[cfg(windows)]
        {
            use ral_core::process::{ReapStatus, release_win_group, try_reap_leader};
            let entries: Vec<(usize, Pgid)> =
                self.jobs.iter().map(|(id, j)| (*id, j.pgid)).collect();
            for (id, pgid) in entries {
                match try_reap_leader(pgid) {
                    ReapStatus::Running => {}
                    status => {
                        release_win_group(pgid.as_raw());
                        self.settle(id, matches!(status, ReapStatus::Exited(0)));
                    }
                }
            }
        }
    }

    /// On shell exit: take every group down gracefully, wait 5s, then
    /// kill survivors.
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
                let pgid = job.pgid.as_pid();
                let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::TERM);
                // SIGCONT a stopped group after the SIGTERM: a frozen job
                // cannot act on (or even observe) the termination request
                // until it runs again, so without this the grace window
                // elapses doing nothing and a suspended editor is SIGKILLed
                // with no chance to clean up.  POSIX shells do the same.
                let _ = rustix::process::kill_process_group(pgid, rustix::process::Signal::CONT);
            }

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while !self.jobs.is_empty() && std::time::Instant::now() < deadline {
                self.reap();
                if !self.jobs.is_empty() {
                    std::thread::sleep(std::time::Duration::from_millis(50));
                }
            }

            for job in self.jobs.values() {
                let pgid = job.pgid;
                let _ = rustix::process::kill_process_group(
                    pgid.as_pid(),
                    rustix::process::Signal::KILL,
                );
                // Drain until ECHILD; `waitpgid_eintr` swallows EINTR so
                // we cannot leave a SIGKILL'd child unreaped just
                // because an unrelated signal landed during shutdown.
                while waitpgid_eintr(pgid, WaitOptions::empty()).is_ok() {}
            }
            self.discard_survivors();
        }

        #[cfg(windows)]
        {
            use ral_core::process::{break_pipeline_group, kill_pipeline_group, release_win_group};

            // Polite first pass: `CTRL_BREAK_EVENT` to every job, then a
            // 5s grace where natural exits are reaped — the Windows
            // analogue of the Unix arm's SIGTERM.
            for job in self.jobs.values() {
                break_pipeline_group(job.pgid);
            }

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
                kill_pipeline_group(job.pgid);
                release_win_group(job.pgid.as_raw());
            }
            self.discard_survivors();
        }
    }
}

/// Result of waiting for a foreground job.
pub struct ForegroundWait {
    pub stopped_by: Option<ral_core::process::Signal>,
    /// Whether the group leader exited 0 — what [`JobTable::settle`] needs to
    /// choose between finishing the job's staged writes and dropping them.
    /// `false` whenever the job stopped instead of ending.
    pub completed: bool,
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
///   3. `waitpgid_eintr` with `UNTRACED` loops over typed statuses. A
///      stopping signal from any member returns with `stopped_by` set; an
///      `ECHILD` drain returns `stopped_by = None`. Misclassifying EINTR as
///      "group is gone" would let the caller remove a live job from the
///      table — the retrying funnel rules that out;
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
pub fn wait_foreground(pgid: Pgid, mooring: &Mooring, shell: &Shell) -> ForegroundWait {
    #[cfg(unix)]
    {
        // RAII handoff: tcsetpgrp + termios snapshot on acquire,
        // restored on drop.  `None` when the run holds no terminal
        // lease (e.g. a non-interactive resume) — exactly when there
        // is no tty handoff to do, so we still SIGCONT and wait but
        // skip the tty dance.
        let _fg_guard = shell.terminal_lease(mooring).and_then(|lease| {
            ral_core::process::ForegroundGuard::try_acquire(pgid.as_raw(), lease)
        });
        // SIGCONT after the tty handoff: the kernel-level race that
        // would otherwise drop the group back into Stopped is gone by
        // construction.  Sending to the whole pgid (not just the
        // leader) wakes pipeline siblings together.
        let _ = rustix::process::kill_process_group(pgid.as_pid(), rustix::process::Signal::CONT);

        // The leader's exit status outlives the drain: it is what settles the
        // job's staged writes once the whole group is gone.
        let mut completed = false;
        loop {
            let Ok((pid, status)) = waitpgid_eintr(pgid, WaitOptions::UNTRACED) else {
                // ECHILD: every member exited and was reaped.
                break ForegroundWait {
                    stopped_by: None,
                    completed,
                };
            };
            if let Some(signal) = status.stopping_signal() {
                // Park any still-running siblings before _fg_guard
                // hands the tty back: a partial-stop (programmatic
                // `kill -STOP` on one member) would otherwise leave
                // the rest running while the job table marks the pgid
                // Stopped.  For Ctrl-Z this is idempotent — the kernel
                // already stopped every member through the controlling
                // tty.
                let _ = rustix::process::kill_process_group(
                    pgid.as_pid(),
                    rustix::process::Signal::STOP,
                );
                break ForegroundWait {
                    stopped_by: Some(ral_core::process::Signal::new(signal)),
                    completed: false,
                };
            }
            if pid == pgid.as_pid() {
                completed = status.exit_status() == Some(0);
            }
            // Exit or signal: keep waiting for the rest of the group.
        }
        // _fg_guard drops here: tty pgid + termios restored.
    }
    #[cfg(windows)]
    {
        let _ = (shell, mooring);
        let completed = matches!(
            ral_core::process::wait_leader_blocking(pgid),
            ral_core::process::ReapStatus::Exited(0)
        );
        ForegroundWait {
            stopped_by: None,
            completed,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] staging a temp beside a target in a scratch directory, then reading the target back, is what these tests are about"
)]
mod tests {
    use super::*;

    fn pgid(raw: i32) -> Pgid {
        Pgid::from_raw(raw).expect("the test pgid is positive")
    }

    fn job(id: usize, pgid: i32, cmd: &str, state: JobState) -> Job {
        Job {
            id,
            pgid: self::pgid(pgid),
            cmd: cmd.into(),
            state,
            pending: Vec::new(),
        }
    }

    /// Stage a write the way an atomic `>` does — a temp beside the target,
    /// holding what the job would have put there — and hand back both paths
    /// plus the pending write that would finish it.
    fn staged(
        dir: &std::path::Path,
        name: &str,
        bytes: &str,
    ) -> (PendingWrite, std::path::PathBuf) {
        let target = dir.join(name);
        let tmp = dir.join(format!(".{name}.ral-write.tmp"));
        std::fs::write(&tmp, bytes).expect("the staged temp is writable");
        (
            PendingWrite {
                tmp,
                target: target.clone(),
            },
            target,
        )
    }

    #[test]
    fn add_assigns_monotonic_ids() {
        let mut jt = JobTable::new();
        assert_eq!(jt.add(1001, "a".into(), JobState::Running, Vec::new()), 1);
        assert_eq!(jt.add(1002, "b".into(), JobState::Stopped, Vec::new()), 2);
        assert_eq!(jt.add(1003, "c".into(), JobState::Running, Vec::new()), 3);
    }

    #[test]
    fn list_returns_jobs_in_id_order() {
        let mut jt = JobTable::new();
        jt.add(1001, "first".into(), JobState::Running, Vec::new());
        jt.add(1002, "second".into(), JobState::Running, Vec::new());
        let listed: Vec<_> = jt.list().iter().map(|j| j.cmd.clone()).collect();
        assert_eq!(listed, vec!["first".to_string(), "second".to_string()]);
    }

    #[test]
    fn stop_marks_every_job_with_pgid() {
        let mut jt = JobTable::new();
        let _ = jt.add(1001, "a".into(), JobState::Running, Vec::new());
        let _ = jt.add(1001, "duplicate-pgid".into(), JobState::Running, Vec::new());
        let _ = jt.add(1002, "other".into(), JobState::Running, Vec::new());
        jt.stop(pgid(1001));
        for j in jt.list() {
            let want = if j.pgid == pgid(1001) {
                JobState::Stopped
            } else {
                JobState::Running
            };
            assert_eq!(j.state, want, "job {} state wrong", j.id);
        }
    }

    #[test]
    fn settle_drops_the_entry() {
        let mut jt = JobTable::new();
        let id = jt.add(1001, "a".into(), JobState::Running, Vec::new());
        jt.settle(id, true);
        assert!(jt.list().is_empty());
    }

    /// A job that reaches its own end replaces the target, however long it
    /// spent stopped on the way there.
    #[test]
    fn settling_a_completed_job_lands_its_staged_write() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("out"), "old").expect("the target is writable");
        let (write, target) = staged(dir.path(), "out", "new");

        let mut jt = JobTable::new();
        let id = jt.add(1001, "c > out".into(), JobState::Stopped, vec![write]);
        jt.settle(id, true);

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new");
    }

    /// Every other ending — a failure, a signal, `disown`, the exit sweep —
    /// leaves the target exactly as it was and takes the temp with it.
    #[test]
    fn settling_an_unfinished_job_leaves_the_target_alone() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("out"), "old").expect("the target is writable");
        let (write, target) = staged(dir.path(), "out", "new");
        let tmp = write.tmp.clone();

        let mut jt = JobTable::new();
        let id = jt.add(1001, "c > out".into(), JobState::Stopped, vec![write]);
        jt.settle(id, false);

        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        assert!(!tmp.exists(), "the staged temp outlived the job");
    }

    /// `disown` is an unfinished ending: nothing is left to rename the temp,
    /// so the target must keep what it had rather than change later.
    #[test]
    fn disown_gives_back_the_pgid_and_drops_the_staged_write() {
        let dir = tempfile::tempdir().expect("a scratch directory");
        std::fs::write(dir.path().join("out"), "old").expect("the target is writable");
        let (write, target) = staged(dir.path(), "out", "new");

        let mut jt = JobTable::new();
        let id = jt.add(99_999_993, "c > out".into(), JobState::Stopped, vec![write]);

        assert_eq!(jt.disown(id), Some(pgid(99_999_993)));
        assert_eq!(jt.disown(id), None);
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
    }

    #[test]
    fn resume_flips_stopped_to_running_and_returns_pgid() {
        // `resume` is pure bookkeeping — the SIGCONT lives at the
        // call site (in `bg`'s kill_process_group, in `wait_foreground`'s
        // tcsetpgrp-then-SIGCONT dance for `fg`).  The fake pgid is
        // never signalled here.
        let mut jt = JobTable::new();
        let id = jt.add(99_999_991, "x".into(), JobState::Stopped, Vec::new());
        assert_eq!(jt.resume(id), Some(pgid(99_999_991)));
        assert_eq!(jt.list()[0].state, JobState::Running);
    }

    #[cfg(unix)]
    #[test]
    fn mark_running_flips_every_job_with_pgid() {
        let mut jt = JobTable::new();
        let _ = jt.add(2001, "a".into(), JobState::Stopped, Vec::new());
        let _ = jt.add(2001, "same-pgid".into(), JobState::Stopped, Vec::new());
        let _ = jt.add(2002, "other".into(), JobState::Stopped, Vec::new());
        jt.mark_running(pgid(2001));
        for j in jt.list() {
            let want = if j.pgid == pgid(2001) {
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
        let id = jt.add(99_999_992, "y".into(), JobState::Running, Vec::new());
        assert_eq!(jt.resume(id), Some(pgid(99_999_992)));
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

    /// A stopped job still reports no lease: stopping is not idleness, and
    /// nothing but the human resumes it.
    #[test]
    fn resident_facets_for_a_stopped_job() {
        let j = job(5, 1234, "vim", JobState::Stopped);
        assert_eq!(j.state_label(), "stopped");
        assert_eq!(j.lease_row(), "none — human-owned");
    }
}
