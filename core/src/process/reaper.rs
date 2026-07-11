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
//! The daemon fires one [`Action`] per entry, in one of two shapes:
//! `Cancel(scope)` cancels a [`CancelScope`] — the death-clock and the
//! foreground wall — and `Run(callback)` runs an opaque host closure.  A
//! scheduled wakeup is the second shape — the host (exarch) hands the reaper a
//! `Run` closure that posts a prompt and wakes its idle loop, while the
//! reaper stays ignorant of prompts, cron, and sessions.  Recurrence is
//! *not* a reaper concept: entries remain one-shot, and a recurring
//! producer re-arms the next occurrence from inside its own `Run`.
//!
//! [`arm_lifetime`] / [`arm_callback`] return a [`Deadline`] guard, which
//! selects between two modes.  Held and then dropped, the guard *disarms*
//! its entry: the daemon skips a disarmed entry when it comes due, so a
//! turn that finishes before its ceiling is never reaped by a late pop.
//! Consumed by [`Deadline::keep`], the entry stays armed forever and fires
//! at its ceiling regardless — the fire-and-forget mode the detached-worker
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

/// What a due entry does when it fires.
///
/// `Cancel` is the original death-clock / foreground-wall action: cancel a
/// [`CancelScope`] with [`CancelCause::Deadline`].  `Run` is the
/// generalisation: invoke an opaque host closure once.  The reaper never
/// inspects a `Run` closure — it is the host's effect (post a wakeup, wake
/// a loop), kept opaque so no host representation leaks into core.
enum Action {
    Cancel(CancelScope),
    Run(Box<dyn FnOnce() + Send>),
}

impl Action {
    /// Run the action.  Consumes `self`: a `Run` closure fires exactly once.
    fn fire(self) {
        match self {
            Self::Cancel(scope) => scope.cancel(CancelCause::Deadline),
            Self::Run(run) => run(),
        }
    }
}

/// A scheduled action: fire `action` once `when` has passed, unless
/// `armed` has been cleared by a dropped [`Deadline`] guard.
///
/// The ordering is **inverted** so the earliest deadline is the
/// *greatest* entry — [`BinaryHeap`] is a max-heap, so its peek/pop must
/// surface the soonest ceiling.  Comparison is on `when` alone;
/// [`Action`] carries no `Ord`/`Eq`, the `armed` flag is irrelevant
/// to ordering, and two entries sharing an `Instant` are interchangeable
/// for the daemon's purposes.
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
    arm(Action::Cancel(scope), after)
}

/// Schedule `run` to be invoked once `after` has elapsed from now,
/// returning the [`Deadline`] guard that governs the entry.
///
/// This is the generalised twin of [`arm_lifetime`]: where that cancels a
/// scope, this runs an opaque host closure.  A scheduled wakeup arms one of
/// these with a closure that posts a prompt to a session inbox and wakes
/// its idle loop; a recurring producer re-arms the next occurrence from
/// inside the closure, since the reaper holds no recurrence of its own.
///
/// The closure runs on the reaper daemon thread, *outside* the heap lock,
/// so it may itself call [`arm_callback`] (or [`arm_lifetime`]) to schedule
/// the next occurrence without deadlocking.  It must be cheap and
/// non-blocking — a long-running closure stalls every later deadline.  The
/// returned [`Deadline`] is `#[must_use]`; drop it to disarm or
/// [`Deadline::keep`] it for fire-and-forget.
pub fn arm_callback(after: Duration, run: impl FnOnce() + Send + 'static) -> Deadline {
    arm(Action::Run(Box::new(run)), after)
}

/// Push one [`Action`] onto the shared heap with an absolute deadline
/// `after` from now, lazily starting the daemon and waking it so it can
/// fold the new entry into its next sleep.  The shared body of
/// [`arm_lifetime`] and [`arm_callback`].
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
    // A newly armed entry may be sooner than what the daemon is sleeping
    // toward; wake it to re-peek the heap top.
    reaper.wake.notify_one();
    Deadline {
        armed,
        keep: false,
    }
}

/// A handle to an armed deadline.
///
/// Dropping it *disarms* the deadline,
/// so work that completes before its ceiling is never cancelled —
/// the reaper skips a disarmed entry when it comes due.  Call
/// [`Deadline::keep`] for a fire-and-forget ceiling that must fire
/// regardless of where this handle goes (the detached-worker
/// death-clock, whose worker outlives the `spawn` call).
#[must_use]
pub struct Deadline {
    armed: Arc<AtomicBool>,
    /// Set by [`Self::keep`]: `Drop` leaves `armed` alone instead of
    /// disarming it.  "Keep it armed" is a state this handle carries, so
    /// `Drop` still runs its ordinary course.
    keep: bool,
}

impl Deadline {
    /// Keep the deadline armed forever: it fires at its ceiling no
    /// matter what.  Consumes the handle; `Drop` still runs, but sees
    /// `keep = true` and leaves `armed` alone.
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
                    // Release the lock before firing.  A `Run` action may
                    // call back into the reaper (a recurring wakeup re-arms
                    // its next occurrence), which locks the same heap;
                    // firing under the lock would deadlock.  `Cancel` is a
                    // cheap lock-free `fetch_max`, but unlocking first keeps
                    // both actions on the same, simple rule: never run an
                    // entry's effect while holding the schedule.
                    drop(heap);
                    if due.armed.load(Ordering::Acquire) {
                        due.action.fire();
                    }
                    heap = reaper.heap.lock().expect("reaper heap poisoned");
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
        let _d = arm_lifetime(scope.clone(), Duration::from_hours(1));
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

    /// A `Run` action fires its closure once its ceiling elapses, the
    /// generalisation a scheduled wakeup rides.  The guard is kept so the
    /// entry stays armed past the call.
    #[test]
    fn arm_callback_runs_after_the_ceiling() {
        let fired = Arc::new(AtomicBool::new(false));
        let flag = fired.clone();
        arm_callback(Duration::from_millis(20), move || {
            flag.store(true, Ordering::Release);
        })
        .keep();

        let mut ran = false;
        for _ in 0..200 {
            if fired.load(Ordering::Acquire) {
                ran = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(
            ran,
            "a Run deadline must invoke its closure after the ceiling"
        );
    }

    /// Dropping a `Run` guard disarms it just like a `Cancel` guard: the
    /// closure never runs.
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

    /// A `Run` closure may re-arm the next occurrence from inside itself —
    /// the reaper fires actions outside the heap lock precisely so a
    /// recurring wakeup can reschedule without deadlocking.  Here a closure
    /// re-arms twice, so the counter reaches three total fires.
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
