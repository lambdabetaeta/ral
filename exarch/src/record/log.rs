//! The io-door for `sessions/<n>/record.jsonl`: the only file handle in the
//! tree for this log.  `append` is reachable only from [`super::seam`], and
//! `read` only from [`super::replay`] — Rust cannot restrict a `pub` item to
//! one specific sibling module, so both are `pub(super)`, visible across
//! `record/` and nowhere past it; the narrower promise is a matter of review,
//! not the type system, exactly as the module map's own risk note admits.
//!
//! The fleet publisher lives inside the same mutex as the writer ([`Inner`]),
//! so a record can only be published while its append is held: no code path
//! reaches the sender without the writer, and channel order is log order by
//! construction.  The publisher is *attachable* rather than fixed at
//! construction because the log outlives any one bus — a session's log is
//! built before the first frontend and survives every per-exchange bus a
//! headless run mints — and an unattached (or dead-channel) publish is a
//! no-op on purpose: the record is already durable, and a consumer that was
//! not listening catches up from the file, never from the channel.

use super::{Entry, Record, Recorded, Seq, Stamp, Transient};
use crate::bootstrap::now_unix_ms;
use crate::bus::{AgentId, Signal, UsageMeter, WeakSender};
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;
use std::sync::Mutex;

/// Where a witnessed record goes beside the file: the fleet-wide channel a
/// frontend drains, tagged with the owning session's id, and the run's usage
/// meter — accounting follows the fact through the seam, so a display-muted
/// child on a dead channel still counts toward the run total.
pub(crate) struct FleetSink {
    pub(crate) id: AgentId,
    /// Weak on purpose: the log outlives any one bus and its facts are
    /// durable without one, so this handle must never hold a channel open or
    /// stall a drain's disconnect on a session object's lifetime.
    pub(crate) tx: WeakSender,
    pub(crate) meter: UsageMeter,
}

pub(crate) struct Log {
    inner: Mutex<Inner>,
}

struct Inner {
    /// `None` for a `--no-logs` session: facts still stamp and publish, they
    /// just have no durable form.
    writer: Option<BufWriter<File>>,
    sink: Option<FleetSink>,
    seq: u64,
    /// This process's own append cursor, tracked rather than re-derived from
    /// `Seek`, so a flush never has to double as a position query.
    pos: u64,
}

impl Log {
    /// Open `path` for a fresh record log, truncating any prior file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be created.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] creates the session's record.jsonl; output infra, not turn-time data I/O"
    )]
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self::over(Some(BufWriter::new(file)), 0, 0))
    }

    /// Reopen `path` for append — resume — seeding the sequence and cursor
    /// from the complete lines already on disk, so a resumed session's stamps
    /// continue the file's own numbering.  Creates the file when a pre-plan
    /// session has none.  The caller quarantines any torn tail first.
    ///
    /// The seeding scan streams: a resume counts the file's lines without ever
    /// holding the file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or reopened.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] reopens the session's record.jsonl for append on resume; output infra, not turn-time data I/O"
    )]
    pub(crate) fn append_to(path: &Path) -> io::Result<Self> {
        let (mut seq, mut pos) = (0u64, 0u64);
        match File::open(path) {
            Ok(file) => {
                let mut prior = BufReader::new(file);
                let mut line = Vec::new();
                loop {
                    line.clear();
                    let read = prior.read_until(b'\n', &mut line)? as u64;
                    if read == 0 {
                        break;
                    }
                    pos += read;
                    seq += u64::from(line.ends_with(b"\n"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::over(Some(BufWriter::new(file)), seq, pos))
    }

    /// A log with no file — `--no-logs` — that still stamps and publishes.
    pub(crate) fn none() -> Self {
        Self::over(None, 0, 0)
    }

    fn over(writer: Option<BufWriter<File>>, seq: u64, pos: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                writer,
                sink: None,
                seq,
                pos,
            }),
        }
    }

    /// Rotate onto a fresh segment — `Some(path)` a new file, `None` the
    /// mirror-only seam a `--no-logs` session keeps — restarting the sequence
    /// and cursor while leaving the attached sink in place.  The segment is
    /// the file's, never the session's: swapping the `Log` instead would
    /// strand the bus and every `Emitter` clone on the rotated-away file.
    ///
    /// `Seq`/`Stamp` ranges are per-segment, not per-session: this only stays
    /// sound because `rotate`'s one caller, `/clear`, resets every fold
    /// (model memo, view) in the same beat, so nothing straddling the old
    /// numbering survives to be confused by the new one starting at zero.
    ///
    /// # Errors
    /// Returns `Err` if the new file cannot be created, or the lock is
    /// poisoned; on either the old segment stays live.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] opens the session's next record.jsonl segment; output infra, not turn-time data I/O"
    )]
    pub(super) fn rotate(&self, path: Option<&Path>) -> io::Result<()> {
        let writer = path.map(File::create).transpose()?.map(BufWriter::new);
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("record log lock poisoned"))?;
        inner.writer = writer;
        inner.seq = 0;
        inner.pos = 0;
        drop(inner);
        Ok(())
    }

    /// Point this log's publisher at a live fleet channel.  Called wherever a
    /// session's seam meets a run's bus (attend, deliberate, a direct
    /// `run_shell`); re-attaching over a dead per-exchange channel is the
    /// ordinary way a headless session's next exchange comes back on air.
    pub(super) fn attach(&self, sink: FleetSink) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.sink = Some(sink);
    }

    /// Append `record`, then publish it on the attached channel before
    /// releasing the lock — the whole reason the sink lives in here rather
    /// than beside it.  Flushed per record (never `fsync`): process-crash
    /// durable, which is what lets a killed session resume, but not
    /// power-loss durable.
    pub(super) fn append(&self, record: Record) -> io::Result<Stamp> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("record log lock poisoned"))?;
        let entry = Entry {
            at_unix_ms: now_unix_ms(),
            record,
        };
        let mut line = serde_json::to_vec(&entry).map_err(io::Error::other)?;
        line.push(b'\n');
        let Entry { record, .. } = entry;
        let start = inner.pos;
        if let Some(writer) = inner.writer.as_mut() {
            writer.write_all(&line)?;
            writer.flush()?;
        }
        let end = start + line.len() as u64;
        inner.pos = end;
        inner.seq += 1;
        let stamp = Stamp::new(Seq::new(inner.seq), start..end);
        if let Some(sink) = &inner.sink {
            if let Record::Forensic(super::Forensic::UsageDelta { usage }) = &record {
                sink.meter.add(usage.into());
            }
            let recorded = Recorded::new(stamp.clone(), record);
            if sink
                .tx
                .send_signal(Signal::Fact(sink.id, recorded))
                .is_err()
            {
                // No live receiver — the record is already durable on disk,
                // which is the whole point: a pressured or absent consumer
                // catches up from the file, never from the channel.
            }
        }
        drop(inner);
        Ok(stamp)
    }

    /// Publish a transient that never touches the file, through the same
    /// mutex as [`Self::append`] so it interleaves with facts in one order.
    pub(super) fn publish_transient(&self, t: Transient) {
        let Ok(inner) = self.inner.lock() else {
            return;
        };
        if let Some(sink) = &inner.sink
            && sink.tx.send_signal(Signal::Transient(sink.id, t)).is_err()
        {
            // No live receiver; a transient has no durable form to catch up
            // from, so there is nothing else to do.
        }
    }

    /// Stream every record back, in file order, each stamped with the `Seq`
    /// and byte range it occupies — what [`super::replay`] folds.
    ///
    /// One line is in memory at a time, so replaying a session costs the size
    /// of its fold's memo and never the size of its log.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be opened; a line that fails to parse
    /// arrives as an `Err` item, leaving the fold to refuse the session.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] streams the session's record.jsonl for replay; output infra, not turn-time data I/O"
    )]
    pub(super) fn read(
        path: &Path,
    ) -> io::Result<impl Iterator<Item = io::Result<Recorded<Record>>>> {
        let mut lines = BufReader::new(File::open(path)?).split(b'\n');
        let (mut seq, mut pos) = (0u64, 0u64);
        Ok(std::iter::from_fn(move || {
            loop {
                let line = match lines.next()? {
                    Ok(line) => line,
                    Err(error) => return Some(Err(error)),
                };
                let start = pos;
                pos += line.len() as u64 + 1;
                if line.is_empty() {
                    continue;
                }
                seq += 1;
                return Some(
                    serde_json::from_slice::<Entry>(&line)
                        .map(|entry| {
                            Recorded::new(Stamp::new(Seq::new(seq), start..pos), entry.record)
                        })
                        .map_err(|error| {
                            io::Error::other(format!(
                                "line {seq} does not parse as the `Entry` envelope ({error}); a session recorded before this exarch's Entry-envelope change cannot be resumed — was this session started with an older exarch?"
                            ))
                        }),
                );
            }
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::Forensic;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "exarch-log-test-{name}-{}-{:?}.jsonl",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    #[test]
    fn a_record_round_trips_through_the_entry_envelope() {
        let path = temp_path("round-trip");
        let log = Log::create(&path).expect("temp record log");
        let record = Record::Forensic(Forensic::Error {
            text: "boom".into(),
        });
        let _stamp = log.append(record).expect("append");

        let back: Vec<_> = Log::read(&path).expect("read back").collect();
        assert_eq!(back.len(), 1);
        let recorded = back.into_iter().next().unwrap().expect("parses");
        assert!(matches!(
            recorded.into_value(),
            Record::Forensic(Forensic::Error { text }) if text == "boom"
        ));

        let bytes = std::fs::read(&path).unwrap();
        let line = String::from_utf8(bytes).unwrap();
        assert!(
            line.contains("\"at_unix_ms\""),
            "the line on disk must carry the Entry envelope: {line}"
        );
    }

    #[test]
    fn a_pre_envelope_line_is_refused_by_name() {
        let path = temp_path("pre-envelope");
        let record = Record::Forensic(Forensic::Error {
            text: "boom".into(),
        });
        let mut line = serde_json::to_vec(&record).unwrap();
        line.push(b'\n');
        std::fs::write(&path, &line).unwrap();

        let back: Vec<_> = Log::read(&path)
            .expect("the file itself reads back")
            .collect();
        assert_eq!(back.len(), 1);
        let error = back.into_iter().next().unwrap().expect_err("no envelope");
        let text = error.to_string();
        assert!(text.contains("Entry"), "{text}");
        assert!(text.contains("older exarch"), "{text}");
    }
}
