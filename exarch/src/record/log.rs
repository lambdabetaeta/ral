//! The syscall site for `sessions/<n>/record.jsonl`: the only file handle in the
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
//! headless run mints.
//!
//! Beside the machine record sits a second, human one.  `record.jsonl` is the
//! session's authority and is meant to be replayed, not read: a person who has
//! just been told the assistant would not start opens it and finds one enormous
//! line of JSON per fact, timestamps in milliseconds since 1970, and no way to
//! see at a glance where the session stopped.  So [`Log::append`] also writes
//! `record.log` in the same directory — one line per fact, the moment rendered
//! as a date a human reads, the class and kind named, and as much of the rest
//! as fits on a line.  It is derived entirely from the record that was just
//! written and is never read back by anything here, which is the point: it may
//! be truncated, tailed, grepped or deleted without any consequence for
//! resume.
//!
//! The seam therefore delivers every record to the sink exactly once,
//! whenever the sink arrives: records appended before the first attach are
//! kept and published, in order, by [`Log::attach`].  A publish onto a *dead*
//! channel stays a no-op on purpose — the record is already durable, and a
//! consumer that stopped listening catches up from the file.

use super::{Entry, Locus, Record, Recorded, Seq, Transient};
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

/// Meter one witnessed record and publish it — the one publish rule, shared
/// by a live [`Log::append`] and the backlog [`Log::attach`] delivers.
fn publish(sink: &FleetSink, recorded: Recorded<Record>) {
    if let Record::Forensic(super::Forensic::UsageDelta { usage }) = recorded.value() {
        sink.meter.add(usage.into());
    }
    if sink
        .tx
        .send_signal(Signal::Fact(sink.id, recorded))
        .is_err()
    {
        // No live receiver — the record is already durable on disk, which is
        // the whole point: a pressured or absent consumer catches up from the
        // file, never from the channel.
    }
}

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
    /// `None` for the store-less log tests build: facts still stamp and
    /// publish, they just have no durable form.
    writer: Option<BufWriter<File>>,
    /// The readable mirror described in the module docs.  `None` both where
    /// there is no `writer` at all and where the mirror could not be opened —
    /// a diagnostic that cannot be written is not a reason to fail a session
    /// whose real record is going down fine.
    plain: Option<BufWriter<File>>,
    sink: Option<FleetSink>,
    /// What was appended before any sink attached — a session's bookend, a
    /// fork's inherited context — held so the first sink to arrive is not
    /// missing the head of its own log.  Emptied by [`Log::attach`] and never
    /// refilled: after an attach there is a sink, live or dead.
    pending: Vec<Recorded<Record>>,
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
        reason = "[silent:record-file-create] creates the session's record.jsonl; output infra, not turn-time data I/O"
    )]
    pub(crate) fn create(path: &Path) -> io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self::over(
            Some(BufWriter::new(file)),
            plain_writer(path, false),
            0,
            0,
        ))
    }

    /// Reopen `path` for append — resume — seeding the sequence and cursor
    /// from the complete lines already on disk, so a resumed session's loci
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
                    seq += u64::from(line.ends_with(b"\n"));
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::over(
            Some(BufWriter::new(file)),
            plain_writer(path, true),
            seq,
            pos,
        ))
    }

    /// A log with no file, for tests: it still stamps and publishes.
    #[cfg(test)]
    pub(crate) fn none() -> Self {
        Self::over(None, None, 0, 0)
    }

    fn over(
        writer: Option<BufWriter<File>>,
        plain: Option<BufWriter<File>>,
        seq: u64,
        pos: u64,
    ) -> Self {
        Self {
            inner: Mutex::new(Inner {
                writer,
                plain,
                sink: None,
                pending: Vec::new(),
                seq,
                pos,
            }),
        }
    }

    /// Rotate onto a fresh segment — `Some(path)` a new file, `None` the
    /// store-less log tests build — restarting the sequence and cursor while
    /// leaving the attached sink in place.  The segment is
    /// the file's, never the session's: swapping the `Log` instead would
    /// strand the bus and every `Emitter` clone on the rotated-away file.
    ///
    /// `Seq`/`Locus` ranges are per-segment, not per-session: this only stays
    /// sound because `rotate`'s one caller, `/clear`, resets every fold
    /// (model memo, view) in the same beat, so nothing straddling the old
    /// numbering survives to be confused by the new one starting at zero.
    ///
    /// # Errors
    /// Returns `Err` if the new file cannot be created, or the lock is
    /// poisoned; on either the old segment stays live.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:record-file-rotate] opens the session's next record.jsonl segment; output infra, not turn-time data I/O"
    )]
    #[allow(clippy::disallowed_methods, reason = "see [`Log`]")]
    pub(super) fn rotate(&self, path: Option<&Path>) -> io::Result<()> {
        let writer = path.map(File::create).transpose()?.map(BufWriter::new);
        // The readable mirror rotates with the file it mirrors, and for the
        // same reason `/clear` rotates at all: a segment's lines belong to the
        // segment, and a mirror left pointing at the old one would narrate a
        // context that no longer exists.
        let plain = path.and_then(|path| plain_writer(path, false));
        let mut inner = self
            .inner
            .lock()
            .map_err(|_| io::Error::other("record log lock poisoned"))?;
        inner.writer = writer;
        inner.plain = plain;
        // Undelivered records of the segment being left behind: the new
        // numbering starts at zero, so publishing them past this point would
        // hand a fold two records claiming one `Seq`.
        inner.pending.clear();
        inner.seq = 0;
        inner.pos = 0;
        drop(inner);
        Ok(())
    }

    /// Point this log's publisher at a live fleet channel, delivering whatever
    /// was appended before any sink existed.  Called wherever a session's seam
    /// meets a run's bus (attend, deliberate, a direct `Avatar::ral`);
    /// re-attaching over a dead per-exchange channel is the ordinary way a
    /// headless session's next exchange comes back on air.
    #[allow(clippy::disallowed_methods, reason = "see [`Log`]")]
    pub(super) fn attach(&self, sink: FleetSink) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        for recorded in std::mem::take(&mut inner.pending) {
            publish(&sink, recorded);
        }
        inner.sink = Some(sink);
    }

    /// Append `record`, then publish it on the attached channel before
    /// releasing the lock — the whole reason the sink lives in here rather
    /// than beside it.  Flushed per record (never `fsync`): process-crash
    /// durable, which is what lets a killed session resume, but not
    /// power-loss durable.
    #[allow(clippy::disallowed_methods, reason = "see [`Log`]")]
    pub(super) fn append(&self, record: Record) -> io::Result<Locus> {
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
        // After the authoritative write and never instead of it: a mirror that
        // cannot be written is dropped silently, because the session's record
        // is already safe and failing the append here would turn a cosmetic
        // problem into a lost turn.
        if let Some(plain) = inner.plain.as_mut() {
            let rendered = headline(&line);
            if writeln!(plain, "{rendered}")
                .and_then(|()| plain.flush())
                .is_err()
            {
                inner.plain = None;
            }
        }
        let end = start + line.len() as u64;
        inner.pos = end;
        inner.seq += 1;
        let body = &line[..line.len() - 1];
        let locus = Locus::over(Seq::new(inner.seq), start, body);
        let recorded = Recorded::new(locus.clone(), record);
        match &inner.sink {
            Some(sink) => publish(sink, recorded),
            None => inner.pending.push(recorded),
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
        if let Some(sink) = &inner.sink
            && sink.tx.send_signal(Signal::Transient(sink.id, t)).is_err()
        {
            // No live receiver; a transient has no durable form to catch up
            // from, so there is nothing else to do.
        }
    }

    /// Stream every record back, in file order, each located by the `Seq`
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
        reason = "[silent:record-file-read] streams the session's record.jsonl for replay; output infra, not turn-time data I/O"
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
                            Recorded::new(Locus::over(Seq::new(seq), start, &line), entry.record)
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

/// How much of one fact's own fields the readable line carries before it is
/// cut short.
///
/// A record can be a whole assistant turn, and a mirror that reproduced it
/// would be no more readable than the JSON it exists to replace — the reader
/// would be back to scrolling.  What a person scanning this file wants is the
/// shape of the session: what happened, in what order, and where it stopped.
/// Two hundred characters is enough to tell one tool call from another and
/// short enough that a hundred facts still fit on a screen; the whole of
/// anything is a `record.jsonl` line away.
const HEADLINE_WIDTH: usize = 200;

/// Open the readable mirror beside `record_path`, or give up quietly.
///
/// `append` follows the authoritative file: a resumed session reopens its
/// record for append, and truncating the mirror there would throw away the
/// narration of everything the session did before it was resumed — which is
/// exactly the part a reader is most likely to have come for.
///
/// Every failure here returns `None`.  There is no error to propagate,
/// because there is no caller for whom this file failing is worse than the
/// session failing: a read-only directory, a name already taken by something
/// that is not a file, a descriptor limit — none of them is a reason to
/// refuse to record.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:record-readable-mirror] opens the session's readable record.log beside record.jsonl; a derived diagnostic, not turn-time data I/O"
)]
fn plain_writer(record_path: &Path, append: bool) -> Option<BufWriter<File>> {
    let path = record_path.with_extension("log");
    let file = if append {
        OpenOptions::new().create(true).append(true).open(path)
    } else {
        File::create(path)
    };
    file.ok().map(BufWriter::new)
}

/// One appended record, as one line a person can read.
///
/// Rendered from the JSON that was just written rather than from the typed
/// [`Record`], and deliberately so: the record classes are wide and still
/// growing, and a hand-written match over them would acquire a stale arm the
/// first time a variant was added — the mirror would then quietly stop
/// narrating whichever fact was newest, which is the fact a debugger is most
/// often chasing.  Reading the envelope's own tags costs one parse of a line
/// already in memory and cannot fall behind the type.
///
/// The shape is `<when>  <class>/<kind>  <fields>`: the moment as a date
/// rather than as milliseconds since 1970, the class and kind from the two
/// tags the envelope already carries, and whatever else the fact holds,
/// flattened onto the one line and cut at [`HEADLINE_WIDTH`].
fn headline(line: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
        // Unreachable in practice — these bytes were produced by
        // `serde_json::to_vec` three statements ago — but a mirror is not
        // worth a panic, so it says what it saw and carries on.
        return "?  a record that does not parse as its own JSON".to_string();
    };
    let at_unix_ms = value
        .get("at_unix_ms")
        .and_then(serde_json::Value::as_u64)
        .unwrap_or_default();
    let when = i64::try_from(at_unix_ms)
        .ok()
        .and_then(|ms| jiff::Timestamp::from_millisecond(ms).ok())
        .map_or_else(
            || format!("+{at_unix_ms}ms"),
            |stamp| stamp.strftime("%Y-%m-%d %H:%M:%S%.3fZ").to_string(),
        );
    // `Record` tags itself by variant name and each class tags itself by
    // `kind`, so the two together already name the fact; see the `Record`
    // docs on why there is no single flattened tag to read instead.
    let body = value.get("record").and_then(|r| r.as_object());
    let (class, fields) = body
        .and_then(|outer| outer.iter().next())
        .map_or(("record", None), |(name, inner)| {
            (name.as_str(), inner.as_object())
        });
    let kind = fields
        .and_then(|f| f.get("kind"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or("?");
    let rest = fields.map_or_else(String::new, |f| {
        f.iter()
            .filter(|(name, _)| name.as_str() != "kind")
            .map(|(name, value)| format!("{name}={}", flattened(value)))
            .collect::<Vec<_>>()
            .join(" ")
    });
    let rest = clip(&rest, HEADLINE_WIDTH);
    format!("{when}  {}/{kind}  {rest}", class.to_lowercase())
        .trim_end()
        .to_string()
}

/// One field's value with every line break spent, so a multi-line prompt or a
/// captured stderr cannot turn one record into twenty lines of a file whose
/// whole promise is one line per record.
fn flattened(value: &serde_json::Value) -> String {
    // Strings unquoted — a message reads better as itself than as a JSON
    // literal — and everything else in the JSON spelling it already has.
    let raw = value
        .as_str()
        .map_or_else(|| value.to_string(), str::to_string);
    raw.replace(['\n', '\r'], "\u{23ce}")
}

/// Cut `text` to `width` characters — characters, not bytes, so a cut never
/// lands inside a multi-byte one — and say that it was cut, since a reader
/// who cannot tell a short field from a truncated one will read the wrong
/// thing off it.
fn clip(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let kept: String = text.chars().take(width).collect();
    format!("{kept}…")
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

    /// The readable mirror is the file a person opens after being told a
    /// session would not start, so what matters is that it exists beside the
    /// machine record, that it holds one line per fact, and that the line
    /// says when, what and something of the detail — without a timestamp
    /// anyone has to convert in their head.
    #[test]
    fn every_record_gets_one_readable_line_beside_the_jsonl() {
        let path = temp_path("readable-mirror");
        let log = Log::create(&path).expect("temp record log");
        for text in ["first", "second"] {
            let _locus = log
                .append(Record::Forensic(Forensic::Error { text: text.into() }))
                .expect("append");
        }

        let mirror = path.with_extension("log");
        let read = std::fs::read_to_string(&mirror).expect("the mirror sits beside record.jsonl");
        let lines: Vec<_> = read.lines().collect();
        assert_eq!(lines.len(), 2, "one line per record, got {read:?}");
        for (line, text) in lines.iter().zip(["first", "second"]) {
            assert!(
                line.contains("forensic/error"),
                "the class and kind name the fact: {line}"
            );
            assert!(
                line.contains(text),
                "the fact's own detail survives: {line}"
            );
            assert!(
                line.starts_with("20") && line.contains('-') && line.contains(':'),
                "the moment is a date a person reads, not milliseconds: {line}"
            );
        }
    }

    /// A record carrying a whole transcript must still be one line, or the
    /// file's one promise — a fact per line — is worth nothing on exactly the
    /// records a debugger cares about.
    #[test]
    fn a_multi_line_fact_is_still_one_line_in_the_mirror() {
        let path = temp_path("readable-flattened");
        let log = Log::create(&path).expect("temp record log");
        let _locus = log
            .append(Record::Forensic(Forensic::Error {
                text: "a stack trace\nwith several lines\nin it".to_string(),
            }))
            .expect("append");

        let read = std::fs::read_to_string(path.with_extension("log")).expect("the mirror");
        assert_eq!(
            read.lines().count(),
            1,
            "a record with newlines in it is still one line: {read:?}"
        );
    }

    #[test]
    fn a_record_round_trips_through_the_entry_envelope() {
        let path = temp_path("round-trip");
        let log = Log::create(&path).expect("temp record log");
        let record = Record::Forensic(Forensic::Error {
            text: "boom".into(),
        });
        let _locus = log.append(record).expect("append");

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

    #[test]
    fn a_locus_round_trips_identically_through_append_and_read() {
        let path = temp_path("locus-round-trip");
        let log = Log::create(&path).expect("temp record log");
        let first = log
            .append(Record::Forensic(Forensic::Error {
                text: "first".into(),
            }))
            .expect("append");
        let second = log
            .append(Record::Forensic(Forensic::Error {
                text: "second".into(),
            }))
            .expect("append");

        let back: Vec<_> = Log::read(&path).expect("read back").collect();
        assert_eq!(back.len(), 2);
        let mut back = back.into_iter();
        let first_back = back.next().unwrap().expect("parses");
        let second_back = back.next().unwrap().expect("parses");
        assert_eq!(*first_back.locus(), first);
        assert_eq!(*second_back.locus(), second);
    }

    #[test]
    fn a_mismatched_body_misses_the_digest() {
        let path = temp_path("digest-mismatch");
        let log = Log::create(&path).expect("temp record log");
        let _locus = log
            .append(Record::Forensic(Forensic::Error {
                text: "boom".into(),
            }))
            .expect("append");

        let recorded = Log::read(&path)
            .expect("read back")
            .next()
            .expect("one record")
            .expect("parses");
        assert_ne!(
            Locus::digest_of(b"something else"),
            recorded.locus().digest()
        );
    }
}
