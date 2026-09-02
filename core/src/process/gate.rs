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
pub struct StageStop(Mutex<Option<Signal>>);

impl StageStop {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Mutex::new(None)))
    }

    pub fn get(&self) -> Option<Signal> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set(&self, sig: Option<Signal>) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = sig;
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
    /// A top-level foreground external the REPL's job table owns: surface
    /// `Escape::Stopped`.
    Escape,
    /// An external inside a stage thread: record the signal in `park.stop`,
    /// wait on `park.gate`, then continue waiting on the same child (it is
    /// alive and will be `SIGCONT`ed).
    Park(StagePark),
}

impl StopPolicy {
    /// The one rule for every external ral waits on.  Inside a stage thread it
    /// parks on the pipeline's gate; outside one, `escapes` (a foreground the
    /// job table can resume) surfaces the stop, and anything else kills and
    /// reaps.
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
