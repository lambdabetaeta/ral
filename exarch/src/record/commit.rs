//! The commit producer: the stream chopper that decides where the model's
//! own text enters the display half of the log, upstream of the seam — so
//! what is recorded is what the user saw, and a resumed scrollback rebuilds
//! it.
//!
//! [`Stream`] holds one [`Chopper`] per lane, each cutting its deltas into
//! records at the last newline it holds and flushing the tail at the turn
//! boundary, so the screen shows the text as it arrives.  Where a cut falls
//! carries no meaning — the view fold joins consecutive records of one lane
//! back into a single block — which is why the rule is a newline and not a
//! paragraph.  The reasoning lane flushes where the prose after it begins, so
//! a `∴` never lands mid-answer.

use std::io;

use crate::provider::Delta;
use crate::record::{Display, Emitter};

/// Which lane's block a chopper's records grow.
#[derive(Clone, Copy)]
enum Lane {
    Answer,
    Thinking,
}

/// One lane of the model's stream, cut into records at the last newline it
/// holds.  `open` is the lane's whole text and `committed` the byte index just
/// past the prefix already recorded, so the lane stays readable in full while
/// its lines record one after another.
///
/// The cut is a newline and nothing more: [`Blocks`](crate::record::Blocks)
/// grows the lane's block by every record that continues it, so a fence, a
/// paragraph, or a sentence is never divided in the block a reader sees.
struct Chopper {
    lane: Lane,
    open: String,
    committed: usize,
}

impl Chopper {
    /// Accumulate one delta, recording every whole line it completes.
    ///
    /// # Errors
    /// Propagates a failed record of those lines.
    fn push(&mut self, emitter: &Emitter, delta: &str) -> io::Result<()> {
        self.open.push_str(delta);
        match self.open[self.committed..].rfind('\n') {
            Some(nl) => self.commit(emitter, self.committed + nl + 1),
            None => Ok(()),
        }
    }

    /// Record whatever tail remains — the lane's end, where the prose after a
    /// reasoning run begins, or the step's boundary.  Idempotent: a flushed
    /// lane has no tail until its next delta.
    ///
    /// # Errors
    /// Propagates a failed record of the tail.
    fn flush(&mut self, emitter: &Emitter) -> io::Result<()> {
        self.commit(emitter, self.open.len())
    }

    /// Record `open[committed..end]` and advance past it.  Text that is
    /// whitespace alone waits instead: it costs nothing to carry, and it
    /// would otherwise open a block holding no words.
    ///
    /// # Errors
    /// Propagates a failed record.
    fn commit(&mut self, emitter: &Emitter, end: usize) -> io::Result<()> {
        if self.open[self.committed..end].trim().is_empty() {
            return Ok(());
        }
        let text = self.open[self.committed..end].to_string();
        self.committed = end;
        let (record, what) = match self.lane {
            Lane::Answer => (Display::Answer { text }, "an answer"),
            Lane::Thinking => (Display::Thinking { text }, "the step's reasoning"),
        };
        let _recorded = emitter.emit(record).map_err(|e| unrecorded(what, &e))?;
        Ok(())
    }
}

/// A failed commit, named by what it was: the producers are the only place
/// that knows, and their caller hands the message straight to the user.
fn unrecorded(what: &str, error: &io::Error) -> io::Error {
    io::Error::other(format!("{what} was not recorded in record.jsonl: {error}"))
}

/// The model's stream, recorded: one [`Chopper`] per lane under one roof,
/// since the two share one order and only a producer holding both can see
/// the seam between them.
///
/// The order is the whole point.  A reasoning run is complete the moment the
/// first prose delta after it arrives — that is where its tail records,
/// ahead of the answer it deliberated into, so a `∴` never lands mid-answer.
/// A run no prose follows flushes at the boundary.
///
/// A streaming callback has no error channel of its own, so the first failed
/// record is stashed and answered at [`Self::seal`]; nothing records after
/// it, a half-ordered scrollback being worse than a short one.
pub(crate) struct Stream {
    prose: Chopper,
    trace: Chopper,
}

impl Default for Stream {
    fn default() -> Self {
        Self {
            prose: Chopper {
                lane: Lane::Answer,
                open: String::new(),
                committed: 0,
            },
            trace: Chopper {
                lane: Lane::Thinking,
                open: String::new(),
                committed: 0,
            },
        }
    }
}

impl Stream {
    /// Absorb one delta into its lane.  Prose closes the reasoning lane
    /// first, which is where a run ends.
    ///
    /// # Errors
    /// Propagates a failed record of either lane.
    pub(crate) fn push(&mut self, emitter: &Emitter, delta: Delta<'_>) -> io::Result<()> {
        match delta {
            Delta::Think(run) => self.trace.push(emitter, run),
            Delta::Say(text) => self
                .trace
                .flush(emitter)
                .and_then(|()| self.prose.push(emitter, text)),
        }
    }

    /// Seal the step: the reasoning tail no prose followed, then the prose
    /// tail — the same two lanes in the same order a prose delta takes them,
    /// at whichever boundary ends the stream, completed or stalled or
    /// cancelled.
    ///
    /// # Errors
    /// Propagates the failed record of either.
    pub(crate) fn seal(&mut self, emitter: &Emitter) -> io::Result<()> {
        self.trace
            .flush(emitter)
            .and_then(|()| self.prose.flush(emitter))
    }
}

#[cfg(test)]
mod tests;
