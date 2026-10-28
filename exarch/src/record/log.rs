//! The handle on `sessions/<n>/record.jsonl`: the only writer in the tree for
//! this log.  `append` is reachable only from [`super::seam`], and
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

use super::{Cursor, Entry, Locus, Publish, RECORD, Record, Recorded, Seq, Transient};
use crate::app::now_unix_ms;
use ral_core::own::OwnDir;
use ral_core::process::{CancelCause, ScopeNode};
use ral_core::sync::{LockExt as _, Torn};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The first recording failure, latched for good and cheap to clone.
#[derive(Clone, Debug)]
pub struct Unavailable(Arc<Failure>);

#[derive(Debug)]
struct Failure {
    /// `None` for a fileless test log.
    record: Option<PathBuf>,
    cause: io::Error,
}

impl Unavailable {
    fn new(record: Option<PathBuf>, cause: io::Error) -> Self {
        Self(Arc::new(Failure { record, cause }))
    }

    pub fn kind(&self) -> io::ErrorKind {
        self.0.cause.kind()
    }
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0.record {
            Some(path) => write!(f, "recording to {} failed", ral_core::path::shown(path))?,
            None => f.write_str("recording failed")?,
        }
        write!(
            f,
            ": {}; this session has ended, since it can record nothing more. Its record keeps everything before the failure, so the run can be resumed once the cause is fixed.",
            self.0.cause
        )
    }
}

impl std::error::Error for Unavailable {}

impl From<Unavailable> for io::Error {
    fn from(failure: Unavailable) -> Self {
        Self::new(failure.kind(), failure)
    }
}

/// `inner` takes its lock with poison kept
/// ([`lock_strict`](ral_core::sync::LockExt::lock_strict)) on purpose: `seq` and `pos` are the file's own position, restated in memory,
/// and [`Self::append`] advances them only after the bytes are written. A panic
/// between the write and the restatement leaves every later `Locus` naming a
/// byte range that is not the record it claims, and replay reads the wrong
/// bytes. A poisoned log must stay poisoned.
pub(crate) struct Log {
    inner: Mutex<Inner>,
    /// A leaf lock, poison ignored: its updates are total.
    latch: Mutex<WriteLatch>,
    /// The record file, as messages name it.
    record: Option<PathBuf>,
}

/// The writer's first failure, and the life it ends.
#[derive(Default)]
struct WriteLatch {
    failure: Option<Unavailable>,
    life: Option<ScopeNode>,
}

/// The session directory a log is kept in, and the writer on its record file.
struct Writer {
    dir: OwnDir,
    file: File,
    #[cfg(test)]
    fail_after: Option<usize>,
}

impl Writer {
    /// The record file, as messages name it.
    fn name(&self) -> PathBuf {
        self.dir.path().join(RECORD)
    }

    fn append(&mut self, line: &[u8]) -> io::Result<()> {
        #[cfg(test)]
        if let Some(remaining) = self.fail_after.as_mut() {
            let written = line.len().min(*remaining);
            self.file.write_all(&line[..written])?;
            *remaining -= written;
            return if written < line.len() {
                Err(io::Error::new(
                    io::ErrorKind::StorageFull,
                    "injected record write failure",
                ))
            } else {
                Ok(())
            };
        }
        self.file.write_all(line)
    }
}

struct Inner {
    /// The log's directory and its writer.  `None` for the store-less log
    /// tests build: facts still stamp and publish, they just have no durable
    /// form.
    writer: Option<Writer>,
    sink: Option<Box<dyn Publish>>,
    seq: u64,
    /// The committed prefix's end, excluding any failed write's tail.
    pos: u64,
}

/// `complete_len` scans backwards a window at a time.
const SCAN_WINDOW: u64 = 8 * 1024;

impl Log {
    /// Start a fresh record log in `dir`, removing any prior file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be removed or created.
    pub(crate) fn create(dir: &OwnDir) -> io::Result<Self> {
        dir.remove(RECORD)?;
        Self::writing(dir, 0, 0)
    }

    /// Reopen `dir`'s record for append — resume — seeding the sequence and
    /// cursor from the complete lines already on disk, so a resumed session's
    /// loci continue the file's own numbering.  Creates the file when there is
    /// none.  The caller quarantines any torn tail first.
    ///
    /// The seeding scan streams: a resume counts the file's lines without ever
    /// holding the file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or reopened.
    pub(crate) fn append_to(dir: &OwnDir) -> io::Result<Self> {
        let (mut seq, mut pos) = (0u64, 0u64);
        match dir.open(RECORD) {
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
            Err(error) => return Err(error.into()),
        }
        Self::writing(dir, seq, pos)
    }

    /// A log over `dir`'s record opened for append, from `seq` and `pos`.
    fn writing(dir: &OwnDir, seq: u64, pos: u64) -> io::Result<Self> {
        let writer = Writer {
            dir: dir.clone(),
            file: dir.append(RECORD)?,
            #[cfg(test)]
            fail_after: None,
        };
        Ok(Self::over(Some(writer), seq, pos))
    }

    /// A log with no file, for tests: it still stamps and publishes.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self::over(None, 0, 0)
    }

    fn over(writer: Option<Writer>, seq: u64, pos: u64) -> Self {
        Self {
            record: writer.as_ref().map(Writer::name),
            inner: Mutex::new(Inner {
                writer,
                sink: None,
                seq,
                pos,
            }),
            latch: Mutex::default(),
        }
    }

    /// Point this log's publisher at a fleet channel.  Called wherever a
    /// session's seam meets a run's bus (attend, deliberate, a direct
    /// `Agent::ral`); re-attaching over a dead per-exchange channel is the
    /// ordinary way a headless session's next exchange comes back on air.
    pub(super) fn attach(&self, sink: Box<dyn Publish>) {
        if let Ok(mut inner) = self.inner.lock_strict() {
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
    pub(super) fn since(
        &self,
        after: Option<Cursor>,
    ) -> io::Result<(
        Cursor,
        impl Iterator<Item = io::Result<Recorded<Record>>> + use<>,
    )> {
        let inner = self.inner.lock_strict()?;
        let from = after.unwrap_or_else(Cursor::start);
        let source = inner
            .writer
            .as_ref()
            .map(|writer| Ok::<_, io::Error>((writer.name(), writer.dir.open(RECORD)?)))
            .transpose()?;
        let until = inner.pos;
        drop(inner);
        Ok((from, Self::records(source, from.seq, from.end, until)?))
    }

    fn latched(&self) -> Option<Unavailable> {
        self.latch.lock_ignore_poison().failure.clone()
    }

    /// Latch `cause` unless a failure stands, ending the bound life.
    pub(super) fn fail(&self, cause: io::Error) -> Unavailable {
        let mut latch = self.latch.lock_ignore_poison();
        let failure = latch
            .failure
            .get_or_insert_with(|| Unavailable::new(self.record.clone(), cause))
            .clone();
        if let Some(life) = latch.life.take() {
            // Without the inbox lock or a notify: a parked owner sees it within PARK_POLL.
            life.cancel(CancelCause::Cancelled);
        }
        drop(latch);
        failure
    }

    /// End `life` when this writer fails, at once if it has.
    pub(super) fn bind(&self, life: ScopeNode) {
        let mut latch = self.latch.lock_ignore_poison();
        debug_assert!(latch.life.is_none(), "one writer, one life");
        if latch.failure.is_some() {
            life.cancel(CancelCause::Cancelled);
        } else {
            latch.life = Some(life);
        }
    }

    /// Observe without the append lock, latching poison when first met.
    pub(super) fn check_available(&self) -> Result<(), Unavailable> {
        if self.inner.is_poisoned() {
            return Err(self.fail(Torn.into()));
        }
        self.latched().map_or(Ok(()), Err)
    }

    #[cfg(test)]
    pub(super) fn fail_after(&self, bytes: usize) {
        let mut inner = self.inner.lock_strict().expect("the append lock is intact");
        inner
            .writer
            .as_mut()
            .expect("fault injection needs a file-backed log")
            .fail_after = Some(bytes);
    }

    #[cfg(test)]
    pub(super) fn poison_append(&self) {
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _guard = self.inner.lock_strict().expect("the append lock is intact");
                panic!("injected append-lock poison");
            }))
            .is_err()
        );
    }

    /// Append `record`, then publish it on the attached channel before
    /// releasing the lock — the whole reason the sink lives in here rather
    /// than beside it. Written directly per record (never `fsync`): process-crash
    /// durable, which is what lets a killed session resume, but not
    /// power-loss durable.
    pub(super) fn append(&self, record: &Record) -> Result<Locus, Unavailable> {
        let mut inner = self
            .inner
            .lock_strict()
            .map_err(|torn| self.fail(torn.into()))?;
        if let Some(failure) = self.latched() {
            return Err(failure);
        }
        let mut line = serde_json::to_vec(&Entry {
            at_unix_ms: now_unix_ms(),
            record,
        })
        .map_err(|error| self.fail(io::Error::other(error)))?;
        line.push(b'\n');
        let start = inner.pos;
        if let Some(writer) = inner.writer.as_mut() {
            writer.append(&line).map_err(|error| self.fail(error))?;
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
    pub(super) fn publish_transient(&self, t: Transient) {
        let Ok(inner) = self.inner.lock_strict() else {
            return;
        };
        if let Some(sink) = &inner.sink {
            sink.transient(t);
        }
    }

    /// The length of `file` through its last newline: what follows is a torn
    /// write, never a record.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read.
    pub(super) fn complete_len(file: &mut File) -> io::Result<u64> {
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

    /// Stream every record of `file`, named `name` in messages, back, in file
    /// order, each located by the `Seq` and byte range it occupies — what
    /// [`super::replay()`] folds.  An unterminated tail is a torn write and is
    /// never parsed.
    ///
    /// One line is in memory at a time, so replaying a session costs the size
    /// of its fold's memo and never the size of its log.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be measured; a line that fails to
    /// parse arrives as an `Err` item, leaving the fold to refuse the session.
    pub(super) fn read(
        mut file: File,
        name: &Path,
    ) -> io::Result<impl Iterator<Item = io::Result<Recorded<Record>>> + use<>> {
        let until = Self::complete_len(&mut file)?;
        Self::records(Some((name.to_path_buf(), file)), 0, 0, until)
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
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
