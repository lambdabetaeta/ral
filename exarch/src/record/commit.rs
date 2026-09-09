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
mod tests {
    use super::*;
    use crate::bus::{BusReceiver, UsageMeter, channel};
    use crate::record::{FleetSink, Record};

    fn emitter() -> (Emitter, BusReceiver) {
        let path = std::env::temp_dir().join(format!(
            "exarch-commit-test-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let emit = Emitter::create(&path).expect("temp record log");
        let (tx, rx) = channel();
        emit.attach(FleetSink {
            id: 0,
            tx: tx.downgrade(),
            meter: UsageMeter::default(),
        });
        (emit, rx)
    }

    fn drain_display(rx: &BusReceiver) -> Vec<Display> {
        crate::bus::drain_records(rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(d) => Some(d),
                Record::Protocol(_) | Record::Forensic(_) => None,
            })
            .collect()
    }

    /// Reasoning precedes the answer inside a step, so its records precede
    /// every one of the answer's: the run flushes at the first prose delta,
    /// not at the step's end, which would strand it between the lines the
    /// chopper had already recorded and the tail it had not.
    #[test]
    fn a_reasoning_run_records_ahead_of_all_the_prose_that_follows_it() {
        let (emit, rx) = emitter();
        let mut stream = Stream::default();
        stream
            .push(&emit, Delta::Think("weighing the cases\n"))
            .unwrap();
        stream
            .push(&emit, Delta::Say("First paragraph.\n\n"))
            .unwrap();
        stream
            .push(&emit, Delta::Say("Second paragraph.\n\ntail"))
            .unwrap();
        stream.seal(&emit).unwrap();

        let commits = drain_display(&rx);
        match commits.as_slice() {
            [Display::Thinking { text }, rest @ ..] => {
                assert_eq!(text, "weighing the cases\n");
                assert!(
                    rest.iter().all(|c| matches!(c, Display::Answer { .. })),
                    "nothing but prose follows the run: {rest:?}"
                );
            }
            other => panic!("expected the reasoning run first, got {other:?}"),
        }
    }

    /// A step that reasons and then calls a tool without a word has no prose
    /// seam to flush at, so the boundary flushes it.
    #[test]
    fn a_wordless_step_seals_its_reasoning_at_the_boundary() {
        let (emit, rx) = emitter();
        let mut stream = Stream::default();
        stream
            .push(&emit, Delta::Think("straight to the shell\n"))
            .unwrap();
        stream.seal(&emit).unwrap();

        match drain_display(&rx).as_slice() {
            [Display::Thinking { text }] => assert_eq!(text, "straight to the shell\n"),
            other => panic!("expected the run alone, got {other:?}"),
        }
    }

    /// The records reassemble the stream exactly: a reader's block is every
    /// record of the lane joined, so nothing may be dropped or duplicated
    /// between two of them.
    #[test]
    fn the_records_reassemble_the_whole_stream() {
        let (emit, rx) = emitter();
        let mut stream = Stream::default();
        let deltas = ["first\n\n", "\n\n", "second\n\n", "tail"];
        for delta in deltas {
            stream.push(&emit, Delta::Say(delta)).unwrap();
        }
        stream.seal(&emit).unwrap();

        let recorded: String = drain_display(&rx)
            .into_iter()
            .map(|d| {
                if let Display::Answer { text } = d {
                    text
                } else {
                    panic!("expected only answer records, got {d:?}")
                }
            })
            .collect();
        assert_eq!(recorded, deltas.concat());
    }

    /// The cut is the last newline and nothing more: a line records the
    /// moment it completes, and the text still short of one waits.  A fence
    /// needs no special case, since the block a reader sees joins the records
    /// back together.
    #[test]
    fn a_line_records_where_it_completes_and_the_open_line_waits() {
        let (emit, rx) = emitter();
        let mut stream = Stream::default();
        stream.push(&emit, Delta::Say("```ral\nlet x = 1")).unwrap();
        match drain_display(&rx).as_slice() {
            [Display::Answer { text }] => assert_eq!(text, "```ral\n"),
            other => panic!("expected the completed line alone, got {other:?}"),
        }

        stream.push(&emit, Delta::Say("\n```\n")).unwrap();
        stream.seal(&emit).unwrap();
        match drain_display(&rx).as_slice() {
            [Display::Answer { text }] => assert_eq!(text, "let x = 1\n```\n"),
            other => panic!("expected both lines in one record, got {other:?}"),
        }
    }

    /// Whitespace alone opens no block: it waits for the words after it, so a
    /// reader never meets a block holding nothing.
    #[test]
    fn whitespace_alone_waits_for_the_words_after_it() {
        let (emit, rx) = emitter();
        let mut stream = Stream::default();
        stream.push(&emit, Delta::Say("\n\n")).unwrap();
        assert!(
            drain_display(&rx).is_empty(),
            "blank lines alone are not worth a block"
        );

        stream.push(&emit, Delta::Say("a word\n")).unwrap();
        match drain_display(&rx).as_slice() {
            [Display::Answer { text }] => assert_eq!(text, "\n\na word\n"),
            other => panic!("expected the blank lines to ride along, got {other:?}"),
        }
    }
}
