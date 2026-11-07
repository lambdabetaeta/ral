//! The collector's handle onto one running stage: its process or thread, the
//! parent's hold on its outbound edge, and the cancel this collector sent it.

use super::super::command::{self, Claim};
use super::collect::{Slot, StageEnd, StageObservation};
use super::route::HeldEdge;
use super::sentinel;
use super::thread::ThreadStage;
#[cfg(test)]
use crate::process::Scope;
use crate::process::jail::JailCgroup;
use crate::process::{CancelCause, Landing, Pgid, WaitOutcome, Watch};
use crate::shell::runtime::Prov;
use std::sync::Arc;

pub(super) struct StageHandle {
    /// Before `kind`: a forced end drops the hold before it joins the writer,
    /// which may be blocked on the edge.
    held: Option<HeldEdge>,
    kind: StageKind,
    slot: Slot,
    /// What the collector sent this stage, joined by `max`.
    pub(super) sent: Option<CancelCause>,
}

/// A direct external stage: no dedicated waiter thread — the reaper's
/// [`Watch`] holds the wait and posts the outcome.
pub(super) struct ExternalStage {
    pub(super) watch: Arc<Watch>,
    pub(super) prov: Prov,
    /// The shown name, and its arguments as vetted for exec: the stage's
    /// observation spells the same argv a standalone dispatch would.
    pub(super) name: String,
    pub(super) args: Vec<String>,
    /// Transient guest-jail cgroup, `None` outside a real Linux guest.
    pub(super) jail: Option<crate::process::jail::JailCgroup>,
    pub(super) pumps: command::Pumps,
    /// Where it sits against its collector's group: in it (`Joins`), or
    /// apart, behind a confining envelope, in its payload's own (`Owns`).
    pub(super) landing: Landing,
    /// The deny lines its failure may claim, as its start knew them.
    pub(super) claim: Claim,
}

pub(super) enum StageKind {
    External(ExternalStage),
    Thread(ThreadStage),
}

impl StageHandle {
    pub(super) fn new(kind: StageKind, held: Option<HeldEdge>, slot: Slot) -> Self {
        Self {
            kind,
            held,
            slot,
            sent: None,
        }
    }

    /// Records the cause; a thread is told, by the collector's write on its
    /// node, an external hears it only as the signal the collector sends next.
    pub(super) fn cancel(&mut self, cause: CancelCause) {
        self.sent = self.sent.max(Some(cause));
        if let StageKind::Thread(t) = &self.kind {
            t.cancel(cause);
        }
    }

    /// A reader-gone cancel, which an external can only hear as the kill.
    pub(super) fn cut(&mut self) {
        self.cancel(CancelCause::ReaderGone);
        if let StageKind::External(e) = &self.kind {
            e.watch.kill();
        }
    }

    /// `Some` for a thread stage alone.
    pub(super) fn thread(&self) -> Option<&ThreadStage> {
        match &self.kind {
            StageKind::Thread(t) => Some(t),
            StageKind::External(_) => None,
        }
    }

    /// `None` for a thread stage, which has no pid of its own to address.
    pub(super) fn watch(&self) -> Option<&Watch> {
        match &self.kind {
            StageKind::External(e) => Some(e.watch.as_ref()),
            StageKind::Thread(_) => None,
        }
    }

    /// The group this stage leads apart from its collector's: an envelope's
    /// payload's, which no signal to the collector's group reaches.
    pub(super) fn own_group(&self) -> Option<Pgid> {
        match &self.kind {
            StageKind::External(ExternalStage {
                landing: Landing::Owns(group),
                ..
            }) => Some(*group),
            StageKind::External(_) | StageKind::Thread(_) => None,
        }
    }

    pub(super) fn jail(&self) -> Option<&JailCgroup> {
        match &self.kind {
            StageKind::External(e) => e.jail.as_ref(),
            StageKind::Thread(_) => None,
        }
    }

    /// Set the sentinel on the outbound edge, once.  With `cut`, mark the
    /// edge dead first, so a write completing before the sentinel snapshots
    /// what is pending is caught by the sink's own post-check.
    pub(super) fn listen(&self, cut: bool) {
        if let Some(held) = &self.held {
            if cut {
                held.edge.mark_dead();
            }
            if !held.listening.replace(true) {
                sentinel::listen(Arc::downgrade(&held.reader), self.slot.clone());
            }
        }
    }

    /// Letting go of the watch here reaps the zombie, unless its group's pin
    /// still holds it: safe only because this stage has already left the
    /// collector's `stages`, so no kill can name its pid.
    pub(super) fn file_external_end(self, outcome: WaitOutcome) -> StageEnd {
        let Self {
            held, kind, sent, ..
        } = self;
        let StageKind::External(e) = kind else {
            panic!("Event::Ended named a stage that was not spawned as an external");
        };
        if let Ok(watch) = Arc::try_unwrap(e.watch) {
            let _ = watch.reap();
        }
        drop(held);
        StageEnd::External {
            prov: e.prov,
            name: e.name,
            args: e.args,
            outcome,
            jail: e.jail,
            pumps: e.pumps,
            sent,
            claim: e.claim,
        }
    }

    /// The thread is already returning, so this reclaims the join rather than
    /// waiting for it.
    pub(super) fn file_thread_end(self, obs: StageObservation) -> StageEnd {
        let Self { held, kind, .. } = self;
        let StageKind::Thread(t) = kind else {
            panic!("Event::Returned named a stage that was not a thread stage");
        };
        let stage = t.id();
        t.join_after_settled();
        drop(held);
        StageEnd::Thread { obs, stage }
    }

    /// An already-running external, for `collect.rs`'s own tests.
    #[cfg(test)]
    pub(super) fn for_test(slot: Slot, child: crate::process::ChildHandle) -> Self {
        Self::new(
            StageKind::External(ExternalStage {
                watch: slot.watch(child),
                prov: crate::shell::runtime::Audit::default().call_site().clone(),
                name: "test".to_string(),
                args: Vec::new(),
                jail: None,
                pumps: command::Pumps::default(),
                landing: Landing::Joins,
                claim: Claim::Unconfined,
            }),
            None,
            slot,
        )
    }

    /// A stage over `child`, which nobody watches for, for `step`'s
    /// transition-table tests: they synthesize the events, so all this owes
    /// them is `cut`'s dispatch and an already-dead pid to reap.
    #[cfg(test)]
    pub(super) fn fake_external_for_step_test(slot: Slot, child: std::process::Child) -> Self {
        // A throwaway channel: this pid's real exit must not reach the
        // collector, whose own `Ended` the test synthesizes by hand.
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch = Arc::new(
            crate::process::ChildHandle::from_std(child).into_watch(tx, std::convert::identity),
        );
        Self::new(
            StageKind::External(ExternalStage {
                watch,
                prov: crate::shell::runtime::Audit::default().call_site().clone(),
                name: "fake".to_string(),
                args: Vec::new(),
                jail: None,
                pumps: command::Pumps::default(),
                landing: Landing::Joins,
                claim: Claim::Unconfined,
            }),
            None,
            slot,
        )
    }

    /// No real thread, so an `Event::Returned` for it files as a real thread
    /// stage's own end would; the stage's scope comes back, so a test can name
    /// its reader-gone stop and read what the collector wrote.
    #[cfg(test)]
    pub(super) fn fake_thread_for_step_test(slot: Slot) -> (Self, Scope) {
        let (stage, scope) = ThreadStage::fake();
        (Self::new(StageKind::Thread(stage), None, slot), scope)
    }
}
