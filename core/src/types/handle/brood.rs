//! Where a loose birth hangs, and who answers for it.

use super::HandleInner;
use crate::fact::WorkerId;
use crate::process::{Scope, ScopeNode};
use crate::sync::LockExt as _;
use crate::types::Shell;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Where this shell's loose births hang: under `under`, and, inside a
/// worker, enrolled in that worker's brood.  A run's cradle is loose under
/// the session root and answers for nothing (the turn is the wrong parent).
#[derive(Clone)]
pub(crate) struct Cradle {
    pub(crate) under: Scope,
    pub(crate) brood: Option<Brood>,
}

impl Cradle {
    pub(crate) fn loose(under: Scope) -> Self {
        Self { under, brood: None }
    }

    pub(crate) fn of_worker(home: &ScopeNode, brood: Brood) -> Self {
        Self {
            under: Scope::clone(home),
            brood: Some(brood),
        }
    }
}

/// The loose births a worker made, in birth order: what its base settles.
#[derive(Clone)]
pub(crate) struct Brood {
    /// The worker it belongs to, which its children's entries name as owner.
    pub(crate) worker: WorkerId,
    children: Arc<Mutex<Vec<HandleInner>>>,
}

impl Brood {
    pub(crate) fn new(worker: WorkerId) -> Self {
        Self {
            worker,
            children: Arc::default(),
        }
    }

    pub(crate) fn enrol(&self, handle: HandleInner) {
        let mut children = self.children.lock_ignore_poison();
        children.retain(|child| !child.is_claimed());
        children.push(handle);
    }

    /// Every child no eliminator has claimed, in birth order.
    pub(crate) fn unclaimed(&self) -> Vec<HandleInner> {
        self.children
            .lock_ignore_poison()
            .iter()
            .filter(|child| !child.is_claimed())
            .cloned()
            .collect()
    }

    /// The halt rule: [`end_unclaimed`] over this brood.
    pub(crate) fn end(&self, shell: &Shell, grace: Duration) {
        end_unclaimed(&self.unclaimed(), shell, grace);
    }
}

/// An eliminator has settled `handle`: its entry leaves the registry, and no
/// brood or litter answers for it.
pub(crate) fn claim(handle: &HandleInner, shell: &Shell) {
    handle.claim();
    shell.local.workers.remove(handle);
}

/// Stop and claim every unclaimed handle, then wait for their threads to end
/// up to `grace`.
pub(crate) fn end_unclaimed(handles: &[HandleInner], shell: &Shell, grace: Duration) {
    let unclaimed: Vec<&HandleInner> = handles.iter().filter(|h| !h.is_claimed()).collect();
    for handle in &unclaimed {
        handle.stop();
        claim(handle, shell);
    }
    wait_ended(&unclaimed, grace);
}

/// Wait for every handle's thread to end, up to `grace`.  Never polls: the
/// waiter's own scope may be the thing that was cancelled.
fn wait_ended(handles: &[&HandleInner], grace: Duration) {
    let deadline = Instant::now() + grace;
    while handles.iter().any(|h| !h.has_ended()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
}
