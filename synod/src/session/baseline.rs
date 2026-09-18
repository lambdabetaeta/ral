//! The folder's shape as it stood before the first message, stat-walked on
//! a thread of its own from the moment
//! [`crate::session::Conversation::begin`] opens — see [`Baseline`].

use crate::workspace::manifest::{Manifest, Progress, Stop};

/// A baseline walk still running, and the switch that stops it.
///
/// Kept as its own type, rather than inline in [`Baseline`], so it alone
/// carries the [`Drop`] that stops-and-joins an abandoned walk: [`Baseline`]
/// itself stays a plain struct, free to be matched and moved out of
/// everywhere it already was, since only this nested type implements
/// [`Drop`].
struct PendingWalk {
    handle: Option<std::thread::JoinHandle<Result<Manifest, String>>>,
    stop: Stop,
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
    fn join(mut self) -> std::thread::Result<Result<Manifest, String>> {
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
        // Only an *unjoined* walk is stopped here. Tripping the switch is
        // permanent, and a walk [`Self::join`] already took has handed back
        // the one manifest the whole conversation is judged against.
        if let Some(handle) = self.handle.take() {
            self.stop.stop();
            let _ = handle.join();
        }
    }
}

/// The folder as it stood before the conversation touched it: a stat-walk
/// started the moment [`crate::session::Conversation::begin`] runs,
/// alongside the machine's boot, and joined the first time
/// [`crate::session::Conversation::exchange`] needs the settled manifest —
/// never before, since the guest must not write to the folder while its
/// baseline is still being read.
pub(super) struct Baseline {
    /// The walk while it is still running; taken by [`Self::settle`], which
    /// is where the [`Option`] earns its keep — it is the placeholder the
    /// `mem::replace` out of a running walk needs.
    walk: Option<PendingWalk>,
    /// What the walk settled into: the folder's shape, the walk's own plain
    /// sentence, or the sentence for a thread that panicked. `None` until
    /// [`Self::settle`] has run once.
    settled: Option<Result<Manifest, String>>,
}

impl Baseline {
    /// Start a walk of `root` on a thread of its own, stoppable through
    /// `stop` and narrating its progress through `progress`.
    ///
    /// `progress` is owned and `Send` because the walk outlives this call:
    /// it runs on past `begin`'s return, so a borrowed closure could not
    /// reach it.
    pub(super) fn spawn(
        root: std::path::PathBuf,
        stop: &Stop,
        mut progress: Box<dyn FnMut(u64) + Send>,
    ) -> Self {
        let walk_stop = stop.clone();
        let handle = std::thread::spawn(move || {
            Manifest::of_folder_via(&root, &walk_stop, &mut progress as Progress<'_>)
        });
        Self {
            walk: Some(PendingWalk {
                handle: Some(handle),
                stop: stop.clone(),
            }),
            settled: None,
        }
    }

    /// The settled manifest, joining the walk the first time this is asked;
    /// every call after the first finds the settled state already waiting,
    /// so a settled baseline never blocks or re-reads anything.
    ///
    /// # Errors
    /// The walk's own error, or a plain sentence if its thread panicked
    /// before finishing.
    pub(super) fn manifest(&mut self) -> Result<&Manifest, String> {
        self.settle();
        match self
            .settled
            .as_ref()
            .expect("settle leaves a settled outcome behind")
        {
            Ok(manifest) => Ok(manifest),
            Err(e) => Err(e.clone()),
        }
    }

    /// Tell a still-running walk to stop, so ending a conversation ends its
    /// baseline instead of waiting the walk out.  A settled baseline has no
    /// walk left to stop, and a stopped one is never asked for its manifest:
    /// only [`crate::session::Conversation::end`] halts a baseline.
    ///
    /// Takes `&self`: the switch is an atomic the walking thread reads, so
    /// tripping it is not a mutation of anything this side holds.
    pub(super) fn halt(&self) {
        if let Some(walk) = &self.walk {
            walk.stop();
        }
    }

    /// Join the walk the first time this is called; a call once already
    /// settled does nothing.
    fn settle(&mut self) {
        if let Some(walk) = self.walk.take() {
            self.settled = Some(match walk.join() {
                Ok(result) => result,
                Err(_) => Err(
                    "Synod crashed while reading the folder, so it cannot say what changed."
                        .to_string(),
                ),
            });
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "REASONED-SILENT: a fixture file written so the baseline walk has something \
              to read; the production code in this file touches no filesystem at all"
)]
mod tests {
    use super::*;
    use crate::test_fixture::workshop;

    /// Joining the walk must not trip its stop switch: what comes back is
    /// the whole folder, not the handful of entries a stopped walk would
    /// have reached — and a stopped walk answers `Err`, so this would be a
    /// loud failure rather than a quiet short manifest.
    #[test]
    fn a_settled_baseline_hands_back_the_whole_folder() {
        let dir = workshop("baseline-settled");
        std::fs::write(dir.path().join("letter.txt"), b"dear all").expect("fixture");

        let mut baseline = Baseline::spawn(
            dir.path().to_path_buf(),
            &Stop::default(),
            Box::new(|_so_far| {}),
        );
        let manifest = baseline.manifest().expect("the baseline walk succeeds");
        assert!(manifest.entries.contains_key("letter.txt"));

        // Asking twice must neither block nor re-walk: the second answer is
        // the first one, still standing.
        assert!(
            baseline
                .manifest()
                .expect("a settled baseline stays settled")
                .entries
                .contains_key("letter.txt")
        );
    }

    /// A baseline dropped without ever being asked for its manifest — the
    /// window closed before the first message — must leave no thread behind
    /// walking a folder nobody is waiting on.
    #[test]
    fn an_abandoned_baseline_stops_its_own_walk() {
        let dir = workshop("baseline-abandoned");
        std::fs::write(dir.path().join("letter.txt"), b"dear all").expect("fixture");

        let baseline = Baseline::spawn(
            dir.path().to_path_buf(),
            &Stop::default(),
            Box::new(|_so_far| {}),
        );
        // `Drop` stops and joins; a walk left running would outlive the
        // fixture directory this drops next.
        drop(baseline);
    }
}
