//! The emit seam: [`Emitter::emit`] is the only publisher, and the only way
//! to mint a [`Recorded`] from a live fact — append-then-publish as one
//! critical section inside [`Log`]'s own mutex, so the channel a printer
//! drains can never run ahead of the file a resume replays.
//!
//! The queue is unbounded on purpose: a bounded one would couple the seam's
//! availability to how fast a terminal drains, which is the dependency the
//! whole plan exists to forbid.  A pressured consumer's escape is the log
//! itself, since every fact carries a sequence number.

use super::log::Log;
use super::{Class, Record, Recorded, Transient};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc::Receiver;

/// What the channel carries: a witnessed fact, or a transient that never
/// touches the log.  The appender only ever accepts a [`Record`], so handing
/// it a [`Transient`] — journaling a delta — is a type error, not a runtime
/// check.
pub enum Published {
    Fact(Recorded<Record>),
    Transient(Transient),
}

/// A cheap, cloneable handle onto one session's record log — the handle a
/// worker stamps its facts through.
#[derive(Clone)]
pub struct Emitter {
    log: Arc<Log>,
}

impl Emitter {
    /// Open `path` for a fresh record log, returning the emitter and the
    /// receiver its printers and folds drain.
    ///
    /// # Errors
    /// Returns `Err` if the log file cannot be created.
    pub fn create(path: &Path) -> io::Result<(Self, Receiver<Published>)> {
        let (log, rx) = Log::create(path)?;
        Ok((Self { log: Arc::new(log) }, rx))
    }

    /// Append `value` and publish it, in that order, under one lock.
    ///
    /// # Errors
    /// Propagates a failed append: a write to the one authoritative log is a
    /// session error, never a shrug.
    pub fn emit<C: Class>(&self, value: C) -> io::Result<Recorded<C>> {
        let record: Record = value.clone().into();
        let stamp = self.log.append(record)?;
        Ok(Recorded::new(stamp, value))
    }

    /// Publish a transient — a delta, the thinking seat, chrome — with no
    /// durable form and no sequence number of its own.
    pub fn transient(&self, t: Transient) {
        self.log.publish_transient(t);
    }
}
