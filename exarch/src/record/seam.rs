//! The emit seam: [`Emitter::commit`] is the only publisher; [`Emitter::emit`]
//! is a commit of one record. Append-then-publish is one
//! critical section inside [`Log`]'s own mutex, so the channel a printer
//! drains can never run ahead of the file a resume replays.
//!
//! The channel is the fleet-wide bus, unbounded for facts on purpose: a
//! bounded queue would couple the seam's availability to how fast a terminal
//! drains, which is the dependency the whole plan exists to forbid.  A
//! pressured consumer's escape is the log itself, since every fact carries a
//! sequence number.

use super::log::Log;
use super::{
    Commit, Control, Cursor, Folds, Forensic, Mail, Model, NonEmpty, Publish, Record, Recorded,
    Schedules, Started, Transient, Unavailable,
};
use ral_core::own::OwnDir;
use ral_core::process::ScopeNode;
use std::fmt;
use std::io;
use std::sync::Arc;

/// A cheap, cloneable handle onto one session's record log — the handle a
/// worker stamps its facts through.
#[derive(Clone)]
pub struct Emitter {
    log: Arc<Log>,
}

impl fmt::Debug for Emitter {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Emitter").finish_non_exhaustive()
    }
}

/// A handle on one session's log that can catch up but never append.
#[derive(Clone)]
pub struct Reader {
    log: Arc<Log>,
}

impl fmt::Debug for Reader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Reader").finish_non_exhaustive()
    }
}

impl Reader {
    /// A mirror's catch-up: where `after` stands, and the commits past it.
    /// See [`super::log::Log::since`].
    ///
    /// # Errors
    /// The log will not open.
    pub(crate) fn since(
        &self,
        after: Option<Cursor>,
    ) -> io::Result<(Cursor, impl Iterator<Item = io::Result<Commit>> + use<>)> {
        self.log.since(after)
    }
}

impl Emitter {
    /// Start a fresh record log in the session directory `dir`.
    ///
    /// # Errors
    /// Returns `Err` if the log file cannot be created.
    pub fn create(dir: &OwnDir) -> io::Result<Self> {
        Ok(Self {
            log: Arc::new(Log::create(dir)?),
        })
    }

    /// Reopen the record in `dir` for append — resume — with stamps continuing
    /// the file's own numbering.  The caller quarantines any torn tail first.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or reopened, or its tail is torn
    /// rather than a whole commit.
    pub fn append_to(dir: &OwnDir) -> io::Result<Self> {
        Ok(Self {
            log: Arc::new(Log::append_to(dir)?),
        })
    }

    /// A recorder that publishes nowhere, for tests: it still stamps facts and
    /// hands them to an attached bus, but keeps no store. Not a session mode —
    /// every real session has a `record.jsonl`.
    #[cfg(test)]
    pub fn none() -> Self {
        Self {
            log: Arc::new(Log::none()),
        }
    }

    /// Point this session's log at a [`Publish`]er; facts append and
    /// publish under one lock from then on.  Idempotent, and re-attaching
    /// after its bus died is the ordinary path back on air.
    pub(crate) fn attach(&self, sink: Box<dyn Publish>) {
        self.log.attach(sink);
    }

    /// A read-only handle on this session's log.
    pub(crate) fn reader(&self) -> Reader {
        Reader {
            log: Arc::clone(&self.log),
        }
    }

    /// Observe without the append lock; a poisoned lock is latched here.
    ///
    /// # Errors
    /// Returns the writer's first failure.
    pub fn check_available(&self) -> Result<(), Unavailable> {
        self.log.check_available()
    }

    /// End `life` when this writer fails, at once if it has.
    pub(crate) fn bind(&self, life: ScopeNode) {
        self.log.bind(life);
    }

    /// Latch `cause` unless a failure stands, ending the bound life.
    pub(super) fn fail(&self, cause: io::Error) -> Unavailable {
        self.log.fail(cause)
    }

    /// Fail after writing `bytes` more bytes to this writer's real file.
    #[cfg(test)]
    pub(crate) fn fail_after(&self, bytes: usize) {
        self.log.fail_after(bytes);
    }

    /// Panic while holding the actual append lock, without changing its state.
    #[cfg(test)]
    pub(crate) fn poison_append(&self) {
        self.log.poison_append();
    }

    /// Append `records` as one commit and publish it under one lock:
    /// the log holds all of them or none.
    ///
    /// # Errors
    /// Propagates a failed append, which latches the writer.
    /// # Panics
    /// A hand-built `Record::Control` must go through `commit_to` or `begin`.
    pub fn commit(&self, records: NonEmpty<Record>) -> Result<Commit, Unavailable> {
        assert!(
            !records.iter().any(Record::is_control),
            "Control records must go through commit_to or begin"
        );
        self.log.append(records)
    }

    /// Append before stepping: a failed append leaves every fold unchanged.
    /// # Errors
    /// The append failed and latched the writer.
    /// # Panics
    /// A fold refuses a live record, which is an authorship bug.
    pub(crate) fn commit_to(
        &self,
        mut folds: Folds<'_>,
        line: NonEmpty<Control>,
        tail: Vec<Forensic>,
    ) -> Result<Commit, Unavailable> {
        let records = NonEmpty {
            first: Record::Control(line.first),
            rest: line
                .rest
                .into_iter()
                .map(Record::Control)
                .chain(tail.into_iter().map(Record::Forensic))
                .collect(),
        };
        let commit = self.log.append(records)?;
        for record in commit.records() {
            folds
                .step(record)
                .unwrap_or_else(|refusal| panic!("control fold refused a live record: {refusal}"));
        }
        Ok(commit)
    }

    /// Append the head and return the folds of that one-line log.
    /// # Errors
    /// The append failed and latched the writer.
    pub(crate) fn begin(&self, started: Started) -> Result<(Mail, Schedules, Model), Unavailable> {
        let folds = super::control::seed(&started);
        drop(
            self.log
                .append(NonEmpty::one(Record::Control(Control::SessionStarted(
                    started,
                ))))?,
        );
        Ok(folds)
    }

    /// End the bound life when its cell can no longer be trusted.
    pub(crate) fn cell_poisoned(&self, why: String) -> Unavailable {
        self.log.cell_poisoned(why)
    }

    /// [`Self::commit`] of one record.
    ///
    /// # Errors
    /// Propagates a failed append, which latches the writer.
    pub fn emit(&self, record: impl Into<Record>) -> Result<Recorded<Record>, Unavailable> {
        Ok(self
            .commit(NonEmpty::one(record.into()))?
            .into_records()
            .first)
    }

    /// [`Self::emit`] where a failure needs no answer here: it has latched the
    /// writer and ended the life bound to it, whose owner announces it once.
    pub fn emit_or_end(&self, record: impl Into<Record>) {
        drop(self.emit(record));
    }

    /// Publish a transient — a delta, the open thinking line, chrome — with no
    /// durable form and no sequence number of its own.
    pub fn transient(&self, t: Transient) {
        self.log.publish_transient(t);
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
