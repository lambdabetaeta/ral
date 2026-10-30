//! The handle on `sessions/<n>/record.jsonl`: the only writer in the tree for
//! this log. This module alone knows the line format: `read` serves replay
//! and resume, `since` a mirror, [`LocusReader`] the transcript reader.
//!
//! The fleet publisher lives inside the same mutex as the writer ([`Inner`]),
//! so a record can only be published while its append is held: no code path
//! reaches the sender without the writer, and channel order is log order by
//! construction.  The publisher is *attachable* rather than fixed at
//! construction because the log outlives any one bus — a session's log is
//! built before the first frontend and survives every bus a
//! headless run mints.
//!
//! The channel is a fast path, never the record: a record appended with no
//! sink attached, or onto a dead channel, is published nowhere.  A consumer
//! that mirrors the log catches up from the file instead ([`Log::since`]).

use super::{Commit, Cursor, Locus, NonEmpty, Publish, RECORD, Record, Recorded, Sum, Transient};
use crate::app::now_unix_ms;
use ral_core::own::OwnDir;
use ral_core::process::{Fatal, ScopeNode};
use ral_core::sync::{LockExt as _, Torn};
use serde::de::{Deserializer, IgnoredAny, MapAccess, Visitor, value::MapAccessDeserializer};
use serde::{Deserialize, Serialize, Serializer};
use serde_json::{error::Category, value::RawValue};
use std::fmt;
use std::fs::File;
use std::io::{self, BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::marker::PhantomData;
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// One line of `record.jsonl`: a commit, sealed by the sum of its exact bytes.
/// `S` is the seal as read: a [`Sum`], or the text standing where one will not
/// parse. Read only through [`object`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Line<'a, S = Sum> {
    sum: S,
    #[serde(borrow)]
    commit: &'a RawValue,
}

/// What a line's `commit` holds. Read only through [`object`].
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Body<R> {
    at_unix_ms: u64,
    records: R,
}

/// A `T` read from a JSON object alone: a derived struct also reads an array.
struct Object<T>(T);

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        deserializer.deserialize_map(Fields(PhantomData)).map(Self)
    }
}

/// The visitor of [`Object`]: a map's entries, as the fields of a `T`.
struct Fields<T>(PhantomData<T>);

impl<'de, T: Deserialize<'de>> Visitor<'de> for Fields<T> {
    type Value = T;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a JSON object")
    }

    fn visit_map<A: MapAccess<'de>>(self, map: A) -> Result<T, A::Error> {
        T::deserialize(MapAccessDeserializer::new(map))
    }
}

/// `text` as a `T`, however deep the writer nested it: every read of a line
/// goes through here.
fn from_json<'de, T: Deserialize<'de>>(text: &'de [u8]) -> serde_json::Result<T> {
    let mut de = serde_json::Deserializer::from_slice(text);
    // Unbounded depth is safe: `serde_stacker` grows the stack onto the heap.
    de.disable_recursion_limit();
    let value = T::deserialize(serde_stacker::Deserializer::new(&mut de))?;
    de.end()?;
    Ok(value)
}

/// `text`, which must be a JSON object, as a `T`.
fn object<'de, T: Deserialize<'de>>(text: &'de [u8]) -> serde_json::Result<T> {
    from_json(text).map(|Object(value)| value)
}

/// Why a complete line yields no commit.
#[derive(Debug)]
enum Unsealed {
    /// Not JSON: damage.
    Syntax(serde_json::Error),
    /// JSON, but not a `{sum, commit}` line: another writer's.
    Shape(serde_json::Error),
    /// The commit does not hash to its seal, which may not even be a sum: damage.
    Sum { sealed: String, actual: Sum },
    /// Sealed, but its records are not this exarch's: another writer's.
    Records(serde_json::Error),
}

impl Unsealed {
    /// Whether a torn write could have left the line, rather than another writer.
    fn damaged(&self) -> bool {
        match self {
            Self::Syntax(_) | Self::Sum { .. } => true,
            Self::Shape(_) | Self::Records(_) => false,
        }
    }

    /// The refusal of line `number` of `path`.
    fn at(self, number: u64, path: &Path) -> io::Error {
        io::Error::other(format!(
            "line {number} of {} {self}",
            ral_core::path::shown(path)
        ))
    }
}

/// The predicate of a refusal whose subject is the line.
impl fmt::Display for Unsealed {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const INTACT: &str = "the lines before it are intact, and nothing from it on can be read.";
        const OLDER: &str = "was this session recorded by an older exarch? Older sessions can be neither resumed nor exported.";
        match self {
            Self::Syntax(error) => write!(
                f,
                "is corrupt: it is not JSON ({error}), so the file was damaged or edited after it was written; {INTACT}"
            ),
            Self::Shape(error) => {
                write!(f, "is not a commit this exarch writes ({error}); {OLDER}")
            }
            Self::Sum { sealed, actual } => write!(
                f,
                "is corrupt: its commit hashes to {actual} but was sealed as {sealed:?}, so the file changed after it was written (edited by hand, or damaged on disk); {INTACT}"
            ),
            Self::Records(error) => {
                write!(
                    f,
                    "holds records this exarch does not read ({error}); {OLDER}"
                )
            }
        }
    }
}

/// A `T` written however deep: the writer's half of [`from_json`].
struct Deep<'a, T>(&'a T);

impl<T: Serialize> Serialize for Deep<'_, T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(serde_stacker::Serializer::new(serializer))
    }
}

/// `body` as one line's bytes, terminator excluded, and the sum sealing it.
fn seal<R: Serialize>(body: &Body<R>) -> serde_json::Result<(Sum, Vec<u8>)> {
    let commit = serde_json::value::to_raw_value(&Deep(body))?;
    let sum = Sum::of(commit.get().as_bytes());
    let line = serde_json::to_vec(&Line {
        sum,
        commit: &commit,
    })?;
    Ok((sum, line))
}

/// The commit `line` seals, if its sum holds.
fn unseal(line: &[u8]) -> Result<(Sum, &RawValue), Unsealed> {
    let Line { sum, commit } = object(line).map_err(|error| unreadable(line, error))?;
    let actual = Sum::of(commit.get().as_bytes());
    if actual == sum {
        Ok((sum, commit))
    } else {
        Err(Unsealed::Sum {
            sealed: sum.to_string(),
            actual,
        })
    }
}

/// Why `line` reads as no [`Line`]: damage unless it is well-formed JSON of
/// another shape. Re-reads `line`, but only on this error path.
fn unreadable(line: &[u8], error: serde_json::Error) -> Unsealed {
    match error.classify() {
        Category::Syntax | Category::Eof | Category::Io => Unsealed::Syntax(error),
        Category::Data => match from_json::<IgnoredAny>(line) {
            Err(syntax) => Unsealed::Syntax(syntax),
            Ok(_) => match object::<Line<'_, String>>(line) {
                // A line in every respect but its seal, which no writer leaves.
                Ok(Line { sum, commit }) => Unsealed::Sum {
                    sealed: sum,
                    actual: Sum::of(commit.get().as_bytes()),
                },
                Err(_) => Unsealed::Shape(error),
            },
        },
    }
}

/// The records a sealed commit holds.
fn records_of(commit: &RawValue) -> Result<NonEmpty<Record>, Unsealed> {
    object::<Body<NonEmpty<Record>>>(commit.get().as_bytes())
        .map(|body| body.records)
        .map_err(Unsealed::Records)
}

/// The `number`th commit, spanning `bytes`, sealed as `body` by `sum`.
fn commit_of(
    sum: Sum,
    body: &RawValue,
    number: u64,
    bytes: Range<u64>,
) -> Result<Commit, Unsealed> {
    Ok(Commit::new(number, bytes, sum, records_of(body)?))
}

/// Each commit's records in order; an unreadable line is one error.
fn flatten(
    commits: impl Iterator<Item = io::Result<Commit>>,
) -> impl Iterator<Item = io::Result<Recorded<Record>>> {
    commits.flat_map(|commit| match commit {
        Ok(commit) => commit
            .into_records()
            .into_iter()
            .map(Ok)
            .collect::<Vec<_>>(),
        Err(error) => vec![Err(error)],
    })
}

/// `records` as the line `append` writes, terminator excluded.
#[cfg(test)]
pub(crate) fn commit_line(records: &[Record]) -> Vec<u8> {
    seal(&Body {
        at_unix_ms: 0,
        records,
    })
    .expect("records serialise")
    .1
}

/// The sum and records of one sealed line, if it is one.
#[cfg(test)]
pub(crate) fn parse_line(line: &[u8]) -> Option<(Sum, Vec<Record>)> {
    let (sum, commit) = unseal(line).ok()?;
    let body = object::<Body<Vec<Record>>>(commit.get().as_bytes()).ok()?;
    Some((sum, body.records))
}

/// The first recording failure, latched for good and cheap to clone.
#[derive(Clone, Debug)]
pub struct Unavailable(Arc<Failure>);

#[derive(Debug)]
enum Failure {
    Recording {
        /// `None` for a fileless test log.
        record: Option<PathBuf>,
        cause: io::Error,
    },
    CellPoisoned(String),
}

impl Unavailable {
    fn new(record: Option<PathBuf>, cause: io::Error) -> Self {
        Self(Arc::new(Failure::Recording { record, cause }))
    }

    pub fn kind(&self) -> io::ErrorKind {
        match &*self.0 {
            Failure::Recording { cause, .. } => cause.kind(),
            Failure::CellPoisoned(_) => io::ErrorKind::Other,
        }
    }
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (record, cause) = match &*self.0 {
            Failure::Recording { record, cause } => (record, cause),
            Failure::CellPoisoned(why) => return f.write_str(why),
        };
        match record {
            Some(path) => write!(f, "recording to {} failed", ral_core::path::shown(path))?,
            None => f.write_str("recording failed")?,
        }
        write!(
            f,
            ": {cause}; this session has ended, since it can record nothing more. Its record keeps everything before the failure, so the run can be resumed once the cause is fixed."
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
/// ([`lock_strict`](ral_core::sync::LockExt::lock_strict)) on purpose: the
/// commit count and `pos` are the file's own position, restated in memory,
/// and [`Self::append`] advances them only after the bytes are written. A panic
/// between the write and the restatement leaves every later `Locus` naming a
/// byte range that is not the record it claims, and replay reads the wrong
/// bytes. A poisoned log must stay poisoned.
pub(crate) struct Log {
    inner: Mutex<Inner>,
    /// The writer's first failure: it ends the life bound to it.
    failure: Fatal<Unavailable>,
    /// The record file, as messages name it.
    record: Option<PathBuf>,
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
    /// The number of the last commit on file.
    commits: u64,
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

    /// Reopen `dir`'s record for append — resume — seeding the commit count
    /// and cursor from the lines already on disk, one commit each, so a resumed session's
    /// loci continue the file's own numbering.  Creates the file when there is
    /// none.
    ///
    /// The seeding scan streams: a resume counts the file's lines without ever
    /// holding the file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read or reopened, or ends in a torn
    /// line ([`Self::complete_len`]), which the caller quarantines first.
    pub(crate) fn append_to(dir: &OwnDir) -> io::Result<Self> {
        let (commits, pos) = match dir.open(RECORD) {
            Ok(file) => Self::extent(file, &dir.path().join(RECORD))?,
            Err(error) if error.kind() == io::ErrorKind::NotFound => (0, 0),
            Err(error) => return Err(error.into()),
        };
        Self::writing(dir, commits, pos)
    }

    /// The number of commits on `file`, named `name` in messages, and its
    /// length, refused unless it ends at a whole commit.
    fn extent(mut file: File, name: &Path) -> io::Result<(u64, u64)> {
        let len = file.metadata()?.len();
        let complete = Self::complete_within(&mut file, len)?;
        if complete < len {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "cannot append to {}: bytes {complete}..{len} are a torn last line (an unterminated write, or a line that is not JSON or fails its sum); a commit appended after it would leave it corrupt mid-log, so it must be quarantined first, as resuming the session does",
                    ral_core::path::shown(name)
                ),
            ));
        }
        let _ = file.seek(SeekFrom::Start(0))?;
        // Only the `len` bytes measured above, so the count and length agree.
        let mut lines = BufReader::new(file.take(len));
        let mut commits = 0;
        while lines.skip_until(b'\n')? > 0 {
            commits += 1;
        }
        Ok((commits, len))
    }

    /// A log over `dir`'s record opened for append, from `commits` and `pos`.
    fn writing(dir: &OwnDir, commits: u64, pos: u64) -> io::Result<Self> {
        let writer = Writer {
            dir: dir.clone(),
            file: dir.append(RECORD)?,
            #[cfg(test)]
            fail_after: None,
        };
        Ok(Self::over(Some(writer), commits, pos))
    }

    /// A log with no file, for tests: it still stamps and publishes.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self::over(None, 0, 0)
    }

    fn over(writer: Option<Writer>, commits: u64, pos: u64) -> Self {
        Self {
            record: writer.as_ref().map(Writer::name),
            inner: Mutex::new(Inner {
                writer,
                sink: None,
                commits,
                pos,
            }),
            failure: Fatal::default(),
        }
    }

    /// Point this log's publisher at a fleet channel.  Called wherever a
    /// session's seam meets a run's bus (attend, deliberate, a direct
    /// `Agent::ral`); re-attaching over a dead channel is the
    /// ordinary way a headless session's next exchange comes back on air.
    pub(super) fn attach(&self, sink: Box<dyn Publish>) {
        if let Ok(mut inner) = self.inner.lock_strict() {
            inner.sink = Some(sink);
        }
    }

    /// Where `after` stands — at the head when none — and the commits past
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
    ) -> io::Result<(Cursor, impl Iterator<Item = io::Result<Commit>> + use<>)> {
        let inner = self.inner.lock_strict()?;
        let from = after.unwrap_or_else(Cursor::start);
        let source = inner
            .writer
            .as_ref()
            .map(|writer| Ok::<_, io::Error>((writer.name(), writer.dir.open(RECORD)?)))
            .transpose()?;
        let until = inner.pos;
        drop(inner);
        Ok((from, Self::commits(source, from.commit, from.end, until)?))
    }

    /// Latch `cause` unless a failure stands, ending the bound life.
    pub(super) fn fail(&self, cause: io::Error) -> Unavailable {
        self.failure
            .latch(Unavailable::new(self.record.clone(), cause))
    }

    pub(super) fn cell_poisoned(&self, why: String) -> Unavailable {
        self.failure
            .latch(Unavailable(Arc::new(Failure::CellPoisoned(why))))
    }

    /// End `life` when this writer fails, at once if it has.
    pub(super) fn bind(&self, life: ScopeNode) {
        self.failure.bind(life);
    }

    /// Observe without the append lock, latching poison when first met.
    pub(super) fn check_available(&self) -> Result<(), Unavailable> {
        if self.inner.is_poisoned() {
            return Err(self.fail(Torn.into()));
        }
        self.failure.cause().map_or(Ok(()), Err)
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

    /// Append one sealed line, then publish its records under the lock.
    /// Never `fsync`: process-crash durable, not power-loss durable.
    pub(super) fn append(&self, records: NonEmpty<Record>) -> Result<Commit, Unavailable> {
        let mut inner = self
            .inner
            .lock_strict()
            .map_err(|torn| self.fail(torn.into()))?;
        if let Some(failure) = self.failure.cause() {
            return Err(failure);
        }
        let (sum, mut line) = seal(&Body {
            at_unix_ms: now_unix_ms(),
            records: &records,
        })
        .map_err(|error| self.fail(io::Error::other(error)))?;
        line.push(b'\n');
        let start = inner.pos;
        if let Some(writer) = inner.writer.as_mut() {
            writer.append(&line).map_err(|error| self.fail(error))?;
        }
        inner.pos = start + line.len() as u64;
        inner.commits += 1;
        let commit = Commit::new(inner.commits, start..inner.pos, sum, records);
        if let Some(sink) = &inner.sink {
            sink.commit(commit.clone());
        }
        drop(inner);
        Ok(commit)
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

    /// The length of `file` through its last complete line, unless that line is
    /// damaged. What follows is torn and never read: an unterminated write, or a
    /// terminated last line that is not JSON or fails its sum.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be read.
    pub(super) fn complete_len(file: &mut File) -> io::Result<u64> {
        let len = file.metadata()?.len();
        Self::complete_within(file, len)
    }

    /// [`Self::complete_len`] of `file`, whose length is `len`.
    fn complete_within(file: &mut File, len: u64) -> io::Result<u64> {
        let Some(last) = Self::newline_before(file, len)? else {
            return Ok(0);
        };
        // An unterminated write is the last line, so the line before it is interior.
        if last + 1 < len {
            return Ok(last + 1);
        }
        let start = Self::newline_before(file, last)?.map_or(0, |newline| newline + 1);
        let _ = file.seek(SeekFrom::Start(start))?;
        let mut line = Vec::new();
        let _ = Read::by_ref(file)
            .take(last - start)
            .read_to_end(&mut line)?;
        Ok(match unseal(&line) {
            Err(fault) if fault.damaged() => start,
            Ok(_) | Err(_) => last + 1,
        })
    }

    /// The last newline before `end`, scanning backwards a window at a time.
    fn newline_before(file: &mut File, mut end: u64) -> io::Result<Option<u64>> {
        while end > 0 {
            let start = end.saturating_sub(SCAN_WINDOW);
            let mut chunk = vec![0; usize::try_from(end - start).unwrap_or(usize::MAX)];
            let _ = file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut chunk)?;
            if let Some(last) = chunk.iter().rposition(|byte| *byte == b'\n') {
                return Ok(Some(start + last as u64));
            }
            end = start;
        }
        Ok(None)
    }

    /// Stream every record of `file`, named `name` in messages, back, in file
    /// order, each located by the `Seq` and byte range it occupies — what
    /// [`super::replay()`] folds.  A torn last line ([`Self::complete_len`]) is
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
        Ok(flatten(Self::commits(
            Some((name.to_path_buf(), file)),
            0,
            0,
            until,
        )?))
    }

    /// The commits after `number`, from byte `pos` up to `until`; none without
    /// a file.
    fn commits(
        source: Option<(PathBuf, File)>,
        mut number: u64,
        mut pos: u64,
        until: u64,
    ) -> io::Result<impl Iterator<Item = io::Result<Commit>> + use<>> {
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
            let (path, lines) = lines.as_mut()?;
            let line = match lines.next()? {
                Ok(line) => line,
                Err(error) => return Some(Err(error)),
            };
            let start = pos;
            pos += line.len() as u64 + 1;
            number += 1;
            Some(
                unseal(&line)
                    .and_then(|(sum, body)| commit_of(sum, body, number, start..pos))
                    .map_err(|fault| fault.at(number, path)),
            )
        }))
    }
}

/// Why a locus no longer names what it was measured over.
const STALE: &str = "the log at this path is no longer the file those byte ranges were measured in; a copied session directory or an edited log";

/// Records read back by [`Locus`] from one log file, each line read once while
/// consecutive loci lie on it. Messages name the line; the caller names the
/// file.
pub(super) struct LocusReader {
    file: File,
    /// The line last read.
    line: Option<Commit>,
}

impl LocusReader {
    pub(super) fn new(file: File) -> Self {
        Self { file, line: None }
    }

    /// The record `locus` names: refused unless its range still holds the
    /// line its sum sealed.
    pub(super) fn record(&mut self, locus: &Locus) -> io::Result<&Record> {
        let line = match self.line.take() {
            Some(line) if line.holds(locus) => line,
            _ => self.line_at(locus)?,
        };
        let (number, index) = (locus.seq().commit(), locus.seq().index());
        self.line
            .insert(line)
            .records()
            .iter()
            .nth(index)
            .map(Recorded::value)
            .ok_or_else(|| {
                io::Error::other(format!(
                    "a locus named record {index} of line {number}, which holds fewer: {STALE}"
                ))
            })
    }

    /// The line `locus` lies on, read, sum-checked and only then parsed.
    fn line_at(&mut self, locus: &Locus) -> io::Result<Commit> {
        let (number, bytes) = (locus.seq().commit(), locus.bytes());
        let _ = self.file.seek(SeekFrom::Start(bytes.start))?;
        let mut buf = Vec::new();
        let _ = Read::by_ref(&mut self.file)
            .take(bytes.end.saturating_sub(bytes.start))
            .read_to_end(&mut buf)?;
        match buf.strip_suffix(b"\n").map(unseal) {
            Some(Ok((sum, body))) if sum == locus.sum() => commit_of(sum, body, number, bytes)
                .map_err(|fault| io::Error::other(format!("line {number} {fault}"))),
            _ => Err(io::Error::other(format!(
                "line {number} did not hash to the locus that named it: {STALE}"
            ))),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
