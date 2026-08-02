//! The `detach` budget: the one piece of session state meant to outlive the
//! session.
//!
//! Counting is monotone for births: a detached process is double-forked and
//! reparented to pid 1, so its death is unobservable here and a release
//! would be a number nobody can compute — unlike the worker cap, whose seat
//! a settled worker frees. A reservation is different. Between admission and
//! the birth, the launch can still fail in this process, where the failure
//! is exactly observable, so an uncommitted [`Reservation`] releases its
//! slot on drop; only one made permanent by
//! [`commit`](Reservation::commit) is forever.
//!
//! Core defaults no budget: a host calls
//! [`Shell::arm_detach`](super::Shell::arm_detach) in the act that installs
//! the `detach` builtin. [`Shell::spawn_thread`](super::Shell::spawn_thread)
//! `Arc`-shares the policy into a worker's shell, so a `detach` under
//! `spawn { }` spends the owning session's budget, while
//! [`LocalState`](super::LocalState)'s [`Drop`] cancels that session's
//! workers but never this.

use std::sync::atomic::{AtomicU64, Ordering};

/// A session's authority to birth processes that outlive it.
pub struct DetachPolicy {
    /// A whole-life total, not a concurrency limit.
    pub budget: u64,
    pub(super) births: AtomicU64,
}

impl DetachPolicy {
    /// Reserve one birth.
    ///
    /// The counter this consults includes reservations still in flight as
    /// well as committed births, so two launches racing for the last slot
    /// cannot both be admitted.
    ///
    /// # Errors
    ///
    /// The budget, once spent, so a caller can name the number to the user.
    pub fn admit(&self) -> Result<Reservation<'_>, u64> {
        self.births
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < self.budget).then_some(n + 1)
            })
            .map(|_| Reservation { policy: self })
            .map_err(|_| self.budget)
    }
}

/// A birth admitted but not yet certain.
///
/// While it lives it occupies a slot, so a racer contending for the last one
/// sees it and is refused — the slot is spent from the moment of admission,
/// not from the moment of birth.
///
/// Dropping it releases the slot: this is every failed or abandoned
/// launch's path back to the budget. [`commit`](Self::commit) is the other
/// path, taken once the process actually exists, and it is not a bookkeeping
/// step but the commitment itself — consuming the guard without letting its
/// destructor run *is* the promise made permanent, since a birth has no
/// release and so the destructor that performs the release must never run
/// for one.
#[must_use = "an unheld reservation releases its slot at once; hold it across the launch and commit on success"]
pub struct Reservation<'a> {
    policy: &'a DetachPolicy,
}

impl Reservation<'_> {
    /// Make the birth permanent. The reservation is consumed without running
    /// its `Drop`, so the slot it holds is never released — exactly the
    /// monotone counting a real birth demands.
    pub fn commit(self) {
        std::mem::forget(self);
    }
}

impl Drop for Reservation<'_> {
    fn drop(&mut self) {
        // An uncommitted reservation was a launch that failed or was
        // abandoned before the process existed, so its count back to the
        // budget is exactly computable: one.
        self.policy.births.fetch_sub(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::LocalState;
    use std::sync::Arc;

    #[test]
    fn admission_exhausts_at_the_budget_and_stays_exhausted() {
        let policy = DetachPolicy {
            budget: 2,
            births: AtomicU64::new(0),
        };
        policy.admit().expect("first admission").commit();
        policy.admit().expect("second admission").commit();
        assert_eq!(policy.admit().map(drop), Err(2));
        assert_eq!(policy.admit().map(drop), Err(2));
        assert_eq!(policy.births.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn tearing_down_a_session_leaves_the_policy_untouched() {
        let policy = Arc::new(DetachPolicy {
            budget: 2,
            births: AtomicU64::new(0),
        });
        policy.admit().expect("first admission").commit();
        let mut local = LocalState::default();
        local.detach = Some(policy.clone());
        drop(local);
        policy.admit().expect("second admission").commit();
        assert_eq!(policy.admit().map(drop), Err(2));
    }

    #[test]
    fn a_dropped_reservation_gives_the_slot_back() {
        let policy = DetachPolicy {
            budget: 1,
            births: AtomicU64::new(0),
        };
        drop(policy.admit().expect("the only slot is free"));
        policy
            .admit()
            .expect("the dropped reservation freed the slot")
            .commit();
        assert_eq!(policy.admit().map(drop), Err(1));
        assert_eq!(policy.births.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn a_committed_reservation_stays_spent() {
        let policy = DetachPolicy {
            budget: 1,
            births: AtomicU64::new(0),
        };
        policy.admit().expect("the only slot is free").commit();
        assert_eq!(policy.admit().map(drop), Err(1));
        assert_eq!(policy.births.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn racers_for_the_last_slot_admit_exactly_the_budget() {
        let policy = DetachPolicy {
            budget: 3,
            births: AtomicU64::new(0),
        };
        let admitted: Vec<Reservation<'_>> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..16)
                .map(|_| scope.spawn(|| policy.admit().ok()))
                .collect();
            handles
                .into_iter()
                .filter_map(|h| h.join().expect("racer thread panicked"))
                .collect()
        });
        assert_eq!(admitted.len(), 3, "exactly the budget must be admitted");
        drop(admitted);
        assert_eq!(policy.births.load(Ordering::Relaxed), 0);
        policy.admit().expect("every slot was released").commit();
    }
}
