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

/// Check if a signal has been received. Returns Ok(()) normally,
/// or Err(EvalSignal::Error) to begin unwinding.
pub fn check(shell: &crate::types::Shell) -> Result<(), crate::types::EvalSignal> {
    let count = SIGNAL_COUNT.load(Ordering::Relaxed);
    if count >= 1 {
        return Err(crate::types::EvalSignal::Error(
            crate::types::Error::new("interrupted", 130).at(shell.location.line, shell.location.col),
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
