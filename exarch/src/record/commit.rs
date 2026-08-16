//! The commit producer: the worker-side coalescers that decide what the
//! display half of the log records, upstream of the seam — so what is
//! recorded is what the user saw, and a resumed scrollback rebuilds it.
//!
//! Two producers live here.  [`Stream`] is the model's: one [`Chopper`] per
//! lane, each cutting its deltas into records at the last newline it holds
//! and flushing the tail at the step boundary, so the screen shows the text
//! as it arrives.  Where a cut falls carries no meaning — the view fold
//! joins consecutive records of one lane back into a single block — which is
//! why the rule is a newline and not a paragraph.  The reasoning lane
//! flushes where the prose after it begins, so a `∴` never lands mid-answer.
//! [`SurfaceBuffer`] is the shell's (moved here whole
//! from `tui/surface.rs` by `git mv` and re-pointed at the seam): it coalesces
//! consecutive same-`(id, path)` diff hunks into one `▎ diff`
//! [`Display::Card`], and dedupes read/exec/grep observations by kind —
//! a deduped bucket of several records as one [`Display::ObservationGroup`],
//! a singleton as its own [`Display::Observation`], each carried as its total
//! wire form (`observation_wire`), never the mark tree a live rail drew.
//! Both buffers are session-keyed, so a session change flushes and reopens
//! rather than merging two sessions into one commit.
//!
//! A write rides the observation buffer too, though it joins no group and is
//! never deduped — two writes to one path are two facts.  It is a barrier —
//! it ends the ral run before it — but it is still an effect of the call that
//! issued it, and a redirect writes at the seam, mid-call.  Landed eagerly it
//! would split a call from the reads it had yet to make; buffered, it flushes
//! *behind* them, so a call's effects stay contiguous and the coalescing
//! projection never has to reunite a call with effects stranded past a
//! barrier.

use std::io;

use crate::bus::AgentId;
use crate::bus::card::{Card, Hunk, Mark, observation_wire};
use crate::provider::Delta;
use crate::record::{Display, Emitter};
use ral_core::types::{Observation, Observed};

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

    /// The step's whole prose — what it settles into as an `Outcome`.  The
    /// chopper already holds every delta, so the driver keeps no second
    /// accumulator of the same stream.
    pub(crate) fn said(&self) -> &str {
        &self.prose.open
    }
}

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
        let _recorded = emitter.emit(Display::Card { marks })?;
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
                let dup = buf
                    .execs
                    .iter()
                    .any(|o| matches!(&o.what, Observed::Command { argv: a, .. } if a == argv));
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

    /// Record the buffered observations in the barrier order the module doc
    /// names — reads, execs, greps, then every write, last and undeduped.  A
    /// deduped bucket of several records as one [`Display::ObservationGroup`],
    /// rebuilt by the view fold into exactly the one grouped card the user
    /// saw; a singleton as its own [`Display::Observation`]; each write as its
    /// own commit, two writes being two facts.
    ///
    /// # Errors
    /// Propagates the first failed emit; later observations in the same
    /// flush are left unrecorded rather than recorded out of order.
    fn flush_observations(&mut self, emitter: &Emitter) -> io::Result<()> {
        let Some(buf) = self.observation_buf.take() else {
            return Ok(());
        };
        for bucket in [&buf.reads, &buf.execs, &buf.greps] {
            flush_bucket(emitter, bucket)?;
        }
        for write in buf.writes {
            let value = observation_wire(&write);
            let _recorded = emitter.emit(Display::Observation { value })?;
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

fn flush_bucket(emitter: &Emitter, bucket: &[Observation]) -> io::Result<()> {
    match bucket {
        [] => {}
        [one] => {
            let value = observation_wire(one);
            let _recorded = emitter.emit(Display::Observation { value })?;
        }
        many => {
            let values = many.iter().map(observation_wire).collect();
            let _recorded = emitter.emit(Display::ObservationGroup { values })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{BusReceiver, UsageMeter, channel};
    use crate::record::{FleetSink, Record};
    use ral_core::syntax::ast::RedirectMode;
    use ral_core::types::{CallSite, WriteOutcome};

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

    fn drain_display(rx: &BusReceiver) -> Vec<Display> {
        crate::bus::drain_records(rx)
            .into_iter()
            .filter_map(|rec| match rec {
                Record::Display(d) => Some(d),
                Record::Protocol(_) | Record::Forensic(_) => None,
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
        let holds = |d: &Display, needle: &str| matches!(d, Display::Observation { value } if format!("{value:?}").contains(needle));
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

    /// A deduped run of several reads is one grouped commit — the one visual
    /// card the user saw — while a lone exec stays its own observation.
    #[test]
    fn a_deduped_bucket_records_grouped_and_a_singleton_alone() {
        let (emit, rx) = emitter();
        let mut buf = SurfaceBuffer::new();
        buf.absorb_observation(&emit, 0, read("a")).unwrap();
        buf.absorb_observation(&emit, 0, read("b")).unwrap();
        buf.absorb_observation(&emit, 0, read("a")).unwrap();
        buf.absorb_observation(&emit, 0, read("c")).unwrap();
        buf.flush_surfaces(&emit).unwrap();

        let commits = drain_display(&rx);
        match commits.as_slice() {
            [Display::ObservationGroup { values }] => {
                assert_eq!(values.len(), 3, "the repeated read deduped: {values:?}");
            }
            other => panic!("expected one grouped commit, got {other:?}"),
        }
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
