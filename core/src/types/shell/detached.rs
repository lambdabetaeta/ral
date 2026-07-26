//! The `detach` budget: the one piece of session state that is meant to
//! outlive the session.
//!
//! A detached process is born by double-fork and reparented to pid 1, so
//! nothing in this process ever observes it again
//! (`decisions/260725_survives-exit-is-its-own-verb`). Two consequences
//! shape everything here.
//!
//! **The policy is armed, never defaulted.** A [`Shell`](super::Shell) with
//! no policy simply lacks the capability: the host installs the `detach`
//! builtin and calls [`Shell::arm_detach`](super::Shell::arm_detach) in one
//! act, so the verb and the budget it spends cannot drift apart. Core mints
//! no default — there is no number it could honestly pick.
//!
//! **The counter is monotone.** [`DetachPolicy::admit`] counts *births*, not
//! occupancy: a detached process's death is unobservable from here, so a
//! release would be a number nobody can compute. This is deliberately unlike
//! the worker cap, where a settled worker frees its seat.
//!
//! **Flow rule.** The policy is `Arc`-shared into a spawned worker's own
//! `Shell` by [`Shell::spawn_thread`](super::Shell::spawn_thread), exactly as
//! the worker registry is, so a `detach` inside a `spawn { }` body spends the
//! owning session's budget rather than a private copy of it. It is equally
//! deliberately absent from [`LocalState`](super::LocalState)'s [`Drop`],
//! which cancels the session's workers: the surviving processes are the one
//! thing a teardown must leave alone.

use std::sync::atomic::{AtomicU64, Ordering};

/// A session's authority to birth processes that outlive it.
pub struct DetachPolicy {
    /// How many processes this session may birth over its whole life.
    pub budget: u64,
    /// Births admitted so far. Never decremented — see the module doc.
    pub(super) births: AtomicU64,
}

impl DetachPolicy {
    /// Admit one birth against the budget.
    ///
    /// # Errors
    ///
    /// The budget it was refused against, once every birth is spent, so the
    /// caller can name the number in the message it hands the user.
    /// Exhaustion is permanent: nothing gives a birth back.
    pub fn admit(&self) -> Result<(), u64> {
        self.births
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| {
                (n < self.budget).then_some(n + 1)
            })
            .map(drop)
            .map_err(|_| self.budget)
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
        assert_eq!(policy.admit(), Ok(()));
        assert_eq!(policy.admit(), Ok(()));
        assert_eq!(policy.admit(), Err(2));
        assert_eq!(policy.admit(), Err(2));
        assert_eq!(policy.births.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn tearing_down_a_session_leaves_the_policy_untouched() {
        let policy = Arc::new(DetachPolicy {
            budget: 2,
            births: AtomicU64::new(0),
        });
        assert_eq!(policy.admit(), Ok(()));
        let mut local = LocalState::default();
        local.detach = Some(policy.clone());
        drop(local);
        assert_eq!(policy.admit(), Ok(()));
        assert_eq!(policy.admit(), Err(2));
    }
}
