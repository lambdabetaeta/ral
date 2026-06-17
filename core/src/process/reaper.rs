//! Deadline reaper — one daemon for every armed lifetime ceiling.
//!
//! A worker with a wall-clock or lifetime ceiling needs *something* to
//! fire a [`CancelCause::Deadline`] cancellation when the ceiling
//! elapses.  The naïve shape is a watchdog thread per worker that sleeps
//! the ceiling and cancels; that does not scale to one-per-`spawn`, where
//! a busy session can hold thousands of in-flight workers.
//!
//! Instead this module keeps **deadlines as data**: a single, lazily
//! started, process-global daemon owns a min-ordered heap of
//! `(deadline, scope)` entries and fires each [`cancel`](CancelScope::cancel)
//! at its `Instant`.  Callers register a ceiling with [`arm_lifetime`]
//! rather than spawning their own timer; the daemon sleeps until the
//! earliest deadline, cancels it, and re-evaluates.
//!
//! [`arm_lifetime`] returns a [`Deadline`] guard, which selects between two
//! modes.  Held and then dropped, the guard *disarms* its entry: the daemon
//! skips a disarmed entry when it comes due, so a turn that finishes before
//! its ceiling is never reaped by a late pop.  Consumed by
//! [`Deadline::keep`], the entry stays armed forever and fires at its
//! ceiling regardless — the fire-and-forget mode the detached-worker
//! death-clock needs, since its worker outlives the `spawn` call that armed
//! it.  A kept entry accepts the harmless late cancel of an already-settled
//! scope ([`CancelScope::cancel`] is a monotone `fetch_max` observed by
//! nobody once the worker is gone) and the bounded retention of its
//! [`CancelScope`] `Arc` until the ceiling elapses — the price of not
//! running a thread per worker.

use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{CancelCause, CancelScope};

/// A scheduled cancellation: cancel `scope` once `when` has passed,
/// unless `armed` has been cleared by a dropped [`Deadline`] guard.
///
/// The ordering is **inverted** so the earliest deadline is the
/// *greatest* entry — [`BinaryHeap`] is a max-heap, so its peek/pop must
/// surface the soonest ceiling.  Comparison is on `when` alone;
/// [`CancelScope`] carries no `Ord`/`Eq`, the `armed` flag is irrelevant
/// to ordering, and two entries sharing an `Instant` are interchangeable
/// for the daemon's purposes.
struct Scheduled {
    when: Instant,
    scope: CancelScope,
    armed: Arc<AtomicBool>,
}

impl PartialEq for Scheduled {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when
    }
}

impl Eq for Scheduled {}

impl Ord for Scheduled {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // Reverse: an earlier `when` compares as greater, so the heap's
        // root is the soonest deadline.
        other.when.cmp(&self.when)
    }
}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The shared schedule the daemon drains and [`arm_lifetime`] feeds.  The
/// [`Condvar`] wakes the daemon when a freshly armed entry may have a
/// sooner deadline than the one it is currently sleeping toward.
struct Reaper {
    heap: Mutex<BinaryHeap<Scheduled>>,
    wake: Condvar,
}

/// The process-global daemon, started on the first [`arm_lifetime`] call.
/// Held as an `Arc` so the daemon thread owns a clone of the same shared
/// schedule rather than refetching it from this slot.
static REAPER: OnceLock<Arc<Reaper>> = OnceLock::new();

/// Schedule `scope` to be cancelled with [`CancelCause::Deadline`] once
/// `after` has elapsed from now, returning the [`Deadline`] guard that
/// governs the entry.
///
/// The first call lazily starts the single daemon thread; every later
/// call reuses it.  The absolute deadline `Instant::now() + after` is
/// pushed onto the shared heap and the daemon is woken so it can fold the
/// new entry into its next sleep.
///
/// Drop the returned guard to disarm the entry — work that completes
/// before its ceiling is then never cancelled.  Call [`Deadline::keep`]
/// to opt into the fire-and-forget mode, where the entry fires at its
/// ceiling no matter where the guard goes.  The [`Deadline`] return is
/// itself `#[must_use]`, so dropping it on the floor disarms and warns.
pub fn arm_lifetime(scope: CancelScope, after: Duration) -> Deadline {
    let reaper = REAPER.get_or_init(start_daemon);
    let when = Instant::now() + after;
    let armed = Arc::new(AtomicBool::new(true));
    reaper
        .heap
        .lock()
        .expect("reaper heap poisoned")
        .push(Scheduled {
            when,
            scope,
            armed: armed.clone(),
        });
    // A newly armed entry may be sooner than what the daemon is sleeping
    // toward; wake it to re-peek the heap top.
    reaper.wake.notify_one();
    Deadline { armed }
}

/// A handle to an armed deadline.  Dropping it *disarms* the deadline,
/// so work that completes before its ceiling is never cancelled —
/// the reaper skips a disarmed entry when it comes due.  Call
/// [`Deadline::keep`] for a fire-and-forget ceiling that must fire
/// regardless of where this handle goes (the detached-worker
/// death-clock, whose worker outlives the `spawn` call).
#[must_use]
pub struct Deadline {
    armed: Arc<AtomicBool>,
}

impl Deadline {
    /// Keep the deadline armed forever: it fires at its ceiling no
    /// matter what.  Consumes the handle so its `Drop` cannot disarm.
    pub fn keep(self) {
        std::mem::forget(self);
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        self.armed.store(false, Ordering::Release);
    }
}

/// Construct the shared [`Reaper`] and spawn its daemon thread.  Called
/// exactly once, through the [`OnceLock`] in [`arm_lifetime`].  The
/// daemon receives its own `Arc` to the schedule, so it never has to
/// reach back through the not-yet-populated [`OnceLock`].
fn start_daemon() -> Arc<Reaper> {
    let reaper = Arc::new(Reaper {
        heap: Mutex::new(BinaryHeap::new()),
        wake: Condvar::new(),
    });
    let worker = Arc::clone(&reaper);
    std::thread::Builder::new()
        .name("ral-reaper".into())
        .spawn(move || daemon_loop(&worker))
        .expect("spawn reaper daemon");
    reaper
}

/// The daemon's body: fire every deadline as it arrives, sleeping in
/// between.
///
/// With an empty heap there is nothing to wait *for*, so the daemon
/// blocks on the condvar with no timeout until [`arm_lifetime`] notifies
/// it.  With a non-empty heap it peeks the soonest deadline: if it is
/// already due it pops and cancels; otherwise it sleeps until then —
/// `wait_timeout`, so a fresh, sooner arm-notification can cut the sleep
/// short.  Every wake re-checks the heap top, so a spurious wakeup costs
/// only a re-peek.
///
/// The thread runs for the process lifetime; there is no shutdown path.
fn daemon_loop(reaper: &Reaper) {
    let mut heap = reaper.heap.lock().expect("reaper heap poisoned");
    loop {
        match heap.peek() {
            None => {
                // Nothing scheduled: wait until an arm wakes us.
                heap = reaper.wake.wait(heap).expect("reaper heap poisoned");
            }
            Some(next) => {
                let now = Instant::now();
                if next.when <= now {
                    // Due: pop and fire, unless a dropped guard disarmed
                    // it.  The pop cannot fail — we just peeked a present
                    // entry under the held lock.
                    let due = heap.pop().expect("peeked entry vanished");
                    if due.armed.load(Ordering::Acquire) {
                        due.scope.cancel(CancelCause::Deadline);
                    }
                } else {
                    // Not yet: sleep until the deadline, woken early by a
                    // sooner arm.  `saturating_duration_since` floors at
                    // zero if `now` overtook `when` between the peek and
                    // here.
                    let remaining = next.when.saturating_duration_since(now);
                    let (g, _) = reaper
                        .wake
                        .wait_timeout(heap, remaining)
                        .expect("reaper heap poisoned");
                    heap = g;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An armed scope is cancelled once its ceiling elapses, with the
    /// [`Deadline`](CancelCause::Deadline) cause the reaper applies.  The
    /// guard is held across the poll so the entry stays armed.
    #[test]
    fn arm_fires_after_the_ceiling() {
        let scope = CancelScope::default();
        let _d = arm_lifetime(scope.clone(), Duration::from_millis(20));

        let mut fired = false;
        for _ in 0..200 {
            if scope.is_cancelled() {
                fired = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(fired, "an armed scope must be cancelled after its ceiling");
        assert_eq!(
            scope.cause(),
            Some(CancelCause::Deadline),
            "the reaper fires the Deadline cause"
        );
    }

    /// A ceiling far in the future does not fire early: nothing cancels
    /// the scope within a window well short of the deadline.
    #[test]
    fn arm_does_not_fire_before_the_ceiling() {
        let scope = CancelScope::default();
        let _d = arm_lifetime(scope.clone(), Duration::from_secs(3600));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !scope.is_cancelled(),
            "a distant ceiling must not fire early"
        );
    }

    /// More than one entry in the heap: two scopes armed with short
    /// ceilings must both fire, exercising the daemon's pop-and-re-peek.
    #[test]
    fn multiple_armed_scopes_each_fire() {
        let one = CancelScope::default();
        let two = CancelScope::default();
        let _d1 = arm_lifetime(one.clone(), Duration::from_millis(20));
        let _d2 = arm_lifetime(two.clone(), Duration::from_millis(20));

        let mut both = false;
        for _ in 0..200 {
            if one.is_cancelled() && two.is_cancelled() {
                both = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(both, "every armed scope in the heap must fire");
    }

    /// Dropping the guard disarms the entry: when its ceiling comes due
    /// the daemon pops it and skips the cancel, so the scope is spared.
    #[test]
    fn disarmed_deadline_does_not_fire() {
        let scope = CancelScope::default();
        drop(arm_lifetime(scope.clone(), Duration::from_millis(20)));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !scope.is_cancelled(),
            "a disarmed deadline must not fire after the guard is dropped"
        );
    }

    /// `keep()` opts the entry into fire-and-forget: the deadline fires at
    /// its ceiling even though the guard went out of scope at the call.
    #[test]
    fn kept_deadline_fires_after_handle_dropped() {
        let scope = CancelScope::default();
        arm_lifetime(scope.clone(), Duration::from_millis(20)).keep();

        let mut fired = false;
        for _ in 0..200 {
            if scope.is_cancelled() {
                fired = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(fired, "a kept deadline must fire though its handle is gone");
        assert_eq!(
            scope.cause(),
            Some(CancelCause::Deadline),
            "the reaper fires the Deadline cause"
        );
    }
}
