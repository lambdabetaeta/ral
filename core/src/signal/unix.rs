//! Unix signal handling and process-group machinery.
//!
//! Three concerns interlock here:
//!
//!   * **Termination signals** (SIGINT/SIGTERM/SIGHUP) increment a shared
//!     counter that the evaluator polls between statements.  Third
//!     occurrence forces `_exit(2)`.
//!   * **Pipeline relays** keep the controlling tty with the shell while
//!     mixed pipelines run, fanning Ctrl+C out to every external pgid.
//!   * **Process-group placement** is the discipline applied at fork:
//!     every external child gets `setpgid` + `reset_child_signals` via a
//!     single `pre_exec` funnel.

use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use super::{Pgid, PgidPolicy, SIGNAL_COUNT};

// ── Termination handler ────────────────────────────────────────────────────

/// Install handlers for SIGINT, SIGTERM, SIGHUP.  Snapshots inherited
/// SIG_IGN dispositions *before* installing ral's own handlers so the
/// nohup rule (preserve dispositions the parent deliberately ignored) can
/// be honored in spawned children — see [`reset_child_signals`].
pub fn install_handlers() {
    snapshot_inherited_ignored();
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
    }
}

extern "C" fn handler(_sig: libc::c_int) {
    let prev = SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    if prev >= 2 {
        // Third signal: force exit. Use _exit to avoid atexit deadlocks.
        unsafe { libc::_exit(128 + _sig) };
    }
    // Forward the same signal to any active pipeline groups so external
    // children die too.  In the interactive shell the relay handler is
    // installed instead; this path is reached only in batch mode.
    for slot in &RELAY_PGIDS {
        let pgid = slot.load(Ordering::Acquire);
        if pgid != 0 {
            unsafe {
                libc::kill(-pgid, _sig);
            }
        }
    }
}

/// Return the handler function pointer for selective signal installation.
pub fn term_handler() -> extern "C" fn(libc::c_int) {
    handler
}

// ── Pipeline relay ─────────────────────────────────────────────────────────
//
// When a pipeline has both internal (thread) and external (process) stages,
// we cannot hand the terminal to the external process group — internal threads
// live in the shell process and would receive SIGTTIN.  Instead we keep the
// terminal with the shell and forward SIGINT to every active external group.
//
// `RELAY_PGIDS` is a fixed slot array.  Each `PipelineRelay` claims one slot
// with CAS; the handler iterates all slots and sends to any non-zero entry.
// When no slots are active the handler does nothing — effectively SIG_IGN for
// the shell.  The handler is installed once at startup and never removed, so
// there is no install/uninstall race.

const MAX_RELAY: usize = 8;

static RELAY_PGIDS: [AtomicI32; MAX_RELAY] = [const { AtomicI32::new(0) }; MAX_RELAY];

extern "C" fn sigint_relay(_: libc::c_int) {
    for slot in &RELAY_PGIDS {
        let pgid = slot.load(Ordering::Acquire);
        if pgid != 0 {
            unsafe {
                libc::kill(-pgid, libc::SIGINT);
            }
        }
    }
}

/// Return the relay handler for installation by the interactive shell.
pub fn relay_handler() -> extern "C" fn(libc::c_int) {
    sigint_relay
}

/// RAII guard: holds a slot in `RELAY_PGIDS` for the duration of a mixed
/// pipeline.  Clearing the slot on drop is the only cleanup needed.
pub struct PipelineRelay(usize);

impl PipelineRelay {
    /// Claim an empty slot and record `pgid`.  Returns `None` if all slots
    /// are full (should not happen in practice; 8 concurrent mixed pipelines).
    pub fn install(pgid: libc::pid_t) -> Option<Self> {
        for (i, slot) in RELAY_PGIDS.iter().enumerate() {
            if slot
                .compare_exchange(0, pgid, Ordering::Release, Ordering::Relaxed)
                .is_ok()
            {
                return Some(PipelineRelay(i));
            }
        }
        None
    }
}

impl Drop for PipelineRelay {
    fn drop(&mut self) {
        RELAY_PGIDS[self.0].store(0, Ordering::Release);
    }
}

// ── Inherited dispositions and child-signal reset ──────────────────────────

/// The signals whose startup disposition we snapshot in
/// [`snapshot_inherited_ignored`] and consult in [`reset_child_signals`].
/// Listed once so the two consumers cannot drift.
///
/// SIGPIPE is deliberately absent: Rust's runtime sets SIGPIPE=IGN at startup
/// so panics on broken-pipe writes are graceful, and that disposition would
/// falsely register as "parent intent" by the time ral reads it.  SIGPIPE is
/// reset unconditionally to SIG_DFL — see [`reset_child_signals`].
const MANAGED_SIGNALS: &[libc::c_int] = &[
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGHUP,
];

/// Bitmask of signals that were SIG_IGN when ral started, indexed by signal
/// number.  Captured by [`install_handlers`] before any of ral's own
/// dispositions are installed.  Read in [`reset_child_signals`] to honor the
/// POSIX nohup rule: a signal the parent deliberately set to SIG_IGN must
/// remain SIG_IGN in spawned children.
///
/// All managed signal numbers are < 64, so a single u64 suffices.
static INHERITED_IGNORED: AtomicU64 = AtomicU64::new(0);

fn snapshot_inherited_ignored() {
    let mut mask: u64 = 0;
    for &sig in MANAGED_SIGNALS {
        let mut old: libc::sigaction = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::sigaction(sig, std::ptr::null(), &mut old) };
        if rc == 0 && old.sa_sigaction == libc::SIG_IGN {
            mask |= 1u64 << sig;
        }
    }
    INHERITED_IGNORED.store(mask, Ordering::Release);
}

fn was_inherited_ignored(sig: libc::c_int) -> bool {
    let mask = INHERITED_IGNORED.load(Ordering::Acquire);
    (mask & (1u64 << sig)) != 0
}

/// Restore the appropriate disposition for the signals that ral overrides
/// or ignores in its own process.  Must run from the post-fork pre-exec
/// closure of every external-child spawn.
///
/// Without an explicit reset, every spawned external would inherit ral's
/// handler pointers.  `execve(2)` resets handler pointers to `SIG_DFL`
/// automatically, so this would mostly work — but `SIG_IGN` survives
/// `execve`.  Anything whose disposition should be `SIG_IGN` must therefore
/// be set *explicitly* to `SIG_IGN` here.
///
/// The nohup rule: a signal that was already `SIG_IGN` when ral started —
/// recorded in `INHERITED_IGNORED` — must be `SIG_IGN` in our children too.
/// The parent deliberately set it; that intent has to survive ral.  For
/// every other managed signal, `SIG_DFL` is the right disposition.
///
/// SIGPIPE is special-cased to `SIG_DFL` unconditionally.  Rust's runtime
/// sets SIGPIPE=IGN at startup; that's not user intent and must not
/// propagate — pipeline producers need SIGPIPE=DFL to die cleanly when
/// their reader closes (`yes | head`).
pub fn reset_child_signals() {
    for &sig in MANAGED_SIGNALS {
        let target = if was_inherited_ignored(sig) {
            libc::SIG_IGN
        } else {
            libc::SIG_DFL
        };
        unsafe {
            libc::signal(sig, target);
        }
    }
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

// ── Process-group placement ────────────────────────────────────────────────

impl PgidPolicy {
    /// Apply this policy from inside a `pre_exec` closure.  Safe to call
    /// from the post-fork pre-exec context; no allocation, no stdlib mutex
    /// use.
    ///
    /// Callers should not invoke `setpgid` directly — [`spawn_with_pgid`]
    /// is the single funnel that installs this policy and mirrors it in
    /// the parent.  Searching for `setpgid` should yield this method plus
    /// the parent-side mirror inside [`spawn_with_pgid`].
    pub fn apply(self) {
        unsafe {
            match self {
                PgidPolicy::Inherit => {}
                PgidPolicy::NewLeader => {
                    libc::setpgid(0, 0);
                }
                PgidPolicy::Join(Pgid(leader)) => {
                    libc::setpgid(0, leader);
                }
            }
        }
    }
}

/// Spawn `cmd` with a single, canonical pre-exec discipline:
///
///   1. inside the child (post-fork, pre-exec): apply `pgid`, then
///      [`reset_child_signals`] (with the nohup rule);
///   2. inside the parent (post-spawn): mirror the `setpgid` so the child's
///      pgid is established regardless of which side wins the race.
///
/// Returns the child plus its leader pgid: `Some` for `NewLeader` /
/// `Join`, `None` for `Inherit`.  Callers that need the leader pgid for
/// later [`wait_handling_stop`] or for the pipeline group simply read it
/// off the return value — there is no separate registration step.
///
/// Ordering: `pre_exec` closures run in registration order, so any caller-
/// installed `pre_exec` (sandbox `RLIMIT`, `2>&1` dup2) runs *before* the
/// closure this function adds.  That order is intentional: fd plumbing
/// and rlimits are independent of pgid placement, and `reset_child_signals`
/// is the last thing we want to happen before `execve` clears the slate.
pub fn spawn_with_pgid(
    cmd: &mut std::process::Command,
    pgid: PgidPolicy,
) -> std::io::Result<(std::process::Child, Option<Pgid>)> {
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(move || {
            pgid.apply();
            reset_child_signals();
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let leader = match pgid {
        PgidPolicy::Inherit => None,
        PgidPolicy::NewLeader => {
            let pid = child.id() as libc::pid_t;
            unsafe { libc::setpgid(pid, pid) };
            Some(Pgid(pid))
        }
        PgidPolicy::Join(p) => {
            let pid = child.id() as libc::pid_t;
            unsafe { libc::setpgid(pid, p.0) };
            Some(p)
        }
    };
    Ok((child, leader))
}

// ── Wait handling for stopped children ─────────────────────────────────────
//
// `Child::wait()` calls `waitpid(pid, &status, 0)` which only returns on
// termination.  A child stopped by SIGTSTP (Ctrl-Z), SIGSTOP, or SIGTTIN
// stays stopped indefinitely — the wait blocks, the controlling tty stays
// owned by the stopped pgid, and ral hangs.
//
// `wait_handling_stop` uses `waitpid(pid, ..., WUNTRACED)` so the wait
// returns on stop too.  ral has no job control yet, so the response is
// to kill the entire pgid (so the rest of a pipeline dies together) and
// loop to reap.  The eventual return is always exited or signalled —
// never stopped.

/// Wait for `child` to terminate, killing its pgid if it stops.
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    pgid: Option<Pgid>,
) -> std::io::Result<std::process::ExitStatus> {
    use std::os::unix::process::ExitStatusExt;
    let pid = child.id() as libc::pid_t;
    loop {
        let mut status: libc::c_int = 0;
        let r = unsafe { libc::waitpid(pid, &mut status, libc::WUNTRACED) };
        if r < 0 {
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() == Some(libc::EINTR) {
                continue;
            }
            return Err(err);
        }
        if libc::WIFSTOPPED(status) {
            crate::dbg_trace!(
                "fg",
                "pid {pid} stopped (signal {}); killing pgid {:?}",
                libc::WSTOPSIG(status),
                pgid
            );
            match pgid {
                Some(Pgid(p)) => unsafe {
                    libc::kill(-p, libc::SIGKILL);
                },
                None => {
                    let _ = child.kill();
                }
            }
            // Loop to reap the now-dying child.
            continue;
        }
        return Ok(std::process::ExitStatus::from_raw(status));
    }
}

// ── Foreground ownership ───────────────────────────────────────────────────
//
// `tcsetpgrp` hands the controlling tty to a target process group; it must
// be reversed before the next REPL read or that read returns EIO (ral
// ignores SIGTTIN, see repl.rs).  `ForegroundGuard` makes the restoration
// unconditional under any early return: acquiring the guard performs
// `tcsetpgrp(target)`, dropping it performs `tcsetpgrp(saved)`.

/// RAII guard for terminal foreground ownership.
///
/// `try_acquire` performs `tcsetpgrp(STDIN_FILENO, target)` and remembers
/// the previous foreground pgid; `drop` restores it.  Returns `None` when
/// the shell isn't interactive, stdin isn't a tty, or the syscall fails —
/// in those cases there's nothing to restore.
pub struct ForegroundGuard {
    saved_pgid: libc::pid_t,
}

impl ForegroundGuard {
    /// Hand the controlling tty to `target`, recording the prior pgid for
    /// the eventual restore.  Returns `None` when no handoff is appropriate.
    pub fn try_acquire(target: libc::pid_t, shell: &crate::types::Shell) -> Option<Self> {
        if !shell.io.interactive || !shell.io.terminal.startup_stdin_tty || target == 0 {
            return None;
        }
        let saved = unsafe { libc::getpgrp() };
        let rc = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, target) };
        if rc != 0 {
            #[cfg(debug_assertions)]
            crate::dbg_trace!(
                "fg",
                "acquire: tcsetpgrp({target}) failed: {}",
                std::io::Error::last_os_error()
            );
            return None;
        }
        Some(Self { saved_pgid: saved })
    }
}

impl Drop for ForegroundGuard {
    /// Restore the foreground pgid recorded at acquisition.
    ///
    /// Critical: a missed restore puts ral into a background pgroup whose
    /// next tty read returns EIO.  Retry on EINTR; on persistent failure
    /// log via `dbg_trace` and verify with `tcgetpgrp` so the silent-loss
    /// case is at least observable.
    fn drop(&mut self) {
        for _ in 0..3 {
            let rc = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, self.saved_pgid) };
            if rc == 0 {
                return;
            }
            let err = std::io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                crate::dbg_trace!(
                    "fg",
                    "release: tcsetpgrp({}) failed: {err}",
                    self.saved_pgid
                );
                break;
            }
        }
        let cur = unsafe { libc::tcgetpgrp(libc::STDIN_FILENO) };
        if cur != self.saved_pgid {
            crate::dbg_trace!(
                "fg",
                "release: tty fg is {cur}, want {} (next tty read may EIO)",
                self.saved_pgid
            );
        }
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

/// Count of currently occupied relay slots (for testing).
#[cfg(test)]
fn active_relay_slots() -> usize {
    RELAY_PGIDS
        .iter()
        .filter(|s| s.load(Ordering::Acquire) != 0)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    // All relay tests share a process-wide lock because `RELAY_PGIDS` is a
    // global.  Tests run concurrently in the same process by default;
    // without this they would steal each other's slots.
    static RELAY_TEST_LOCK: Mutex<()> = Mutex::new(());

    // ── Slot allocation ────────────────────────────────────────────────────

    #[test]
    fn slots_fill_and_drain() {
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        // Claim all 8 slots with distinct pgids.
        let guards: Vec<_> = (1..=MAX_RELAY as i32)
            .map(|pgid| PipelineRelay::install(pgid).expect("slot should be free"))
            .collect();
        assert_eq!(active_relay_slots(), MAX_RELAY);

        // A 9th install must fail.
        assert!(PipelineRelay::install(99).is_none());

        // Drop all; every slot must be released.
        drop(guards);
        assert_eq!(active_relay_slots(), 0);
    }

    #[test]
    fn released_slot_is_reusable() {
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        let g1 = PipelineRelay::install(1).unwrap();
        drop(g1);
        // The same pgid should be installable again immediately.
        let g2 = PipelineRelay::install(1).unwrap();
        drop(g2);
        assert_eq!(active_relay_slots(), 0);
    }

    // ── Concurrency stress ─────────────────────────────────────────────────

    #[test]
    fn concurrent_install_drop_stress() {
        // 8 threads race to claim all slots simultaneously, hold briefly,
        // release.  Repeat 500 times.  No slot should ever be double-claimed
        // or leaked.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        const ROUNDS: usize = 500;

        for round in 0..ROUNDS {
            let barrier = Arc::new(Barrier::new(MAX_RELAY));
            let handles: Vec<_> = (0..MAX_RELAY)
                .map(|t| {
                    let b = barrier.clone();
                    std::thread::spawn(move || {
                        b.wait(); // all threads start together
                        let pgid = (round * MAX_RELAY + t + 1) as i32;
                        let g = PipelineRelay::install(pgid)
                            .expect("slot unavailable — possible double-claim");
                        std::thread::yield_now();
                        drop(g);
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            assert_eq!(active_relay_slots(), 0, "slot leak after round {round}");
        }
    }

    #[test]
    fn overflow_returns_none_not_panic() {
        // Fill all slots, then hammer install from many threads
        // simultaneously.  Every extra install must return None, never panic
        // or corrupt state.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        let _guards: Vec<_> = (1..=MAX_RELAY as i32)
            .map(|p| PipelineRelay::install(p).unwrap())
            .collect();

        let handles: Vec<_> = (0..32)
            .map(|_| {
                std::thread::spawn(|| {
                    for pgid in 100..200i32 {
                        let _ = PipelineRelay::install(pgid); // must not panic
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }

    // ── Signal forwarding ──────────────────────────────────────────────────

    #[test]
    fn relay_delivers_sigint_to_child_group() {
        // Spawn `sleep 1000` as a child in its own process group (via
        // pre_exec).  Claim a relay slot for the child's pgid and call
        // sigint_relay directly — equivalent to the shell receiving SIGINT
        // while the pipeline runs.  Verify the child was killed by the
        // signal.
        //
        // We use Command + pre_exec rather than fork() to avoid the hazards
        // of forking inside a multithreaded test binary.
        let _lock = RELAY_TEST_LOCK.lock().unwrap();

        use std::os::unix::process::CommandExt;

        let mut cmd = std::process::Command::new("sleep");
        cmd.arg("1000");
        unsafe {
            cmd.pre_exec(|| {
                libc::setpgid(0, 0);
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                Ok(())
            });
        }
        let mut child = cmd.spawn().expect("spawn sleep");
        let child_pid = child.id() as libc::pid_t;

        // Parent mirrors setpgid to close the race.
        unsafe {
            libc::setpgid(child_pid, child_pid);
        }

        // Claim relay slot and fire the handler directly.
        let _relay = PipelineRelay::install(child_pid).expect("slot");
        sigint_relay(libc::SIGINT);

        let status = child.wait().expect("wait");
        // sleep was killed by SIGINT; exit code should be non-zero.
        assert!(!status.success(), "child should have been killed by SIGINT");
    }
}

