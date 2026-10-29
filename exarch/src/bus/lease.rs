//! An exchange's cancellation follows its owner.

use ral_core::process::{CancelCause, Scope, ScopeNode};
use std::ops::Deref;

/// A scope held for as long as the lease lives: dropping it cancels.
#[must_use]
pub(crate) struct Lease(ScopeNode);

impl Lease {
    pub(crate) fn under(parent: &Scope) -> Self {
        Self(parent.child())
    }

    pub(crate) fn node(&self) -> ScopeNode {
        self.0.clone()
    }
}

impl Deref for Lease {
    type Target = Scope;

    fn deref(&self) -> &Scope {
        &self.0
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.cancel(CancelCause::Cancelled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ral_core::process::DurableRoot;

    #[test]
    fn dropping_an_exchange_cancels_its_node_but_not_its_life_or_siblings() {
        let life = DurableRoot::default().child();
        let exchange = Lease::under(&life);
        let sibling = Lease::under(&life);
        let node = exchange.node();
        assert_eq!(exchange.id(), node.id());
        drop(exchange);
        assert_eq!(node.cause(), Some(CancelCause::Cancelled));
        assert!(!life.is_cancelled());
        assert!(!sibling.is_cancelled());
    }

    #[test]
    fn ending_a_life_cancels_existing_and_new_exchange_leases() {
        let life = DurableRoot::default().child();
        let exchange = Lease::under(&life);
        life.cancel(CancelCause::Terminated);
        assert_eq!(exchange.cause(), Some(CancelCause::Terminated));
        assert_eq!(Lease::under(&life).cause(), Some(CancelCause::Terminated));
    }

    #[test]
    fn a_stronger_exchange_cause_survives_lease_drop() {
        let life = DurableRoot::default().child();
        let exchange = Lease::under(&life);
        let node = exchange.node();
        node.cancel(CancelCause::TimedOut);
        drop(exchange);
        assert_eq!(node.cause(), Some(CancelCause::TimedOut));
        assert!(!life.is_cancelled());
    }

    #[test]
    fn unwinding_an_exchange_cancels_the_node_it_published() {
        let life = DurableRoot::default().child();
        let mut node = None;
        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let exchange = Lease::under(&life);
            node = Some(exchange.node());
            panic!("exchange unwound");
        }));
        assert!(unwind.is_err());
        assert_eq!(node.unwrap().cause(), Some(CancelCause::Cancelled));
        assert!(!life.is_cancelled());
    }
}
