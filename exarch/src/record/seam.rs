//! The emit seam: [`Emitter::emit`] is the only publisher, and the only way
//! to mint a [`Recorded`] from a live fact — append-then-publish as one
//! critical section inside [`Log`]'s own mutex, so the channel a printer
//! drains can never run ahead of the file a resume replays.
//!
//! The channel is the fleet-wide bus, unbounded for facts on purpose: a
//! bounded queue would couple the seam's availability to how fast a terminal
//! drains, which is the dependency the whole plan exists to forbid.  A
//! pressured consumer's escape is the log itself, since every fact carries a
//! sequence number.

use super::log::Log;
use super::{Cursor, Publish, Record, Recorded, Transient};
use ral_core::own::OwnDir;
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
    /// Returns `Err` if the file cannot be read or reopened.
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
    /// after a per-exchange bus died is the ordinary path back on air.
    pub(crate) fn attach(&self, sink: Box<dyn Publish>) {
        self.log.attach(sink);
    }

    /// A mirror's catch-up: where `after` stands, and the records past it.
    /// See [`super::log::Log::since`].
    ///
    /// # Errors
    /// The log will not open.
    pub(crate) fn since(
        &self,
        after: Option<Cursor>,
    ) -> io::Result<(
        Cursor,
        impl Iterator<Item = io::Result<Recorded<Record>>> + use<>,
    )> {
        self.log.since(after)
    }

    /// Observe this shared writer's availability without taking the append lock.
    /// An observation reserves nothing; append checks again under the lock.
    ///
    /// # Errors
    /// Returns the terminal write failure, or a poisoned append-lock error.
    pub fn check_available(&self) -> io::Result<()> {
        self.log.check_available()
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

    /// Append `record` and publish it, in that order, under one lock.
    ///
    /// # Errors
    /// Propagates a failed append: a write to the one authoritative log is a
    /// session error, never a shrug.
    pub fn emit(&self, record: impl Into<Record>) -> io::Result<Recorded<Record>> {
        let record = record.into();
        Ok(Recorded::new(self.log.append(&record)?, record))
    }

    /// [`Self::emit`] where a failed append has nowhere to go but
    /// [`Self::report_fault`].
    pub fn emit_or_report(&self, record: impl Into<Record>) {
        if let Err(error) = self.emit(record) {
            self.report_fault(&error);
        }
    }

    /// Publish a transient — a delta, the open thinking line, chrome — with no
    /// durable form and no sequence number of its own.
    pub fn transient(&self, t: Transient) {
        self.log.publish_transient(t);
    }

    /// Report a record that could not be appended as a [`Transient::Fault`]
    /// instead of the record it could not become: the log itself is what
    /// just failed, so the failure cannot go through it.  Also prints to
    /// stderr — the one last resort in the tree — so an unwritable log never
    /// loses the diagnostic outright even before a printer draws the chrome
    /// lane it lands in.
    pub fn report_fault(&self, error: &io::Error) {
        Self::report_fault_with(error, |fault| self.transient(fault));
    }

    /// Terminal owners retain a publisher independent of the append lock.
    pub(crate) fn report_fault_with(error: &io::Error, publish: impl FnOnce(Transient)) {
        let text = format!("a record was not appended to record.jsonl: {error}");
        publish(Transient::Fault { text: text.clone() });
        ral_core::errln!("exarch: {text}");
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
