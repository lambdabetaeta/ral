//! Signal handling: SIGINT, SIGTERM, SIGHUP set a flag that the
//! evaluator checks between statements, triggering unwinding.
//!
//! First signal: begin unwinding (guard cleanup runs).
//! Second signal during unwind: deferred until cleanup completes.
//! Third signal: immediate process exit.
//!
//! For mixed internal/external pipelines, PipelineRelay claims a slot in
//! RELAY_PGIDS so that sigint_relay forwards Ctrl+C to that process group.
//! The relay handler is installed permanently by the interactive shell
//! (replacing SIG_IGN); when no slots are active it does nothing.

#[cfg(unix)]
use std::sync::atomic::AtomicI32;
use std::sync::atomic::{AtomicU8, Ordering};

/// 0 = normal, 1 = interrupted, 2 = second signal, >=3 = force exit.
static SIGNAL_COUNT: AtomicU8 = AtomicU8::new(0);

/// Check whether the current evaluation should unwind.
///
/// Two reasons can fire:
///   * a process-level signal (SIGINT / SIGTERM / SIGHUP) — increments
///     the global `SIGNAL_COUNT`;
///   * a structured-concurrency cancel — the shell's `CancelScope` (or
///     any of its ancestors) has been cancelled, e.g. by a
///     `RunningPipeline::Drop` on the abort path.
///
/// Both unwind via the same `EvalSignal::Error` so callers don't need to
/// distinguish them — the message is "interrupted" vs "cancelled".
pub fn check(shell: &crate::types::Shell) -> Result<(), crate::types::EvalSignal> {
    if SIGNAL_COUNT.load(Ordering::Relaxed) >= 1 {
        return Err(crate::types::EvalSignal::Error(
            crate::types::Error::new("interrupted", 130)
                .at(shell.location.line, shell.location.col),
        ));
    }
    if shell.cancel.is_cancelled() {
        return Err(crate::types::EvalSignal::Error(
            crate::types::Error::new("cancelled", 130)
                .at(shell.location.line, shell.location.col),
        ));
    }
    Ok(())
}

/// Clear the signal flag (e.g., after handling in interactive mode).
pub fn clear() {
    SIGNAL_COUNT.store(0, Ordering::Relaxed);
}

/// Returns true if a signal is pending.
pub fn is_interrupted() -> bool {
    SIGNAL_COUNT.load(Ordering::Relaxed) >= 1
}

/// Install signal handlers for SIGINT, SIGTERM, SIGHUP.
/// Must be called once at program startup.
#[cfg(unix)]
pub fn install_handlers() {
    unsafe {
        libc::signal(libc::SIGINT, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        libc::signal(libc::SIGHUP, handler as *const () as libc::sighandler_t);
    }
}

#[cfg(unix)]
extern "C" fn handler(_sig: libc::c_int) {
    let prev = SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
    if prev >= 2 {
        // Third signal: force exit. Use _exit to avoid atexit deadlocks.
        unsafe { libc::_exit(128 + _sig) };
    }
    // Forward the same signal to any active pipeline groups so external
    // children die too.
    // In the interactive shell the relay handler is installed instead; this
    // path is reached only in batch mode (non-interactive).
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
#[cfg(unix)]
pub fn term_handler() -> extern "C" fn(libc::c_int) {
    handler
}

#[cfg(windows)]
pub fn install_handlers() {
    // SetConsoleCtrlHandler via the ctrlc crate.  The handler increments the
    // same SIGNAL_COUNT flag the evaluator polls between statements, and
    // returns TRUE so Windows does not terminate the process.
    let _ = ctrlc::set_handler(|| {
        let prev = SIGNAL_COUNT.fetch_add(1, Ordering::Relaxed);
        if prev >= 2 {
            // Third Ctrl+C: give up and exit immediately.
            std::process::exit(130);
        }
    });
}

#[cfg(not(any(unix, windows)))]
pub fn install_handlers() {}

// ── Pipeline relay ───────────────────────────────────────────────────────────
//
// When a pipeline has both internal (thread) and external (process) stages,
// we cannot hand the terminal to the external process group — internal threads
// live in the shell process and would receive SIGTTIN.  Instead we keep the
// terminal with the shell and forward SIGINT to every active external group.
//
// RELAY_PGIDS is a fixed slot array.  Each PipelineRelay claims one slot with
// CAS; the handler iterates all slots and sends to any non-zero entry.  When
// no slots are active the handler does nothing, which is effectively SIG_IGN
// for the shell.  The handler is installed once at startup and never removed,
// so there is no install/uninstall race.

#[cfg(unix)]
const MAX_RELAY: usize = 8;

#[cfg(unix)]
static RELAY_PGIDS: [AtomicI32; MAX_RELAY] = [const { AtomicI32::new(0) }; MAX_RELAY];

#[cfg(unix)]
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
#[cfg(unix)]
pub fn relay_handler() -> extern "C" fn(libc::c_int) {
    sigint_relay
}

/// RAII guard: holds a slot in RELAY_PGIDS for the duration of a mixed pipeline.
/// Clearing the slot on drop is the only cleanup needed.
#[cfg(unix)]
pub struct PipelineRelay(usize);

#[cfg(unix)]
impl PipelineRelay {
    /// Claim an empty slot and record `pgid`.  Returns `None` if all slots are
    /// full (should not happen in practice; 8 concurrent mixed pipelines).
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

#[cfg(unix)]
impl Drop for PipelineRelay {
    fn drop(&mut self) {
        RELAY_PGIDS[self.0].store(0, Ordering::Release);
    }
}

/// Non-unix stub so callers can use `PipelineRelay` unconditionally.
#[cfg(not(unix))]
pub struct PipelineRelay;

// ── Cooperative cancellation ────────────────────────────────────────────────
//
// `CancelScope` is the structured-concurrency primitive: a tree of Arc-shared
// flags.  A worker checks `is_cancelled` at well-defined poll points (the same
// places `signal::check` is already called); cancelling an outer scope
// propagates to every inner scope, so a top-level Ctrl-C — or a
// `RunningPipeline::Drop` on the abort path — unwinds every thread that
// inherited the scope at its next poll point.
//
// The chain is walked, not flattened, so subscopes can carry their own
// flag (cancelling only their subtree) while still observing parent
// cancellation.  No mutex, no allocation in the hot path — just an
// `AtomicBool::load` per ancestor.

/// Internal node of the cancel-scope tree.  A scope is cancelled if its
/// own flag is set OR any ancestor's flag is set.
#[derive(Debug)]
struct ScopeNode {
    flag: AtomicU8,
    parent: Option<std::sync::Arc<ScopeNode>>,
}

/// A handle into the cancel-scope tree.  Cheap to clone (one `Arc`
/// bump); cheap to check (chain of atomic loads).
///
/// Construction:
///   * `root()` — a fresh top-level scope with no parent.
///   * `child()` — a new scope nested under `self`; cancelling `self` (or
///     any of its ancestors) cancels the child too.
///   * `share()` — clone the *same* scope (no new node); used when
///     handing the current scope to a spawned thread.
///
/// Cancellation is one-way: once cancelled, a scope stays cancelled.
#[derive(Debug, Clone)]
pub struct CancelScope(std::sync::Arc<ScopeNode>);

impl CancelScope {
    /// A fresh root scope.  Used by the default `Shell` and by tests
    /// that don't need cancellation.
    pub fn root() -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: None,
        }))
    }

    /// A new scope nested under `self`.  Cancelling any ancestor (or
    /// `self`) cancels the returned child.  `RunningPipeline` creates
    /// one of these per pipeline so cancelling the pipeline doesn't
    /// reach the parent shell.
    pub fn child(&self) -> Self {
        Self(std::sync::Arc::new(ScopeNode {
            flag: AtomicU8::new(0),
            parent: Some(self.0.clone()),
        }))
    }

    /// Set this scope's flag.  Idempotent.  Visible to every share /
    /// child of this scope at the next `is_cancelled` poll.
    pub fn cancel(&self) {
        self.0.flag.store(1, Ordering::Release);
    }

    /// Walk the parent chain, returning true if any node's flag is set.
    pub fn is_cancelled(&self) -> bool {
        let mut node: &std::sync::Arc<ScopeNode> = &self.0;
        loop {
            if node.flag.load(Ordering::Acquire) != 0 {
                return true;
            }
            match &node.parent {
                Some(p) => node = p,
                None => return false,
            }
        }
    }
}

impl Default for CancelScope {
    fn default() -> Self {
        Self::root()
    }
}

// ── Process-group placement ──────────────────────────────────────────────────
//
// Every spawned external process makes one of three choices about its
// process group: stay in the parent's group, become a leader of a fresh
// group, or join an existing group as a non-leader.  `PgidPolicy` makes
// that choice explicit at the type level — no `pid_t == 0` sentinel, no
// "did the caller remember to setpgid?" comments.

/// Newtype wrapping a Unix process-group ID.  Constructed from a real pid;
/// no `0` sentinel encodes "no pgid" — the `Option<Pgid>` does.
#[cfg(unix)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pgid(pub libc::pid_t);

#[cfg(not(unix))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pgid(pub i32);

/// Process-group placement decision applied via `pre_exec` before `execve`.
///
/// Single-command foreground jobs and pipeline first stages use
/// `NewLeader`; subsequent pipeline stages use `Join(leader_pgid)`;
/// non-pipeline non-foreground children use `Inherit`.
#[derive(Clone, Copy, Debug)]
pub enum PgidPolicy {
    /// Inherit the parent's pgid — no `setpgid` call.
    Inherit,
    /// Become the leader of a fresh process group (`setpgid(0, 0)`).
    NewLeader,
    /// Join an existing pgid as a non-leader (`setpgid(0, leader)`).
    Join(Pgid),
}

#[cfg(unix)]
impl PgidPolicy {
    /// Apply this policy from inside a `pre_exec` closure.  Safe to call from
    /// the post-fork pre-exec context; no allocation, no stdlib mutex use.
    ///
    /// This is the *only* place `setpgid` should be called from spawning
    /// code.  Searching for `setpgid` should yield this single call site
    /// plus a parent-side race-guard that mirrors it (see `exec_external`).
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

#[cfg(not(unix))]
impl PgidPolicy {
    pub fn apply(self) {}
}

/// Restore default disposition for the signals that ral overrides or
/// ignores in its own process.  Must run from the post-fork pre-exec
/// closure of every external-child spawn — it is *not* a foreground-only
/// concern.
///
/// ral installs handlers for SIGINT and ignores SIGTTIN / SIGTTOU /
/// SIGPIPE at startup; without this reset, every spawned external
/// inherits those dispositions and behaves unlike the same command run
/// from a normal shell.  In particular SIGPIPE-IGN means a child that
/// writes past a closed downstream stage gets EPIPE instead of dying —
/// most utilities don't handle EPIPE and end up looping or producing
/// confusing diagnostics on what should be a clean SIGPIPE death.
///
/// Universal — applies to standalone externals, pipeline external stages,
/// foreground or backgrounded.
#[cfg(unix)]
pub fn reset_child_signals() {
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::signal(libc::SIGQUIT, libc::SIG_DFL);
        libc::signal(libc::SIGTSTP, libc::SIG_DFL);
        libc::signal(libc::SIGTTIN, libc::SIG_DFL);
        libc::signal(libc::SIGTTOU, libc::SIG_DFL);
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
}

#[cfg(not(unix))]
pub fn reset_child_signals() {}

// ── Wait handling for stopped children ──────────────────────────────────────
//
// `Child::wait()` calls `waitpid(pid, &status, 0)` which only returns on
// termination.  A child stopped by SIGTSTP (Ctrl-Z), SIGSTOP, or SIGTTIN
// stays stopped indefinitely — the wait blocks, the controlling tty stays
// owned by the stopped pgid, and ral hangs.
//
// `wait_handling_stop` uses `waitpid(pid, ..., WUNTRACED)` so the wait
// returns on stop too.  ral has no job control yet, so the response is
// to kill the entire pgid (so the rest of a pipeline dies together) and
// loop to reap.  The eventual return is always an exited or signalled
// status — never stopped.

/// Wait for `child` to terminate, killing its pgid if it stops.
///
/// Why this exists: see the module-level commentary above.  Returning a
/// stopped status would just push the deadlock up one level — the caller
/// has no way to express "stopped" in the pipeline result type, and
/// without job-control machinery there's nothing useful for it to do
/// with such a status.
#[cfg(unix)]
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

#[cfg(not(unix))]
pub fn wait_handling_stop(
    child: &mut std::process::Child,
    _pgid: Option<Pgid>,
) -> std::io::Result<std::process::ExitStatus> {
    child.wait()
}

// ── Foreground ownership ─────────────────────────────────────────────────────
//
// `tcsetpgrp` hands the controlling tty to a target process group; it must be
// reversed before the next REPL read or that read returns EIO (ral ignores
// SIGTTIN, see repl.rs).  `ForegroundGuard` makes the restoration unconditional
// under any early return: acquiring the guard performs `tcsetpgrp(target)`,
// dropping it performs `tcsetpgrp(saved)`.  Only the unix variant has fields;
// non-unix is a zero-sized stub so callers compile unchanged.

/// RAII guard for terminal foreground ownership.
///
/// `try_acquire` performs `tcsetpgrp(STDIN_FILENO, target)` and remembers the
/// previous foreground pgid; `drop` restores it.  Returns `None` when the
/// shell isn't interactive, stdin isn't a tty, or the syscall fails — in
/// those cases there's nothing to restore.
#[cfg(unix)]
pub struct ForegroundGuard {
    saved_pgid: libc::pid_t,
}

#[cfg(unix)]
impl ForegroundGuard {
    /// Hand the controlling tty to `target`, recording the prior pgid for
    /// the eventual restore.  Returns `None` when no handoff is appropriate.
    pub fn try_acquire(target: libc::pid_t, shell: &crate::types::Shell) -> Option<Self> {
        if !shell.io.interactive || !shell.io.terminal.stdin_tty || target == 0 {
            return None;
        }
        let saved = unsafe { libc::getpgrp() };
        let rc = unsafe { libc::tcsetpgrp(libc::STDIN_FILENO, target) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            crate::dbg_trace!("fg", "acquire: tcsetpgrp({target}) failed: {err}");
            return None;
        }
        Some(Self { saved_pgid: saved })
    }
}

#[cfg(unix)]
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

/// Non-unix stub so callers can use `ForegroundGuard` unconditionally.
#[cfg(not(unix))]
pub struct ForegroundGuard;

#[cfg(not(unix))]
impl ForegroundGuard {
    pub fn try_acquire(_target: i32, _shell: &crate::types::Shell) -> Option<Self> {
        None
    }
}

/// Count of currently occupied relay slots (for testing).
#[cfg(all(unix, test))]
fn active_relay_slots() -> usize {
    RELAY_PGIDS
        .iter()
        .filter(|s| s.load(Ordering::Acquire) != 0)
        .count()
}

#[cfg(all(unix, test))]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier, Mutex};

    // All relay tests share a process-wide lock because RELAY_PGIDS is a
    // global.  Tests run concurrently in the same process by default; without
    // this they would steal each other's slots.
    static RELAY_TEST_LOCK: Mutex<()> = Mutex::new(());

    // ── Slot allocation ──────────────────────────────────────────────────────

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

    // ── Concurrency stress ───────────────────────────────────────────────────

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
        // Fill all slots, then hammer install from many threads simultaneously.
        // Every extra install must return None, never panic or corrupt state.
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

    // ── Signal forwarding ────────────────────────────────────────────────────

    #[test]
    fn relay_delivers_sigint_to_child_group() {
        // Spawn `sleep 1000` as a child in its own process group (via
        // pre_exec).  Claim a relay slot for the child's pgid and call
        // sigint_relay directly — equivalent to the shell receiving SIGINT
        // while the pipeline runs.  Verify the child was killed by the signal.
        //
        // We use Command + pre_exec rather than fork() to avoid the hazards of
        // forking inside a multithreaded test binary.
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
