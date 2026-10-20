//! Where a birth hangs, and who answers for it.

use super::HandleInner;
use crate::fact::WorkerId;
use crate::process::Scope;
use crate::sync::LockExt as _;
use crate::types::Owner;
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

/// The births a gather made, in birth order: what its exit settles.
#[derive(Clone)]
pub(crate) struct Brood {
    /// Whom its children's entries name as owner.
    pub(crate) owner: BroodOwner,
    children: Arc<Mutex<Vec<HandleInner>>>,
}

impl Brood {
    pub(crate) fn new(owner: BroodOwner) -> Self {
        Self {
            owner,
            children: Arc::default(),
        }
    }

    /// Prunes only a child that is claimed and has ended: a cancelled one
    /// still running stays for [`Self::wait_cancelled`].
    pub(crate) fn enrol(&self, handle: HandleInner) {
        let mut children = self.children.lock_ignore_poison();
        children.retain(|child| !(child.is_claimed() && child.has_ended()));
        children.push(handle);
    }

    /// Every child, in birth order.
    pub(crate) fn children(&self) -> Vec<HandleInner> {
        self.children.lock_ignore_poison().clone()
    }

    /// I4′: wait up to `grace` for every child whose scope is cancelled and
    /// whose thread has not ended — the ones this brood stopped, and the ones
    /// a `cancel` in the body or from outside stopped.  A child an outside
    /// claimer took and left running is that claimer's, and is not waited for.
    pub(crate) fn wait_cancelled(&self, grace: Duration) {
        wait_cancelled_of(&self.children(), grace);
    }

    /// The halt rule (I4) with its drain (I4′): claim and stop every child no
    /// one has claimed, then wait up to `grace` for every cancelled child to end.
    pub(crate) fn end(&self, grace: Duration) {
        let children = self.children();
        for child in &children {
            child.claim_and_stop();
        }
        wait_cancelled_of(&children, grace);
    }
}

fn wait_cancelled_of(children: &[HandleInner], grace: Duration) {
    let cancelled: Vec<&HandleInner> = children
        .iter()
        .filter(|h| h.cancel.is_cancelled())
        .collect();
    wait_ended(&cancelled, grace);
}

/// Claim and stop every handle no one has claimed, then wait for their
/// threads to end up to `grace`.
pub(crate) fn end_unclaimed<'a>(
    handles: impl IntoIterator<Item = &'a HandleInner>,
    grace: Duration,
) {
    let claimed: Vec<&HandleInner> = handles.into_iter().filter(|h| h.claim_and_stop()).collect();
    wait_ended(&claimed, grace);
}

/// Wait for every handle's thread to end, up to `grace`.  Never polls: the
/// waiter's own scope may be the thing that was cancelled.
fn wait_ended(handles: &[&HandleInner], grace: Duration) {
    let deadline = Instant::now() + grace;
    while handles.iter().any(|h| !h.has_ended()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}
