//! The Ctrl-Z park: one gate per pipeline, shared by its stage threads, and
//! the policy that says whether a stopped external parks, escapes, or is
//! killed and reaped.
//!
//! [`StageGate::wait`] is the one place a parked stage blocks, so its two
//! invariants — cancel checked before pause, `paused` read under the
//! `Condvar`'s own mutex — are what a nested pipeline's correctness rests on.

use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use super::cancel::{CancelCause, CancelScope};
use super::outcome::Signal;

/// One per pipeline, shared by its stage threads.  `paused` is the Ctrl-Z
/// park; waiters wake on resume or on their own scope's cancel.
pub struct StageGate {
    paused: Mutex<bool>,
    condvar: Condvar,
}

impl StageGate {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            paused: Mutex::new(false),
            condvar: Condvar::new(),
        })
    }

    fn set(&self, paused: bool) {
        *self.paused.lock().unwrap_or_else(PoisonError::into_inner) = paused;
        self.condvar.notify_all();
    }

    pub fn pause(&self) {
        self.set(true);
    }

    pub fn resume(&self) {
        self.set(false);
    }

    pub fn is_paused(&self) -> bool {
        *self.paused.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Block while paused.  Polls `scope.cause()` every 20 ms so a cancel
    /// always wins; `Err` carries the cause.
    ///
    /// # Errors
    /// Returns `Err(cause)` when `scope` is cancelled while parked.
    pub fn wait(&self, scope: &CancelScope) -> Result<(), CancelCause> {
        let mut guard = self.paused.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(cause) = scope.cause() {
                return Err(cause);
            }
            if !*guard {
                return Ok(());
            }
            let (next, _timeout) = self
                .condvar
                .wait_timeout(guard, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner);
            guard = next;
        }
    }
}

/// A stage thread's outstanding stop, as the collector reads it.
///
/// The thread (or an external it waits on) writes the signal; the collector
/// holding the handle clears it once acknowledged — the owner on resume, a
/// joining collector at once, having reported the stop to its own stage.
/// Whether the stage has *ended* is not here: that is `JoinHandle::is_finished`,
/// which an unwinding panic cannot skip.
///
/// The condvar turns this into a producer the pipeline collector's dedicated
/// interior-stop watcher thread can block on — [`Self::wait_for_change`] —
/// rather than a cell the collector must still poll each pass.
struct StopState {
    signal: Option<Signal>,
    /// Set once by [`StageStop::close`], the stage thread's own last act
    /// before it sends its `Settled` event: the watcher's sole way to learn
    /// there will be no further edge to wait for.
    closed: bool,
}

pub struct StageStop(Mutex<StopState>, Condvar);

impl StageStop {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(
            Mutex::new(StopState {
                signal: None,
                closed: false,
            }),
            Condvar::new(),
        ))
    }

    pub fn get(&self) -> Option<Signal> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).signal
    }

    pub fn set(&self, sig: Option<Signal>) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).signal = sig;
        self.1.notify_all();
    }

    /// The stage thread's own last act, before it sends its `Settled` event:
    /// wake [`Self::wait_for_change`] a final time so its watcher thread
    /// leaves rather than blocking on an edge that will never come.
    pub fn close(&self) {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).closed = true;
        self.1.notify_all();
    }

    /// Block until the signal differs from `last`, or [`Self::close`] has run
    /// — `None` for either a resume back to `None` or a close, which the
    /// caller tells apart by nothing needing to: a watcher that wakes to
    /// `last` unchanged and closed simply stops.
    pub fn wait_for_change(&self, last: Option<Signal>) -> Option<Signal> {
        let mut guard = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if guard.closed || guard.signal != last {
                return guard.signal;
            }
            guard = self
                .1
                .wait(guard)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    /// Whether [`Self::close`] has run — the interior-stop watcher's own exit
    /// check, since a resume (`signal` returning to `None`) and a close both
    /// wake [`Self::wait_for_change`] with the same `None` result.
    pub fn is_closed(&self) -> bool {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).closed
    }
}

/// A stage thread's share of its pipeline's park.
///
/// The gate every stage of the pipeline waits on, and this stage's own
/// outstanding stop, read by the collector that holds its handle.  Travels on
/// the `Mooring` beside the cancel scope, so a nested pipeline's stages inherit
/// the gate and a detached worker does not.
#[derive(Clone)]
pub struct StagePark {
    pub gate: Arc<StageGate>,
    pub stop: Arc<StageStop>,
}

/// What a stopped external does to the ral that waits on it.
#[derive(Clone)]
pub enum StopPolicy {
    /// Batch mode: kill and reap on the spot.
    KillAndReap,
    /// A top-level foreground external: answered with `SIGCONT`, exactly as
    /// `Park` is.
    Escape,
    /// An external inside a stage thread: record the signal in `park.stop`,
    /// wait on `park.gate`, then continue waiting on the same child (it is
    /// alive and will be `SIGCONT`ed).
    Park(StagePark),
}

impl StopPolicy {
    /// The one rule for every external ral waits on.  Inside a stage thread it
    /// parks on the pipeline's gate; outside one, `escapes` (a foreground)
    /// answers with `SIGCONT`, and anything else kills and reaps.
    pub fn for_external(mooring: &crate::types::Mooring, escapes: bool) -> Self {
        match &mooring.park {
            Some(p) => Self::Park(p.clone()),
            None if escapes => Self::Escape,
            None => Self::KillAndReap,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wait_returns_ok_on_resume() {
        let gate = StageGate::new();
        gate.pause();
        let scope = CancelScope::root();
        let g = Arc::clone(&gate);
        let resumer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            g.resume();
        });
        assert!(gate.wait(&scope).is_ok(), "resume must unblock the wait");
        resumer.join().expect("resumer thread");
    }

    #[test]
    fn wait_returns_err_on_cancel_while_paused() {
        let gate = StageGate::new();
        gate.pause();
        let scope = CancelScope::root();
        let s = scope.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            s.cancel(CancelCause::Interrupt);
        });
        assert_eq!(
            gate.wait(&scope),
            Err(CancelCause::Interrupt),
            "a scope cancelled while parked must end the wait with its cause"
        );
        canceller.join().expect("canceller thread");
    }

    #[test]
    fn for_external_yields_park_under_a_mooring_with_one() {
        let mut mooring = crate::types::Mooring::adrift();
        let park = StagePark {
            gate: StageGate::new(),
            stop: StageStop::new(),
        };
        mooring.park = Some(park);
        assert!(
            matches!(
                StopPolicy::for_external(&mooring, false),
                StopPolicy::Park(_)
            ),
            "a mooring carrying a park always parks, whatever `escapes` says"
        );
    }

    #[test]
    fn for_external_yields_escape_or_kill_and_reap_without_one() {
        let mooring = crate::types::Mooring::adrift();
        assert!(matches!(
            StopPolicy::for_external(&mooring, true),
            StopPolicy::Escape
        ));
        assert!(matches!(
            StopPolicy::for_external(&mooring, false),
            StopPolicy::KillAndReap
        ));
    }
}
