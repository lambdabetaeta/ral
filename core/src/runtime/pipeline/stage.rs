//! The collector's handle onto one running stage: its process or thread, the
//! parent's hold on its outbound edge, and the cancel this collector sent it.

use super::super::command;
use super::collect::{Slot, StageEnd, StageObservation};
use super::route::HeldEdge;
use super::sentinel;
use super::thread::ThreadStage;
use crate::process::{CancelCause, Pgid, WaitOutcome, Watch};
use std::sync::Arc;

pub(super) struct StageHandle {
    kind: StageKind,
    held: Option<HeldEdge>,
    slot: Slot,
    /// What the collector sent this stage, joined by `max`.
    pub(super) sent: Option<CancelCause>,
}

/// A direct external stage: no dedicated waiter thread — the reaper's
/// [`Watch`] holds the wait and posts the outcome.
pub(super) struct ExternalStage {
    pub(super) watch: Watch,
    pub(super) name: String,
    /// Transient guest-jail cgroup, `None` outside a real Linux guest.
    pub(super) jail: Option<crate::process::jail::JailCgroup>,
    pub(super) pumps: command::Pumps,
    /// The payload's own process group behind a confining envelope; `None`
    /// for an unconfined stage, which joined the pipeline's.
    pub(super) envelope: Option<Pgid>,
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

    /// Records the cause; a thread is told, an external hears it only as the
    /// signal the collector sends next.
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

    /// `None` for a thread stage, which has no pid of its own to address.
    pub(super) fn watch(&self) -> Option<&Watch> {
        match &self.kind {
            StageKind::External(e) => Some(&e.watch),
            StageKind::Thread(_) => None,
        }
    }

    /// `Some` only for an external stage confined behind an envelope.
    pub(super) fn envelope(&self) -> Option<Pgid> {
        match &self.kind {
            StageKind::External(e) => e.envelope,
            StageKind::Thread(_) => None,
        }
    }

    /// Mark the outbound edge dead and set the sentinel on its read end.
    /// Marked before the sentinel snapshots what is pending, so a write
    /// completing between the two is caught by the sink's own post-check.
    pub(super) fn arm(&self) {
        if let Some(held) = &self.held {
            held.edge.mark_dead();
            sentinel::listen(Arc::clone(&held.reader), self.slot.clone());
        }
    }

    /// Reaping here is safe only because this stage has already left the
    /// collector's `stages`, so no kill can name a reaped pid.
    pub(super) fn file_external_end(self, outcome: WaitOutcome) -> StageEnd {
        let Self {
            held, kind, sent, ..
        } = self;
        let StageKind::External(e) = kind else {
            panic!("Event::Ended named a stage that was not spawned as an external");
        };
        let _ = e.watch.reap();
        drop(held);
        StageEnd::External {
            name: e.name,
            outcome,
            jail: e.jail,
            pumps: e.pumps,
            sent,
            enveloped: e.envelope.is_some(),
        }
    }

    /// The thread is already returning, so this reclaims the join rather than
    /// waiting for it.
    pub(super) fn file_thread_end(self, obs: StageObservation) -> StageEnd {
        let Self { held, kind, .. } = self;
        let StageKind::Thread(t) = kind else {
            panic!("Event::Returned named a stage that was not a thread stage");
        };
        t.join_after_settled();
        drop(held);
        StageEnd::Thread(obs)
    }

    /// An already-running external, for `collect.rs`'s own tests.
    #[cfg(test)]
    pub(super) fn for_test(slot: Slot, child: crate::process::ChildHandle) -> Self {
        Self::new(
            StageKind::External(ExternalStage {
                watch: slot.watch(child),
                name: "test".to_string(),
                jail: None,
                pumps: command::Pumps::default(),
                envelope: None,
            }),
            None,
            slot,
        )
    }

    /// A real child nobody watches for, for `step`'s transition-table tests:
    /// they synthesize the events, so all this owes them is `cut`'s dispatch
    /// and an already-dead pid to reap.
    #[cfg(test)]
    #[allow(clippy::disallowed_methods, reason = "[test] test process scaffolding")]
    pub(super) fn fake_external_for_step_test(slot: Slot) -> Self {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a fake stage's child");
        let _ = child.kill();
        // A throwaway channel: this pid's real exit must not reach the
        // collector, whose own `Ended` the test synthesizes by hand.
        let (tx, _rx) = std::sync::mpsc::channel();
        let watch =
            crate::process::ChildHandle::from_std(child).into_watch(tx, std::convert::identity);
        Self::new(
            StageKind::External(ExternalStage {
                watch,
                name: "fake".to_string(),
                jail: None,
                pumps: command::Pumps::default(),
                envelope: None,
            }),
            None,
            slot,
        )
    }

    /// No real thread, so an `Event::Returned` for it files as a real thread
    /// stage's own end would.
    #[cfg(test)]
    pub(super) fn fake_thread_for_step_test(slot: Slot) -> Self {
        Self::new(
            StageKind::Thread(ThreadStage::fake_for_step_test()),
            None,
            slot,
        )
    }
}
