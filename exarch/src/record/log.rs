//! The io-door for `sessions/<n>/record.jsonl`: the only file handle in the
//! tree for this log.  `append` is reachable only from [`super::seam`], and
//! `read` only from [`super::replay`] — Rust cannot restrict a `pub` item to
//! one specific sibling module, so both are `pub(super)`, visible across
//! `record/` and nowhere past it; the narrower promise is a matter of review,
//! not the type system, exactly as the module map's own risk note admits.
//!
//! The channel `Sender` lives inside the same mutex as the writer
//! ([`Inner`]), so a record can only be published while its append is held:
//! no code path reaches the sender without the writer, and channel order is
//! log order by construction.

use super::seam::Published;
use super::{Record, Recorded, Seq, Stamp};
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

pub(crate) struct Log {
    inner: Mutex<Inner>,
}

struct Inner {
    writer: BufWriter<File>,
    tx: Sender<Published>,
    seq: u64,
    /// This process's own append cursor, tracked rather than re-derived from
    /// `Seek`, so a flush never has to double as a position query.
    pos: u64,
}

impl Log {
    /// Open `path` for this session's record log, truncating any prior file,
    /// and return the receiver its channel feeds.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be created.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] creates the session's record.jsonl; output infra, not turn-time data I/O"
    )]
    pub(crate) fn create(path: &Path) -> io::Result<(Self, Receiver<Published>)> {
        let file = File::create(path)?;
        let (tx, rx) = mpsc::channel();
        let log = Self {
            inner: Mutex::new(Inner {
                writer: BufWriter::new(file),
                tx,
                seq: 0,
                pos: 0,
            }),
        };
        Ok((log, rx))
    }

    /// Append `record`, then publish it on the channel before releasing the
    /// lock — the whole reason the sender lives in here rather than beside
    /// it.  Flushed per record (never `fsync`): process-crash durable, which
    /// is what lets a killed session resume, but not power-loss durable.
    pub(super) fn append(&self, record: Record) -> io::Result<Stamp> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("record log lock poisoned"))?;
        let mut line = serde_json::to_vec(&record).map_err(io::Error::other)?;
        line.push(b'\n');
        let start = inner.pos;
        inner.writer.write_all(&line)?;
        inner.writer.flush()?;
        let end = start + line.len() as u64;
        inner.pos = end;
        inner.seq += 1;
        let stamp = Stamp::new(Seq::new(inner.seq), start..end);
        let recorded = Recorded::new(stamp.clone(), record);
        if inner.tx.send(Published::Fact(recorded)).is_err() {
            // No live receiver — the record is already durable on disk,
            // which is the whole point: a pressured or absent consumer
            // catches up from the file, never from the channel.
        }
        drop(inner);
        Ok(stamp)
    }

    /// Publish a transient that never touches the file, through the same
    /// mutex as [`Self::append`] so it interleaves with facts in one order.
    pub(super) fn publish_transient(&self, t: super::Transient) {
        let Ok(inner) = self.inner.lock() else {
            return;
        };
        if inner.tx.send(Published::Transient(t)).is_err() {
            // No live receiver; a transient has no durable form to catch up
            // from, so there is nothing else to do.
        }
    }

    /// Read every record back, in file order, each stamped with the `Seq`
    /// and byte range it occupies — what [`super::replay`] folds.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or a line fails to parse.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] reads the session's record.jsonl whole for replay; the O(file) cost is an accepted loss, not a hot-path read"
    )]
    pub(super) fn read(path: &Path) -> io::Result<Vec<io::Result<Recorded<Record>>>> {
        let bytes = std::fs::read(path)?;
        let mut out = Vec::new();
        let mut pos: u64 = 0;
        let mut seq: u64 = 0;
        for line in bytes.split(|&b| b == b'\n') {
            if line.is_empty() {
                continue;
            }
            let start = pos;
            let end = start + line.len() as u64 + 1;
            pos = end;
            seq += 1;
            let parsed = serde_json::from_slice::<Record>(line)
                .map(|record| Recorded::new(Stamp::new(Seq::new(seq), start..end), record))
                .map_err(io::Error::other);
            out.push(parsed);
        }
        Ok(out)
    }
}
