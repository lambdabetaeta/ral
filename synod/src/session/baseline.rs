//! The folder's before-checkpoint, captured on a thread of its own from the
//! moment [`crate::session::Conversation::begin`] opens the store — see
//! [`Baseline`].

use crate::workspace;

/// A capture walk still running, and the switch that stops it.
///
/// Kept as its own type, rather than inline in [`Baseline::Pending`], so it
/// alone carries the [`Drop`] that stops-and-joins an abandoned walk:
/// [`Baseline`] itself stays a plain enum, free to be matched and moved out
/// of by value everywhere it already was, since only this nested type — not
/// [`Baseline`] — implements [`Drop`].
pub(super) struct PendingWalk {
    handle: Option<std::thread::JoinHandle<CaptureResult>>,
    stop: workspace::manifest::Stop,
}

impl PendingWalk {
    fn stop(&self) {
        self.stop.stop();
    }

    /// Join the thread, consuming the walk.
    ///
    /// # Panics
    /// Panics if called after the handle was already taken — never, since
    /// [`Baseline::settle`] is the only caller and it consumes the walk on
    /// its one call.
    fn join(mut self) -> std::thread::Result<CaptureResult> {
        self.handle
            .take()
            .expect("a PendingWalk's handle is taken at most once, by this call")
            .join()
    }
}

impl Drop for PendingWalk {
    /// A walk dropped without ever being joined — [`Conversation::begin`]'s
    /// error path, once `?` runs straight through rather than by way of an
    /// explicit abandon — must not leave its thread running past it: stop it
    /// and join, discarding whatever it produced.
    ///
    /// [`Conversation::begin`]: crate::session::Conversation::begin
    fn drop(&mut self) {
        // Only an *unjoined* walk is stopped here. The switch is the
        // store's own, shared with every capture it will ever take, and
        // tripping it is permanent: stopping a walk that [`Self::join`]
        // already took would end the baseline and every later exchange's
        // checkpoint with it.
        if let Some(handle) = self.handle.take() {
            self.stop.stop();
            let _ = handle.join();
        }
    }
}

/// The folder's baseline capture: started the moment
/// [`crate::session::Conversation::begin`] opens the store, alongside the
/// machine's boot, and joined the first time
/// [`crate::session::Conversation::exchange`] or
/// [`crate::session::Conversation::end`] needs the settled store — never
/// before, since the guest must not touch the folder while its baseline is
/// still being read.
pub(super) enum Baseline {
    Pending(PendingWalk),
    Ready(workspace::HistoryStore),
    /// The capture itself failed — a read error, a full disk — but the
    /// store it was capturing into came back regardless, since
    /// [`workspace::HistoryStore::capture`] only borrows it; `end` still
    /// has something to wipe.
    Failed(workspace::HistoryStore, String),
    /// The capture thread panicked outright, taking its store down with
    /// it: unwinding dropped that store's lock the same as a clean return
    /// would have, so the directory it leaves behind is unlocked, not
    /// leaked — [`workspace::history::sweep_stale`] collects it at the next
    /// start.
    Crashed(String),
    /// No copy was ever made: the folder did not fit where the store goes,
    /// and the opening said so. The conversation runs without undo rather
    /// than not running at all.
    Untaken,
    /// [`Baseline::settle`]'s placeholder for the instant between taking
    /// `Pending`'s walk and joining it — never observed outside that call.
    Settling,
}

/// What the capture thread hands back: the store it captured into,
/// regardless of outcome, and the capture's own result.
pub(super) type CaptureResult = (
    workspace::HistoryStore,
    Result<workspace::history::Checkpoint, String>,
);

impl Baseline {
    /// Start a baseline capturing `history` on a thread of its own.
    pub(super) fn spawn(root: std::path::PathBuf, history: workspace::HistoryStore) -> Self {
        let stop = history.stop_switch();
        let handle = std::thread::spawn(move || {
            let result = history.capture(&root, workspace::Moment::Before);
            (history, result)
        });
        Self::Pending(PendingWalk {
            handle: Some(handle),
            stop,
        })
    }

    /// The settled store, joining the capture thread the first time this is
    /// asked; every call after the first finds the settled state already
    /// waiting, so a settled baseline never blocks or re-reads anything.
    /// `None` for a conversation that has no safety copy at all — not a
    /// failure, a conversation the user was told runs without undo.
    ///
    /// # Errors
    /// The capture's own error, or a plain sentence if its thread panicked
    /// before finishing.
    pub(super) fn store(&mut self) -> Result<Option<&workspace::HistoryStore>, String> {
        self.settle();
        match self {
            Self::Ready(store) => Ok(Some(store)),
            Self::Untaken => Ok(None),
            Self::Failed(_, e) | Self::Crashed(e) => Err(e.clone()),
            Self::Pending(_) | Self::Settling => unreachable!("settled just above"),
        }
    }

    /// Tell a still-running capture to stop, so ending a conversation ends
    /// its copy instead of waiting the copy out. A settled baseline has no
    /// walk left to stop, and a stopped one is never asked for its store:
    /// only [`crate::session::Conversation::end`] halts a baseline, and it
    /// wipes.
    pub(super) fn halt(&mut self) {
        if let Self::Pending(walk) = self {
            walk.stop();
        }
    }

    /// Join the capture thread the first time this is called, settling
    /// into [`Self::Ready`], [`Self::Failed`], or [`Self::Crashed`]; a call
    /// once already settled does nothing.
    fn settle(&mut self) {
        if let Self::Pending(_) = self {
            let Self::Pending(walk) = std::mem::replace(self, Self::Settling) else {
                unreachable!("just matched Pending above")
            };
            *self = match walk.join() {
                Ok((store, Ok(_before))) => Self::Ready(store),
                Ok((store, Err(e))) => Self::Failed(store, e),
                Err(_) => Self::Crashed(
                    "Synod's safety copy of the folder crashed while it was being read."
                        .to_string(),
                ),
            };
        }
    }

    /// The store [`crate::session::Conversation::end`] wipes, settling
    /// first so a still-running capture is joined rather than abandoned —
    /// `None` only for [`Self::Crashed`], the one case with no store to hand
    /// back.
    pub(super) fn into_store(mut self) -> Option<workspace::HistoryStore> {
        self.settle();
        match self {
            Self::Ready(store) | Self::Failed(store, _) => Some(store),
            Self::Untaken | Self::Crashed(_) | Self::Pending(_) | Self::Settling => None,
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: a fixture file written so the baseline walk has something               to read; the production code in this file touches no filesystem at all"
)]
mod tests {
    use super::*;
    use crate::test_fixture::granted_workshop;

    /// The baseline's walk shares its stop switch with the store it
    /// captures into, so joining that walk must not trip the switch: every
    /// later exchange checkpoints through the very same store.
    #[test]
    fn a_settled_baseline_leaves_the_store_able_to_capture_again() {
        let (_dir, folder, store) = granted_workshop("baseline-settled");
        std::fs::write(folder.join("letter.txt"), b"dear all").expect("fixture");

        let mut baseline = Baseline::spawn(folder.clone(), store);
        let store = baseline
            .store()
            .expect("the baseline capture succeeds")
            .expect("a copy was taken");

        store
            .capture(&folder, workspace::Moment::After)
            .expect("a settled baseline leaves its store able to capture again");
    }
}
