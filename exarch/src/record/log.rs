//! The syscall site for `sessions/<n>/record.jsonl`: the only file handle in the
//! tree for this log.  `append` is reachable only from [`super::seam`], and
//! `read` only from [`super::replay()`] and [`Log::since`] — Rust cannot
//! restrict a `pub` item to one specific sibling module, so both are
//! `pub(super)`, visible across `record/` and nowhere past it; the narrower
//! promise is a matter of review, not the type system, exactly as the module
//! map's own risk note admits.
//!
//! The fleet publisher lives inside the same mutex as the writer ([`Inner`]),
//! so a record can only be published while its append is held: no code path
//! reaches the sender without the writer, and channel order is log order by
//! construction.  The publisher is *attachable* rather than fixed at
//! construction because the log outlives any one bus — a session's log is
//! built before the first frontend and survives every per-exchange bus a
//! headless run mints.
//!
//! The channel is a fast path, never the record: a record appended with no
//! sink attached, or onto a dead channel, is published nowhere.  A consumer
//! that mirrors the log catches up from the file instead ([`Log::since`]).

use super::{Cursor, Entry, Locus, Publish, Record, Recorded, Seq, Transient};
use crate::app::now_unix_ms;
use std::fs::{File, OpenOptions};
use std::io::{self, BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `inner` is outside the workspace's poison door ([`ral_core::sync::LockExt`])
/// on purpose: `seq` and `pos` are the file's own position, restated in memory,
/// and [`Self::append`] advances them only after the bytes are written. A panic
/// between the write and the restatement leaves every later `Locus` naming a
/// byte range that is not the record it claims, and replay reads the wrong
/// bytes. A poisoned log must stay poisoned.
pub(crate) struct Log {
    inner: Mutex<Inner>,
}

struct Inner {
    /// The log's file and its writer.  `None` for the store-less log tests
    /// build: facts still stamp and publish, they just have no durable form.
    writer: Option<(PathBuf, BufWriter<File>)>,
    sink: Option<Box<dyn Publish>>,
    seq: u64,
    /// This process's own append cursor, tracked rather than re-derived from
    /// `Seek`, so a flush never has to double as a position query.
    pos: u64,
}

/// `complete_len` scans backwards a window at a time.
const SCAN_WINDOW: u64 = 8 * 1024;

impl Log {
    /// Open `path` for a fresh record log, truncating any prior file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be created.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:record-file-create] creates the session's record.jsonl; output infra, not turn-time data I/O"
    )]
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        Ok(Self::over(
            Some((path.to_path_buf(), BufWriter::new(File::create(path)?))),
            0,
            0,
        ))
    }

    /// Reopen `path` for append — resume — seeding the sequence and cursor
    /// from the complete lines already on disk, so a resumed session's loci
    /// continue the file's own numbering.  Creates the file when there is
    /// none.  The caller quarantines any torn tail first.
    ///
    /// The seeding scan streams: a resume counts the file's lines without ever
    /// holding the file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or reopened.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:record-file-append] reopens the session's record.jsonl for append on resume; output infra, not turn-time data I/O"
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
                    // `read`'s own rule: a terminated line that is not empty.
                    seq += u64::from(line.len() > 1 && line.ends_with(b"\n"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::over(
            Some((path.to_path_buf(), BufWriter::new(file))),
            seq,
            pos,
        ))
    }

    /// A log with no file, for tests: it still stamps and publishes.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self::over(None, 0, 0)
    }

    fn over(writer: Option<(PathBuf, BufWriter<File>)>, seq: u64, pos: u64) -> Self {
        Self {
            inner: Mutex::new(Inner {
                writer,
                sink: None,
                seq,
                pos,
            }),
        }
    }

    /// Point this log's publisher at a fleet channel.  Called wherever a
    /// session's seam meets a run's bus (attend, deliberate, a direct
    /// `Avatar::ral`); re-attaching over a dead per-exchange channel is the
    /// ordinary way a headless session's next exchange comes back on air.
    #[allow(clippy::disallowed_methods, reason = "see [`Log`]")]
    pub(super) fn attach(&self, sink: Box<dyn Publish>) {
        if let Ok(mut inner) = self.inner.lock() {
            inner.sink = Some(sink);
        }
    }

    /// Where `after` stands — at the head when none — and the records past
    /// it.  The lock fixes only the log's extent and a handle on its file:
    /// whatever lands later reaches the channel, so a consumer that reads this
    /// and then drains misses nothing.
    ///
    /// # Errors
    /// The lock is poisoned or the file will not open; a line that will not
    /// parse arrives as an `Err` item.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:record-file-read] streams the session's record.jsonl for a mirror's catch-up; output infra, not turn-time data I/O"
    )]
    pub(super) fn since(
        &self,
        after: Option<Cursor>,
    ) -> io::Result<(
        Cursor,
        impl Iterator<Item = io::Result<Recorded<Record>>> + use<>,
    )> {
        let inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("record log lock poisoned"))?;
        let from = after.unwrap_or_else(Cursor::start);
        let source = inner
            .writer
            .as_ref()
            .map(|(path, _)| File::open(path).map(|file| (path.clone(), file)))
            .transpose()?;
        let until = inner.pos;
        drop(inner);
        Ok((from, Self::records(source, from.seq, from.end, until)?))
    }

    /// Append `record`, then publish it on the attached channel before
    /// releasing the lock — the whole reason the sink lives in here rather
    /// than beside it.  Flushed per record (never `fsync`): process-crash
    /// durable, which is what lets a killed session resume, but not
    /// power-loss durable.
    #[allow(clippy::disallowed_methods, reason = "see [`Log`]")]
    pub(super) fn append(&self, record: &Record) -> io::Result<Locus> {
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("record log lock poisoned"))?;
        let mut line = serde_json::to_vec(&Entry {
            at_unix_ms: now_unix_ms(),
            record,
        })
        .map_err(io::Error::other)?;
        line.push(b'\n');
        let start = inner.pos;
        if let Some((_, writer)) = inner.writer.as_mut() {
            writer.write_all(&line)?;
            writer.flush()?;
        }
        inner.pos = start + line.len() as u64;
        inner.seq += 1;
        let locus = Locus::over(Seq::new(inner.seq), start, &line[..line.len() - 1]);
        if let Some(sink) = &inner.sink {
            sink.fact(Recorded::new(locus.clone(), record.clone()));
        }
        drop(inner);
        Ok(locus)
    }

    /// Publish a transient that never touches the file, through the same
    /// mutex as [`Self::append`] so it interleaves with facts in one order.
    #[allow(clippy::disallowed_methods, reason = "see [`Log`]")]
    pub(super) fn publish_transient(&self, t: Transient) {
        let Ok(inner) = self.inner.lock() else {
            return;
        };
        if let Some(sink) = &inner.sink {
            sink.transient(t);
        }
    }

    /// The length of `path` through its last newline: what follows is a torn
    /// write, never a record.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:record-complete-len] scans record.jsonl's tail for the last complete line; output infra, not turn-time data I/O"
    )]
    pub(super) fn complete_len(path: &Path) -> io::Result<u64> {
        let mut file = File::open(path)?;
        let mut end = file.metadata()?.len();
        while end > 0 {
            let start = end.saturating_sub(SCAN_WINDOW);
            let mut chunk = vec![0; usize::try_from(end - start).unwrap_or(usize::MAX)];
            let _ = file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut chunk)?;
            if let Some(last) = chunk.iter().rposition(|byte| *byte == b'\n') {
                return Ok(start + last as u64 + 1);
            }
            end = start;
        }
        Ok(0)
    }

    /// Stream every record back, in file order, each located by the `Seq`
    /// and byte range it occupies — what [`super::replay()`] folds.  An
    /// unterminated tail is a torn write and is never parsed.
    ///
    /// One line is in memory at a time, so replaying a session costs the size
    /// of its fold's memo and never the size of its log.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be opened; a line that fails to parse
    /// arrives as an `Err` item, leaving the fold to refuse the session.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:record-replay-read] streams the session's record.jsonl for replay; output infra, not turn-time data I/O"
    )]
    pub(super) fn read(
        path: &Path,
    ) -> io::Result<impl Iterator<Item = io::Result<Recorded<Record>>> + use<>> {
        let until = Self::complete_len(path)?;
        Self::records(Some((path.to_path_buf(), File::open(path)?)), 0, 0, until)
    }

    /// The records of `source` from byte `pos`, where record `seq` ended, up to
    /// byte `until`; none without a file.
    fn records(
        source: Option<(PathBuf, File)>,
        mut seq: u64,
        mut pos: u64,
        until: u64,
    ) -> io::Result<impl Iterator<Item = io::Result<Recorded<Record>>> + use<>> {
        let mut lines = source
            .map(|(path, mut file)| {
                let _at = file.seek(SeekFrom::Start(pos))?;
                Ok::<_, io::Error>((
                    path,
                    BufReader::new(file.take(until.saturating_sub(pos))).split(b'\n'),
                ))
            })
            .transpose()?;
        Ok(std::iter::from_fn(move || {
            loop {
                let (path, lines) = lines.as_mut()?;
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
                    serde_json::from_slice::<Entry<Record>>(&line)
                        .map(|entry| {
                            Recorded::new(Locus::over(Seq::new(seq), start, &line), entry.record)
                        })
                        .map_err(|error| {
                            io::Error::other(format!(
                                "line {seq} of {} is not a record this exarch writes ({error}); was this session recorded by an older exarch? Older sessions can be neither resumed nor exported.",
                                ral_core::path::shown(path)
                            ))
                        }),
                );
            }
        }))
    }
}

#[cfg(test)]
mod tests;
