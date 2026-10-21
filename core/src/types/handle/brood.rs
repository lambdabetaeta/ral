//! Where a birth hangs, and who answers for it.

use super::HandleInner;
use super::compacting::Compacting;
use crate::fact::WorkerId;
use crate::process::Scope;
use crate::sync::LockExt as _;
use crate::types::Owner;
use std::borrow::Borrow;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where this shell's births hang: under `under`, and, inside a gather (a
/// `gather { … }`, or a worker's body), enrolled in its brood.  A run's
/// cradle is loose under the session root and answers for nothing (the turn
/// is the wrong parent).
#[derive(Clone)]
pub(crate) struct Cradle {
    pub(crate) under: Scope,
    pub(crate) brood: Option<Brood>,
}

impl Cradle {
    pub(crate) fn loose(under: Scope) -> Self {
        Self { under, brood: None }
    }

    pub(crate) fn owning(under: Scope, brood: Brood) -> Self {
        Self {
            under,
            brood: Some(brood),
        }
    }
}

/// Who answers for a brood: the worker whose body it is, or a `gather`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BroodOwner {
    Worker(WorkerId),
    Gather,
}

impl From<BroodOwner> for Owner {
    fn from(owner: BroodOwner) -> Self {
        match owner {
            BroodOwner::Worker(id) => Self::Worker(id),
            BroodOwner::Gather => Self::Gather,
        }
    }
}

/// Refusal from [`Brood::enrol`] once its [`ScopeOwner`] has closed membership:
/// the rejected handle has already been claimed and stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EnrolClosed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MembershipState {
    Open,
    Closed,
}

struct Membership {
    state: MembershipState,
    children: Compacting<HandleInner>,
}

impl Default for Membership {
    fn default() -> Self {
        Self {
            state: MembershipState::Open,
            children: Compacting::default(),
        }
    }
}

/// The shared enrolment side of a gather or worker brood.
///
/// Only [`ScopeOwner::new`] constructs a fresh [`Brood`], and only
/// [`ScopeOwner`] holds close, stop and drain authority.
#[derive(Clone)]
pub(crate) struct Brood {
    /// Whom its children's entries name as owner.
    pub(crate) owner: BroodOwner,
    membership: Arc<Mutex<Membership>>,
}

impl Brood {
    /// Enrol `handle` in birth order while the owner is open, compacting
    /// consumed entries on an amortized schedule.  If the owner has already
    /// closed membership, releases the membership lock, claims/stops `handle`,
    /// and returns [`Err(EnrolClosed)`].
    pub(crate) fn enrol(&self, handle: HandleInner) -> Result<(), EnrolClosed> {
        let mut guard = self.membership.lock_ignore_poison();
        let m = &mut *guard;
        match m.state {
            MembershipState::Open => {
                m.children.push(handle);
                Ok(())
            }
            MembershipState::Closed => {
                drop(guard);
                handle.claim_and_stop();
                Err(EnrolClosed)
            }
        }
    }
}

/// The unique guard owning a [`Brood`]'s lifecycle.
///
/// Dropping a `ScopeOwner` closes enrolment and stops every unclaimed child
/// without waiting, so a panic in the body or in the exit join cannot leave
/// sibling workers running uncancelled.
pub(crate) struct ScopeOwner {
    brood: Brood,
}

impl ScopeOwner {
    pub(crate) fn new(owner: BroodOwner) -> Self {
        Self {
            brood: Brood {
                owner,
                membership: Arc::default(),
            },
        }
    }

    pub(crate) fn brood(&self) -> Brood {
        self.brood.clone()
    }

    /// Monotonically close enrolment and prune consumed entries once,
    /// retaining the remaining handles for join or stop.
    pub(crate) fn close(&self) {
        let mut m = self.brood.membership.lock_ignore_poison();
        if m.state == MembershipState::Open {
            m.state = MembershipState::Closed;
            m.children.prune();
        }
    }

    /// Every retained child, in birth order.
    pub(crate) fn children(&self) -> Vec<HandleInner> {
        self.brood.membership.lock_ignore_poison().children.to_vec()
    }

    /// Close enrolment, snapshot retained handles out of the lock, and claim
    /// and stop each one without waiting.
    pub(crate) fn stop(&self) {
        self.close();
        for child in self.children() {
            child.claim_and_stop();
        }
    }

    /// Wait up to a single `grace` deadline for every retained child to reach
    /// a terminal state (`Ready` or `Consumed`), including children already
    /// `Stopping`.
    pub(crate) fn drain(&self, grace: Duration) {
        let children = self.children();
        wait_ended(&children, grace);
    }

    /// The halt rule (I4) with its drain (I4′): stop every child, then wait up
    /// to `grace` for all of them to reach a terminal state.
    pub(crate) fn end(&self, grace: Duration) {
        self.stop();
        self.drain(grace);
    }

    #[cfg(test)]
    pub(crate) fn retained_len(&self) -> usize {
        self.brood.membership.lock_ignore_poison().children.len()
    }

    #[cfg(test)]
    pub(crate) fn compaction_visits(&self) -> usize {
        self.brood.membership.lock_ignore_poison().children.visits()
    }
}

impl Drop for ScopeOwner {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Claim and stop every unclaimed handle in `handles`, then wait up to a
/// single `grace` deadline for all of `handles` (including ones already
/// `Stopping`) to reach a terminal state.
pub(crate) fn stop_and_drain<'a>(
    handles: impl IntoIterator<Item = &'a HandleInner>,
    grace: Duration,
) {
    let handles: Vec<&HandleInner> = handles.into_iter().collect();
    for handle in &handles {
        handle.claim_and_stop();
    }
    wait_ended(&handles, grace);
}

/// Wait for every handle to reach a terminal state, up to `grace`.  Never
/// polls: the waiter's own scope may be the thing that was cancelled.
fn wait_ended<H: Borrow<HandleInner>>(handles: &[H], grace: Duration) {
    let deadline = Instant::now() + grace;
    while handles.iter().any(|h| !h.borrow().has_ended()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::super::Sample;
    use super::super::compacting::MIN_SWEEP_AT;
    use super::*;
    use crate::io::ByteBuffer;
    use crate::process::DurableRoot;
    use crate::types::{CompletionPermit, ThreadEnd, Value};

    fn fresh_handle() -> (HandleInner, CompletionPermit) {
        HandleInner::new(
            WorkerId::mint(),
            "<brood-test>",
            DurableRoot::default().worker(),
            ByteBuffer::default(),
            ByteBuffer::default(),
        )
    }

    #[test]
    fn enrol_on_closed_membership_cancels_running_and_consumes_ready() {
        let owner = ScopeOwner::new(BroodOwner::Gather);
        let brood = owner.brood();
        owner.close();

        // A running handle rejected by closed enrolment is immediately Stopping
        // with cancellation signalled, without caller cleanup.
        let (running, _running_permit) = fresh_handle();
        assert_eq!(brood.enrol(running.clone()), Err(EnrolClosed));
        assert_eq!(running.sample(), Sample::Stopping);
        assert!(running.cancel.is_cancelled());

        // An already-ready handle rejected by closed enrolment is immediately Consumed.
        let (ready, ready_permit) = fresh_handle();
        ready_permit.finish(ThreadEnd::Settled(Ok(Value::Int(7))));
        assert_eq!(ready.sample(), Sample::Ready(()));
        assert_eq!(brood.enrol(ready.clone()), Err(EnrolClosed));
        assert_eq!(ready.sample(), Sample::Consumed);
        assert_eq!(owner.retained_len(), 0);
    }

    #[test]
    fn dropping_brood_clone_does_not_stop_children_but_dropping_owner_does() {
        let owner = ScopeOwner::new(BroodOwner::Gather);
        let brood = owner.brood();
        let (child, _permit) = fresh_handle();
        brood.enrol(child.clone()).expect("open");

        drop(brood);
        assert_eq!(child.sample(), Sample::Running(()));
        assert!(!child.cancel.is_cancelled());

        drop(owner);
        assert_eq!(child.sample(), Sample::Stopping);
        assert!(child.cancel.is_cancelled());
    }

    #[test]
    fn stop_and_drain_waits_for_already_stopping_handles_too() {
        let (already_stopping, stopping_permit) = fresh_handle();
        assert!(already_stopping.claim_and_stop());
        assert_eq!(already_stopping.sample(), Sample::Stopping);

        let (running, running_permit) = fresh_handle();
        let finisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            stopping_permit.finish(ThreadEnd::Settled(Ok(Value::Unit)));
            running_permit.finish(ThreadEnd::Settled(Ok(Value::Unit)));
        });

        stop_and_drain([&already_stopping, &running], Duration::from_secs(1));
        assert_eq!(already_stopping.sample(), Sample::Consumed);
        assert_eq!(running.sample(), Sample::Consumed);
        finisher.join().unwrap();
    }

    #[test]
    fn compaction_is_amortized_on_all_live_and_mostly_consumed_workloads() {
        // 1. Mostly-consumed workload: 1 long-lived child + 200 consumed children.
        let owner = ScopeOwner::new(BroodOwner::Gather);
        let brood = owner.brood();
        let (live, _live_permit) = fresh_handle();
        brood.enrol(live).expect("enrol live");

        for _ in 0..200 {
            let (h, permit) = fresh_handle();
            permit.finish(ThreadEnd::Settled(Ok(Value::Unit)));
            let _ = h.claim_ready();
            assert_eq!(h.sample(), Sample::Consumed);
            brood.enrol(h).expect("enrol consumed");
        }
        assert!(
            owner.retained_len() <= MIN_SWEEP_AT,
            "consumed entries must remain bounded by the sweep threshold, got {}",
            owner.retained_len()
        );
        assert!(
            owner.compaction_visits() <= 250,
            "mostly-consumed compaction visits must be linear, got {}",
            owner.compaction_visits()
        );
        owner.close();
        assert_eq!(
            owner.retained_len(),
            1,
            "close prunes remaining consumed entries once"
        );

        // 2. All-live workload: 200 live children double sweep_at at 32, 64, 128.
        let live_owner = ScopeOwner::new(BroodOwner::Gather);
        let live_brood = live_owner.brood();
        let mut permits = Vec::new();
        for _ in 0..200 {
            let (h, permit) = fresh_handle();
            permits.push(permit);
            live_brood.enrol(h).expect("enrol live");
        }
        assert_eq!(live_owner.retained_len(), 200);
        // Sweeps happen at lengths 32, 64, 128 -> 32 + 64 + 128 = 224 visits (<< 200*199/2).
        assert_eq!(live_owner.compaction_visits(), 32 + 64 + 128);
        assert_eq!(permits.len(), 200);
    }
}
