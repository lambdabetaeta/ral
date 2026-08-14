//! The commit producer's grouping half, moved here whole from `tui/surface.rs`
//! by `git mv` and re-pointed at the seam: [`PatchBuf`] coalesces consecutive
//! same-`(id, path)` diff hunks into one `▎ diff` [`Display::Card`], and
//! [`ObservationBuf`] dedupes read/exec/grep events by kind before recording
//! each as its own [`Display::Observation`].  Both are session-keyed, so a
//! session change flushes and reopens rather than merging two sessions into
//! one commit.
//!
//! Grouping survives the move as a record-time *dedup and ordering* decision
//! — a repeated read within one run is still one fact, and a write still
//! flushes behind the reads it arrived among — but no longer as a single
//! combined card: the frozen [`Display`] vocabulary has no variant for a
//! *grouped* read/exec/grep card, and the wiki text assigns that
//! reconstruction to a printer reading consecutive [`Display::Observation`]
//! commits, not to the producer.  `observation_wire` (`bus/card.rs`, P4's
//! parcel) is what makes the individual commit possible: it carries the
//! observation's total wire form, not the mark tree a live rail drew from it.
//!
//! **Not wired to a live call site.**  Nothing yet hands this a
//! [`crate::record::Emitter`] or an [`Observation`]: `tui::app`'s three call
//! sites still address the old, viewport-mutating shape this module no
//! longer has, and per the plan the worker side — not `app.rs` — is meant to
//! own the coalescing going forward.  Rewiring `tui::app` is the view fold's
//! and the resume path's business (parcels P3/P6), not this one's.
//!
//! The chopper the commit producer is also meant to own — accumulating the
//! assistant delta stream and committing markdown at each fence-safe break —
//! is not implemented here for the same reason [`crate::record::view`]
//! reports: the frozen `Display` vocabulary has no variant for streamed
//! assistant prose (`Display::Thinking` is reasoning only, keyed to
//! `answer_chars`, not the answer itself), so there is nothing yet to commit
//! a chopped chunk *as*.  See this parcel's report for the exact gap.
//!
//! A write rides the observation buffer too, though it joins no group.  It is a
//! barrier — it ends the ral run before it — but it is still an effect of the
//! call that issued it, and a redirect writes at the seam, mid-call.  Landed
//! eagerly it would split a call from the reads it had yet to make; buffered, it
//! flushes *behind* them, so a call's effects stay contiguous and the coalescing
//! projection never has to reunite a call with effects stranded past a barrier.

use std::io;

use crate::bus::AgentId;
use crate::bus::card::{Card, Hunk, Mark, observation_wire};
use crate::record::{Display, Emitter};
use ral_core::types::{Observation, Observed};

pub(crate) struct SurfaceBuffer {
    patch_buf: Option<PatchBuf>,
    observation_buf: Option<ObservationBuf>,
}

struct PatchBuf {
    id: AgentId,
    path: String,
    hunks: Vec<Hunk>,
}

/// Buckets are deduped and order-independent — the user does not care in what
/// order a burst interleaved.  Holding the whole [`Observation`], envelope
/// included, is what lets flush call [`observation_wire`] on each one; the
/// old buffer kept only the bare `Observed` fact, which sufficed for
/// rendering but not for a faithful record.
///
/// `writes` is the exception on both counts: a mutation is not deduped away and
/// not reordered among its fellows, since two writes to one path are two facts.
/// It is a bucket only so a write leaves the buffer at the same boundary as the
/// reads around it, and it flushes last — the barrier closing the run.
struct ObservationBuf {
    id: AgentId,
    reads: Vec<Observation>,
    execs: Vec<Observation>,
    greps: Vec<Observation>,
    writes: Vec<Observation>,
}

impl SurfaceBuffer {
    pub(crate) fn new() -> Self {
        Self {
            patch_buf: None,
            observation_buf: None,
        }
    }

    /// Extend the open buffer on a matching `(id, path)`, else flush and
    /// reopen, so consecutive edits to one file record as one `Display::Card`
    /// diff.
    ///
    /// # Errors
    /// Propagates a failed flush of whatever the new group displaced.
    pub(crate) fn absorb_patch(
        &mut self,
        emitter: &Emitter,
        id: AgentId,
        path: String,
        hunks: Vec<Hunk>,
    ) -> io::Result<()> {
        let same = self
            .patch_buf
            .as_ref()
            .is_some_and(|b| b.id == id && b.path == path);
        if same {
            let buf = self.patch_buf.as_mut().expect("same-path implies Some");
            buf.hunks.extend(hunks);
        } else {
            self.flush_surfaces(emitter)?;
            self.patch_buf = Some(PatchBuf { id, path, hunks });
        }
        Ok(())
    }

    fn flush_patch_buf(&mut self, emitter: &Emitter) -> io::Result<()> {
        let Some(buf) = self.patch_buf.take() else {
            return Ok(());
        };
        let card = Card(vec![Mark::Diff {
            path: buf.path,
            hunks: buf.hunks,
        }]);
        let marks = serde_json::to_value(&card).expect("Card's derived Serialize cannot fail");
        emitter.emit(Display::Card { marks })?;
        Ok(())
    }

    /// Bucket a read/exec/grep/write, flushing first on a session change.
    ///
    /// # Errors
    /// Propagates a failed flush of a prior session's buffer.
    pub(crate) fn absorb_observation(
        &mut self,
        emitter: &Emitter,
        id: AgentId,
        event: Observation,
    ) -> io::Result<()> {
        if self.observation_buf.as_ref().is_some_and(|b| b.id != id) {
            self.flush_observations(emitter)?;
        }
        let buf = self.observation_buf.get_or_insert_with(|| ObservationBuf {
            id,
            reads: Vec::new(),
            execs: Vec::new(),
            greps: Vec::new(),
            writes: Vec::new(),
        });
        match &event.what {
            Observed::Read { path } => {
                let dup = buf
                    .reads
                    .iter()
                    .any(|o| matches!(&o.what, Observed::Read { path: p } if p == path));
                if !dup {
                    buf.reads.push(event);
                }
            }
            Observed::Command { argv, .. } => {
                let dup = buf.execs.iter().any(
                    |o| matches!(&o.what, Observed::Command { argv: a, .. } if a == argv),
                );
                if !dup {
                    buf.execs.push(event);
                }
            }
            Observed::Grep { .. } => {
                let dup = buf.greps.iter().any(|o| o.what == event.what);
                if !dup {
                    buf.greps.push(event);
                }
            }
            Observed::Write { .. } => buf.writes.push(event),
            Observed::Capability { .. } => {
                unreachable!("a capability check records as its own commit, never bucketed")
            }
            Observed::Worker { .. } => {
                unreachable!("a worker birth records as its own commit, never bucketed")
            }
            Observed::Act { .. } => {
                unreachable!("an `Act` never reaches the commit producer from the engine seam")
            }
        }
        Ok(())
    }

    /// Record each buffered observation as its own [`Display::Observation`],
    /// in the barrier order the module doc names: reads, execs, and greps —
    /// deduped, order-independent among themselves — then every write, last
    /// and undeduped.
    ///
    /// # Errors
    /// Propagates the first failed emit; later observations in the same
    /// flush are left unrecorded rather than recorded out of order.
    fn flush_observations(&mut self, emitter: &Emitter) -> io::Result<()> {
        let Some(buf) = self.observation_buf.take() else {
            return Ok(());
        };
        for observed in buf
            .reads
            .into_iter()
            .chain(buf.execs)
            .chain(buf.greps)
            .chain(buf.writes)
        {
            let value = observation_wire(&observed);
            emitter.emit(Display::Observation { value })?;
        }
        Ok(())
    }

    /// The shared commit boundary: io first, so an io group lands ahead of a
    /// diff buffered before it.
    ///
    /// # Errors
    /// Propagates a failed emit from either half.
    pub(crate) fn flush_surfaces(&mut self, emitter: &Emitter) -> io::Result<()> {
        self.flush_observations(emitter)?;
        self.flush_patch_buf(emitter)
    }

    /// Discard both buffers unrecorded — `/clear` wipes the scrollback they
    /// would have landed in.
    pub(crate) fn clear(&mut self) {
        self.patch_buf = None;
        self.observation_buf = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::seam::Published;
    use ral_core::syntax::ast::RedirectMode;
    use ral_core::types::{CallSite, WriteOutcome};
    use std::sync::mpsc::Receiver;

    fn emitter() -> (Emitter, Receiver<Published>) {
        let path = std::env::temp_dir().join(format!(
            "exarch-commit-test-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        Emitter::create(&path).expect("temp record log")
    }

    fn site() -> CallSite {
        CallSite {
            script: "test".into(),
            line: 1,
            col: 1,
        }
    }

    fn observation(what: Observed) -> Observation {
        Observation {
            site: site(),
            start: 0,
            end: 0,
            principal: "alex".into(),
            what,
        }
    }

    fn write(path: &str) -> Observation {
        observation(Observed::Write {
            path: path.into(),
            mode: RedirectMode::Write,
            outcome: WriteOutcome::Committed,
            new_bytes: None,
            old_bytes: None,
        })
    }

    fn read(path: &str) -> Observation {
        observation(Observed::Read { path: path.into() })
    }

    fn drain_display(rx: &Receiver<Published>) -> Vec<Display> {
        rx.try_iter()
            .filter_map(|p| match p {
                Published::Fact(rec) => match rec.into_value() {
                    crate::record::Record::Display(d) => Some(d),
                    _ => None,
                },
                Published::Transient(_) => None,
            })
            .collect()
    }

    /// `echo hi |> 'f'; read 'g'` is one call, and the redirect writes at its
    /// seam.  Landed eagerly the write would stand between the call and the read
    /// it had yet to make; buffered, it flushes behind every read of that call,
    /// so the barrier closes the run rather than splitting it.
    #[test]
    fn a_write_flushes_behind_the_reads_it_arrived_among() {
        let (emit, rx) = emitter();
        let mut buf = SurfaceBuffer::new();
        buf.absorb_observation(&emit, 0, write("f")).unwrap();
        buf.absorb_observation(&emit, 0, read("g")).unwrap();
        buf.flush_surfaces(&emit).unwrap();

        let commits: Vec<Display> = drain_display(&rx);
        let holds = |d: &Display, needle: &str| {
            matches!(d, Display::Observation { value } if format!("{value:?}").contains(needle))
        };
        let at = |needle: &str| {
            commits
                .iter()
                .position(|c| holds(c, needle))
                .unwrap_or_else(|| panic!("no commit holding {needle:?}: {commits:?}"))
        };
        assert!(
            at("g") < at("f"),
            "the read the write arrived before still records ahead of it: {commits:?}"
        );
    }
}
