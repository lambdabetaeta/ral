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
use crate::process::{CancelCause, Pgid, WaitOutcome, Watch};
use crate::shell::runtime::{AuditStart, Prov};
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
/// [`Watch`] holds the wait and posts the outcome.  Filing its end reaps the
/// watch and hands the rest on as an [`ExternalEnd`].
pub(super) struct LiveExternal {
    pub(super) watch: Arc<Watch>,
    /// Apart from its collector's group, behind a confining envelope: its
    /// payload's own.
    pub(super) payload: Option<Pgid>,
    pub(super) end: ExternalEnd,
}

/// What an external's settlement reads once its process is gone.
pub(super) struct ExternalEnd {
    pub(super) name: String,
    /// Absent for a lone command, whose own door claims and records.
    pub(super) audit: Option<Box<StageAudit>>,
    /// Transient guest-jail cgroup, `None` outside a real Linux guest.
    pub(super) jail: Option<crate::process::jail::JailCgroup>,
    pub(super) pumps: command::Pumps,
    /// The deny lines its failure may claim, as its start knew them.
    pub(super) claim: Claim,
}

/// What a pipeline stage's settlement owes its audit: the site that claims
/// its failure, and, if anyone listened at launch, where its record starts
/// and the argv as vetted for exec, spelled as a standalone dispatch would.
pub(super) struct StageAudit {
    pub(super) prov: Prov,
    pub(super) start: Option<AuditStart>,
    pub(super) args: Vec<String>,
}

pub(super) enum StageKind {
    External(LiveExternal),
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
            StageKind::External(e) => e.payload,
            StageKind::Thread(_) => None,
        }
    }

    pub(super) fn jail(&self) -> Option<&JailCgroup> {
        match &self.kind {
            StageKind::External(e) => e.end.jail.as_ref(),
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
        let StageKind::External(LiveExternal { watch, end, .. }) = kind else {
            panic!("Event::Ended named a stage that was not spawned as an external");
        };
        if let Ok(watch) = Arc::try_unwrap(watch) {
            let _ = watch.reap();
        }
        drop(held);
        StageEnd::External { end, outcome, sent }
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
            StageKind::External(LiveExternal {
                watch: slot.watch(child),
                payload: None,
                end: ExternalEnd {
                    name: "test".to_string(),
                    audit: None,
                    jail: None,
                    pumps: command::Pumps::default(),
                    claim: Claim::Unconfined,
                },
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
            StageKind::External(LiveExternal {
                watch,
                payload: None,
                end: ExternalEnd {
                    name: "fake".to_string(),
                    audit: None,
                    jail: None,
                    pumps: command::Pumps::default(),
                    claim: Claim::Unconfined,
                },
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
