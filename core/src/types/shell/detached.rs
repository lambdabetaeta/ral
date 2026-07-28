//! The `detach` budget: the one piece of session state meant to outlive the
//! session.
//!
//! Counting is monotone. A detached process is double-forked and reparented
//! to pid 1, so its death is unobservable here and a release would be a
//! number nobody can compute — unlike the worker cap, whose seat a settled
//! worker frees.
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
    /// Admit one birth.
    ///
    /// # Errors
    ///
    /// The budget, once spent, so a caller can name the number to the user.
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
