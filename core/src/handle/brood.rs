//! Where a birth hangs, and who answers for it.

use super::HandleInner;
use super::compacting::Compacting;
use crate::fact::WorkerId;
use crate::process::{Scope, wait_ended};
use crate::shell::runtime::Owner;
use crate::sync::LockExt as _;
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

/// Refusal from [`Brood::enrol`] once its [`BroodGuard`] has closed enrolment:
/// the rejected handle has already been claimed and stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EnrolClosed;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EnrolmentState {
    Open,
    Closed,
}

struct Enrolment {
    state: EnrolmentState,
    children: Compacting<HandleInner>,
}

impl Default for Enrolment {
    fn default() -> Self {
        Self {
            state: EnrolmentState::Open,
            children: Compacting::default(),
        }
    }
}

/// The shared enrolment side of a gather or worker brood.
///
/// Only [`BroodGuard::new`] constructs a fresh [`Brood`], and only
/// [`BroodGuard`] holds close, stop and drain authority.
#[derive(Clone)]
pub(crate) struct Brood {
    /// Whom its children's entries name as owner.
    pub(crate) owner: BroodOwner,
    enrolment: Arc<Mutex<Enrolment>>,
}

impl Brood {
    /// Enrol `handle` in birth order while the owner is open, compacting
    /// consumed entries on an amortized schedule.  If the owner has already
    /// closed enrolment, releases the enrolment lock, claims/stops `handle`,
    /// and returns [`Err(EnrolClosed)`](EnrolClosed).
    pub(crate) fn enrol(&self, handle: HandleInner) -> Result<(), EnrolClosed> {
        let mut guard = self.enrolment.lock_ignore_poison();
        let m = &mut *guard;
        match m.state {
            EnrolmentState::Open => {
                m.children.push(handle);
                Ok(())
            }
            EnrolmentState::Closed => {
                drop(guard);
                handle.claim_and_stop();
                Err(EnrolClosed)
            }
        }
    }
}

/// The unique guard owning a [`Brood`]'s lifecycle.
///
/// Dropping a `BroodGuard` closes enrolment and stops every unclaimed child
/// without waiting, so a panic in the body or in the exit join cannot leave
/// sibling workers running uncancelled.
pub(crate) struct BroodGuard {
    brood: Brood,
}

impl BroodGuard {
    pub(crate) fn new(owner: BroodOwner) -> Self {
        Self {
            brood: Brood {
                owner,
                enrolment: Arc::default(),
            },
        }
    }

    pub(crate) fn brood(&self) -> Brood {
        self.brood.clone()
    }

    /// Monotonically close enrolment and prune consumed entries once,
    /// retaining the remaining handles for join or stop.
    pub(crate) fn close(&self) {
        let mut m = self.brood.enrolment.lock_ignore_poison();
        if m.state == EnrolmentState::Open {
            m.state = EnrolmentState::Closed;
            m.children.prune();
        }
    }

    /// Every retained child, in birth order.
    pub(crate) fn children(&self) -> Vec<HandleInner> {
        self.brood.enrolment.lock_ignore_poison().children.to_vec()
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
        wait_ended(Instant::now() + grace, || all_ended(&children));
    }

    /// The halt rule (I4) with its drain (I4′): stop every child, then wait up
    /// to `grace` for all of them to reach a terminal state.
    pub(crate) fn end(&self, grace: Duration) {
        self.stop();
        self.drain(grace);
    }

    #[cfg(test)]
    pub(crate) fn retained_len(&self) -> usize {
        self.brood.enrolment.lock_ignore_poison().children.len()
    }

    #[cfg(test)]
    pub(crate) fn compaction_visits(&self) -> usize {
        self.brood.enrolment.lock_ignore_poison().children.visits()
    }
}

impl Drop for BroodGuard {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Claim and stop every unclaimed handle in `handles`, then wait up to a
/// single `grace` deadline for all of `handles` (including ones already
/// `Stopping`) to reach a terminal state.
pub(crate) fn stop_and_drain(handles: &[HandleInner], grace: Duration) {
    for handle in handles {
        handle.claim_and_stop();
    }
    wait_ended(Instant::now() + grace, || all_ended(handles));
}

/// The witness a brood's and a `race`'s drains wait on: every handle published
/// (`Ready` or `Consumed`), each worker's ending in hand, though its thread may
/// still be delivering its `done` and dropping its shell.
fn all_ended(handles: &[HandleInner]) -> bool {
    handles.iter().all(|h| h.sample().has_ended())
}

#[cfg(test)]
mod tests {
    use super::super::Sample;
    use super::super::compacting::MIN_SWEEP_AT;
    use super::*;
    use crate::handle::CompletionPermit;
    use crate::io::Streams;
    use crate::ir::ValClos;
    use crate::process::DurableRoot;
    use crate::shell::runtime::End;

    fn fresh_handle() -> (HandleInner, CompletionPermit) {
        HandleInner::new(
            WorkerId::mint(),
            "<brood-test>",
            DurableRoot::default().child(),
            Streams::default(),
        )
    }

    #[test]
    fn enrol_on_closed_enrolment_cancels_running_and_consumes_ready() {
        let owner = BroodGuard::new(BroodOwner::Gather);
        let brood = owner.brood();
        owner.close();

        // A running handle rejected by closed enrolment is immediately Stopping
        // with cancellation signalled, without caller cleanup.
        let (running, _running_permit) = fresh_handle();
        assert_eq!(brood.enrol(running.clone()), Err(EnrolClosed));
        assert_eq!(running.sample(), Sample::Stopping);
        assert!(running.home.is_cancelled());

        // An already-ready handle rejected by closed enrolment is immediately Consumed.
        let (ready, ready_permit) = fresh_handle();
        ready_permit.finish(End::Ok(ValClos::int(7)));
        assert_eq!(ready.sample(), Sample::Ready(()));
        assert_eq!(brood.enrol(ready.clone()), Err(EnrolClosed));
        assert_eq!(ready.sample(), Sample::Consumed);
        assert_eq!(owner.retained_len(), 0);
    }

    #[test]
    fn dropping_brood_clone_does_not_stop_children_but_dropping_owner_does() {
        let owner = BroodGuard::new(BroodOwner::Gather);
        let brood = owner.brood();
        let (child, _permit) = fresh_handle();
        brood.enrol(child.clone()).expect("open");

        drop(brood);
        assert_eq!(child.sample(), Sample::Running(()));
        assert!(!child.home.is_cancelled());

        drop(owner);
        assert_eq!(child.sample(), Sample::Stopping);
        assert!(child.home.is_cancelled());
    }

    #[test]
    fn stop_and_drain_waits_for_already_stopping_handles_too() {
        let (already_stopping, stopping_permit) = fresh_handle();
        already_stopping.claim_and_stop();
        assert_eq!(already_stopping.sample(), Sample::Stopping);

        let (running, running_permit) = fresh_handle();
        let finisher = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            stopping_permit.finish(End::Ok(ValClos::UNIT));
            running_permit.finish(End::Ok(ValClos::UNIT));
        });

        stop_and_drain(
            &[already_stopping.clone(), running.clone()],
            Duration::from_secs(1),
        );
        assert_eq!(already_stopping.sample(), Sample::Consumed);
        assert_eq!(running.sample(), Sample::Consumed);
        finisher.join().unwrap();
    }

    #[test]
    fn compaction_is_amortized_on_all_live_and_mostly_consumed_workloads() {
        // 1. Mostly-consumed workload: 1 long-lived child + 200 consumed children.
        let owner = BroodGuard::new(BroodOwner::Gather);
        let brood = owner.brood();
        let (live, _live_permit) = fresh_handle();
        brood.enrol(live).expect("enrol live");

        for _ in 0..200 {
            let (h, permit) = fresh_handle();
            permit.finish(End::Ok(ValClos::UNIT));
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
        let live_owner = BroodGuard::new(BroodOwner::Gather);
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
