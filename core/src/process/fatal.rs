//! A terminal failure, bound to the life it ends.

use std::sync::Mutex;

use super::{CancelCause, ScopeNode};
use crate::sync::LockExt as _;

/// A terminal failure, latched once.
///
/// The first cause stands for good, and latching ends the life bound to it,
/// from whichever thread latches. Both happen under one lock, so a standing
/// cause implies an ended life.
///
/// A leaf lock, poison ignored: its updates are total.
pub struct Fatal<T>(Mutex<Latched<T>>);

struct Latched<T> {
    cause: Option<T>,
    life: Option<ScopeNode>,
}

impl<T> Default for Fatal<T> {
    fn default() -> Self {
        Self(Mutex::new(Latched {
            cause: None,
            life: None,
        }))
    }
}

impl<T> Fatal<T> {
    /// End `life` when this latches, at once if it has.
    pub fn bind(&self, life: ScopeNode) {
        let mut latched = self.0.lock_ignore_poison();
        debug_assert!(latched.life.is_none(), "one failure, one life");
        if latched.cause.is_some() {
            life.cancel(CancelCause::Cancelled);
        } else {
            latched.life = Some(life);
        }
    }

    /// The bound life, released unended: `None` if none is bound, as after a
    /// latch, which ended it.
    pub fn unbind(&self) -> Option<ScopeNode> {
        self.0.lock_ignore_poison().life.take()
    }
}

impl<T: Clone> Fatal<T> {
    /// The cause that stands, if one does.
    pub fn cause(&self) -> Option<T> {
        self.0.lock_ignore_poison().cause.clone()
    }

    /// Latch `cause` unless one stands, ending the bound life; the cause
    /// that stands.
    pub fn latch(&self, cause: T) -> T {
        let mut latched = self.0.lock_ignore_poison();
        if let Some(life) = latched.life.take() {
            life.cancel(CancelCause::Cancelled);
        }
        latched.cause.get_or_insert(cause).clone()
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests {
    use super::*;
    use crate::process::DurableRoot;

    #[test]
    fn the_first_cause_stands() {
        let failure = Fatal::<&str>::default();
        assert_eq!(failure.cause(), None);
        assert_eq!(failure.latch("first"), "first");
        assert_eq!(failure.latch("second"), "first");
        assert_eq!(failure.cause(), Some("first"));
    }

    #[test]
    fn latching_ends_the_bound_life() {
        let failure = Fatal::default();
        let life = DurableRoot::default().child();
        failure.bind(life.clone());
        assert!(!life.is_cancelled());
        failure.latch(());
        assert_eq!(life.cause(), Some(CancelCause::Cancelled));
    }

    #[test]
    fn a_life_bound_after_the_latch_ends_at_once() {
        let failure = Fatal::default();
        failure.latch(());
        let life = DurableRoot::default().child();
        failure.bind(life.clone());
        assert_eq!(life.cause(), Some(CancelCause::Cancelled));
    }

    #[test]
    fn an_unbound_life_outlives_the_latch() {
        let failure = Fatal::default();
        let life = DurableRoot::default().child();
        failure.bind(life.clone());
        assert!(failure.unbind().is_some());
        failure.latch(());
        assert!(!life.is_cancelled());
        assert!(failure.unbind().is_none());
    }
}
