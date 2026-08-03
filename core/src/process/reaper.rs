//! Deadline reaper — one process-global daemon for every armed ceiling.
//!
//! Deadlines are data, not threads: a heap of `(when, action)` entries drained
//! by a single lazily started daemon, so a session holding thousands of
//! in-flight ceilings costs one thread rather than one watchdog apiece.
//!
//! An entry either cancels a [`CancelScope`] with [`CancelCause::Deadline`] or
//! runs an opaque host closure — opaque so that no host notion of prompts,
//! cron, or sessions leaks into core.  Entries are one-shot; recurrence is a
//! producer re-arming the next occurrence from inside its own closure.
//!
//! Dropping the returned [`Deadline`] disarms the entry, so work that finishes
//! before its ceiling is never reaped by a late pop.  [`Deadline::keep`] leaves
//! it armed for good, at the price of retaining whatever it captured until the
//! ceiling elapses; a late cancel of a settled scope is harmless, since
//! [`CancelScope::cancel`] is a monotone `fetch_max` nobody is left to observe.

use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{CancelCause, CancelScope};

enum Action {
    Cancel(CancelScope),
    Run(Box<dyn FnOnce() + Send>),
}

impl Action {
    fn fire(self) {
        match self {
            Self::Cancel(scope) => scope.cancel(CancelCause::Deadline),
            Self::Run(run) => run(),
        }
    }
}

/// A scheduled action, skipped when it comes due if a dropped [`Deadline`]
/// has cleared `armed`.
///
/// [`BinaryHeap`] is a max-heap, so the ordering below is inverted: the
/// earliest `when` compares greatest and sits at the root.  Entries sharing an
/// `Instant` are interchangeable, so `when` alone decides.
struct Scheduled {
    when: Instant,
    action: Action,
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
        other.when.cmp(&self.when)
    }
}

impl PartialOrd for Scheduled {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// The shared schedule: the daemon drains it, `arm` feeds it, and the
/// [`Condvar`] cuts the daemon's sleep short when a fresh entry may be sooner
/// than the deadline it is sleeping toward.
struct Reaper {
    heap: Mutex<BinaryHeap<Scheduled>>,
    wake: Condvar,
}

/// The one schedule, built on the first arm.  The daemon thread carries its own
/// `Arc` clone, so it never reads back through this still-unfilled slot.
static REAPER: OnceLock<Arc<Reaper>> = OnceLock::new();

/// Cancel `scope` with [`CancelCause::Deadline`] once `after` has elapsed from
/// now.  A run's wall clock arms one of these over its foreground scope.
pub fn arm_lifetime(scope: CancelScope, after: Duration) -> Deadline {
    arm(Action::Cancel(scope), after)
}

/// Invoke `run` once `after` has elapsed from now.
///
/// The closure runs on the daemon thread with the heap lock released, so it may
/// re-arm itself — how a worker's lease chain and a scheduled wakeup get their
/// recurrence.  It must not block: a slow closure stalls every later entry.
pub fn arm_callback(after: Duration, run: impl FnOnce() + Send + 'static) -> Deadline {
    arm(Action::Run(Box::new(run)), after)
}

/// The shared body of [`arm_lifetime`] and [`arm_callback`]; the first call
/// starts the daemon.
fn arm(action: Action, after: Duration) -> Deadline {
    let reaper = REAPER.get_or_init(start_daemon);
    let when = Instant::now() + after;
    let armed = Arc::new(AtomicBool::new(true));
    reaper
        .heap
        .lock()
        .expect("reaper heap poisoned")
        .push(Scheduled {
            when,
            action,
            armed: armed.clone(),
        });
    // This entry may beat the deadline the daemon is sleeping toward.
    reaper.wake.notify_one();
    Deadline { armed, keep: false }
}

/// A handle to an armed deadline.
///
/// Dropping it disarms the entry, so work that completes before its ceiling
/// is never cancelled; call [`Deadline::keep`] when the effect must outlive
/// the call that armed it.
#[must_use]
pub struct Deadline {
    armed: Arc<AtomicBool>,
    keep: bool,
}

impl Deadline {
    /// Leave the entry armed for good, so it fires at its ceiling whatever
    /// becomes of this handle.
    pub fn keep(mut self) {
        self.keep = true;
    }
}

impl Drop for Deadline {
    fn drop(&mut self) {
        if !self.keep {
            self.armed.store(false, Ordering::Release);
        }
    }
}

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

/// Fire every deadline as it arrives.  An empty heap blocks on the condvar
/// untimed until an arm notifies; otherwise the daemon sleeps toward the
/// soonest entry under `wait_timeout`, so a sooner arm cuts the sleep short.
/// Every wake re-peeks, which is what makes a spurious one free.  Runs for the
/// process lifetime — there is no shutdown path.
fn daemon_loop(reaper: &Reaper) {
    let mut heap = reaper.heap.lock().expect("reaper heap poisoned");
    loop {
        match heap.peek() {
            None => {
                heap = reaper.wake.wait(heap).expect("reaper heap poisoned");
            }
            Some(next) => {
                let now = Instant::now();
                if next.when <= now {
                    let due = heap.pop().expect("peeked entry vanished");
                    // Never run an entry's effect while holding the schedule:
                    // a `Run` closure may re-arm, which locks this same heap.
                    drop(heap);
                    if due.armed.load(Ordering::Acquire) {
                        due.action.fire();
                    }
                    heap = reaper.heap.lock().expect("reaper heap poisoned");
                } else {
                    // Saturating: `now` may have overtaken `when` since the peek.
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

    #[test]
    fn arm_does_not_fire_before_the_ceiling() {
        let scope = CancelScope::default();
        let _d = arm_lifetime(scope.clone(), Duration::from_hours(1));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !scope.is_cancelled(),
            "a distant ceiling must not fire early"
        );
    }

    /// Exercises the daemon's pop-and-re-peek, not just its first entry.
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

    #[test]
    fn disarmed_callback_does_not_run() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        drop(arm_callback(Duration::from_millis(20), move || {
            flag.store(true, Ordering::Release);
        }));
        std::thread::sleep(Duration::from_millis(80));
        assert!(
            !fired.load(Ordering::Acquire),
            "a disarmed Run deadline must not invoke its closure"
        );
    }

    /// The recurrence a lease chain rides: were actions fired under the heap
    /// lock, the re-arm inside the closure would deadlock instead of counting
    /// to three.
    #[test]
    fn callback_can_rearm_itself() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));

        fn rearm(count: &Arc<std::sync::atomic::AtomicUsize>) {
            let n = count.fetch_add(1, Ordering::AcqRel) + 1;
            if n < 3 {
                let next = count.clone();
                arm_callback(Duration::from_millis(15), move || rearm(&next)).keep();
            }
        }

        let first = count.clone();
        arm_callback(Duration::from_millis(15), move || rearm(&first)).keep();

        let mut reached = false;
        for _ in 0..200 {
            if count.load(Ordering::Acquire) >= 3 {
                reached = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            reached,
            "a self-re-arming Run closure must fire each scheduled occurrence"
        );
    }
}
