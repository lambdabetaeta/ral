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

use super::log::{FleetSink, Log};
use super::{Class, Recorded, Transient};
use std::io;
use std::path::Path;
use std::sync::Arc;

/// A cheap, cloneable handle onto one session's record log — the handle a
/// worker stamps its facts through.
#[derive(Clone)]
pub struct Emitter {
    log: Arc<Log>,
}

impl Emitter {
    /// Open `path` for a fresh record log.
    ///
    /// # Errors
    /// Returns `Err` if the log file cannot be created.
    pub fn create(path: &Path) -> io::Result<Self> {
        Ok(Self {
            log: Arc::new(Log::create(path)?),
        })
    }

    /// Reopen `path` for append — resume — with stamps continuing the file's
    /// own numbering.  The caller quarantines any torn tail first.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or reopened.
    pub fn append_to(path: &Path) -> io::Result<Self> {
        Ok(Self {
            log: Arc::new(Log::append_to(path)?),
        })
    }

    /// A seam with no file — `--no-logs` — that still stamps facts and
    /// publishes them to an attached bus, matching `Transcript::none`.
    pub fn none() -> Self {
        Self {
            log: Arc::new(Log::none()),
        }
    }

    /// Point this session's log at a live fleet channel; facts append and
    /// publish under one lock from then on.  Idempotent, and re-attaching
    /// after a per-exchange bus died is the ordinary path back on air.
    pub(crate) fn attach(&self, sink: FleetSink) {
        self.log.attach(sink);
    }

    /// Append `value` and publish it, in that order, under one lock.
    ///
    /// # Errors
    /// Propagates a failed append: a write to the one authoritative log is a
    /// session error, never a shrug.
    pub fn emit<C: Class>(&self, value: C) -> io::Result<Recorded<C>> {
        let stamp = self.log.append(value.clone().into())?;
        Ok(Recorded::new(stamp, value))
    }

    /// Publish a transient — a delta, the thinking seat, chrome — with no
    /// durable form and no sequence number of its own.
    pub fn transient(&self, t: Transient) {
        self.log.publish_transient(t);
    }

    /// Report a record the seam could not append — a failed display commit,
    /// today's one caller — as a [`Transient::Fault`] instead of the record
    /// it could not become: the log itself is what just failed, so the
    /// failure cannot go through it.  Also prints to stderr, mirroring
    /// `Agent::note_error`'s own last-resort fallback, so an unwritable log
    /// never loses the diagnostic outright even before a printer draws the
    /// chrome lane it lands in.
    pub fn report_fault(&self, error: &io::Error) {
        let text = format!("a display commit was not recorded in record.jsonl: {error}");
        self.transient(Transient::Fault { text: text.clone() });
        eprintln!("exarch: {text}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Signal, UsageMeter, channel};

    #[test]
    fn a_seam_fault_reaches_the_screen_as_a_transient() {
        let path = std::env::temp_dir().join(format!(
            "exarch-seam-fault-test-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ));
        let emit = Emitter::create(&path).expect("temp record log");
        let (tx, rx) = channel();
        emit.attach(FleetSink {
            id: 7,
            tx: tx.downgrade(),
            meter: UsageMeter::default(),
        });

        emit.report_fault(&io::Error::other("disk is full"));

        match rx.recv().expect("the fault publishes") {
            Signal::Transient(id, Transient::Fault { text }) => {
                assert_eq!(id, 7);
                assert!(text.contains("disk is full"), "{text}");
            }
            Signal::Event(_) | Signal::Fact(..) | Signal::Transient(..) => {
                panic!("expected a Transient::Fault")
            }
        }
    }
}
