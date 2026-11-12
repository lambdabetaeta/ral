//! Redirects: open a redirect list's targets, install them over the shell's
//! streams, run a body, then restore the streams and settle the writes.
//!
//! [`RedirectState`] is the one interpreter of a redirect list.  The machine
//! holds it in `Frame::Redirect`; [`with_redirects`] brackets a synchronous
//! call with it.
//!
//! Redirects replace the shell's `Sink`s and `Source`, never the process's
//! fds: libtest, the REPL frontend and sibling ral threads share those.
//!
//! Every target is opened through [`Shell::locate`], so the handle names the
//! object the grant judged, and paths resolve against the shell's scoped cwd.

use crate::authority::policy::FsOp;
use crate::fact::{IoOperation, IoSubject, Read, Write};
use crate::io::{IoPatch, Sink, Source, SourceReader};
use crate::ir::{Redirects, StderrTarget, StdinSource, WriteMode};
use crate::path::{
    Kind, Reading, StagedWrite, Streamed, Streaming, Trace, Visibility, Writing, open_discard,
};
use crate::shell::runtime::{
    Bytes, Error, Mooring, Observed, Settled, Shell, ValClos, WriteOutcome,
};
use crate::source::Span;
use crate::text::quote_native;
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read as _, Seek as _, Write as _};
use std::panic::{AssertUnwindSafe, catch_unwind, resume_unwind};
use std::sync::Arc;

/// Run `body` under `redirects`.  Its writes commit if it returns and abort
/// if it breaks; a panic restores the streams and drops the writes.
///
/// An external runs this inside `Command::with_record`'s audit capture, so
/// redirected bytes stay out of the `Command` record.
pub(crate) fn with_redirects(
    redirects: &Redirects<String>,
    span: Option<Span>,
    mooring: &Mooring,
    shell: &mut Shell,
    body: impl FnOnce(&mut Shell) -> Settled<ValClos>,
) -> Settled<ValClos> {
    if redirects.is_empty() {
        return body(shell);
    }
    let state = RedirectState::install(redirects, span, mooring, shell)?;
    match catch_unwind(AssertUnwindSafe(|| body(shell))) {
        Ok(Ok(value)) => state.commit(mooring, shell).map(|()| value),
        // The body's break takes precedence over anything settling reports.
        Ok(Err(b)) => {
            state.abort(mooring, shell);
            Err(b)
        }
        Err(payload) => {
            state.abandon(shell);
            resume_unwind(payload)
        }
    }
}

/// An installed redirect.  It borrows nothing, so the machine can hold it
/// across steps, and it must be ended explicitly by `commit`, `abort` or
/// `abandon`.
pub(crate) struct RedirectState {
    /// The streams the redirect replaced.
    prior: IoPatch,
    writes: Vec<PendingWrite>,
    /// The redirect's site, which every observation it makes carries.
    span: Option<Span>,
}

impl RedirectState {
    /// Open every target, then install them.  If an open fails, nothing is
    /// installed and the writes already opened are aborted.
    pub(crate) fn install(
        redirects: &Redirects<String>,
        span: Option<Span>,
        mooring: &Mooring,
        shell: &mut Shell,
    ) -> Settled<Self> {
        shell.at_site("redirect", span, |shell| {
            let mut writes = Vec::new();
            match open_targets(redirects, mooring, shell, &mut writes) {
                Ok(patch) => Ok(Self {
                    prior: shell.io.swap(patch),
                    writes,
                    span,
                }),
                Err(b) => {
                    abort_writes(writes, mooring, shell);
                    Err(b.locate("redirect", span, shell))
                }
            }
        })
    }

    /// Restore the streams, then publish every write.  Returns the first
    /// publish failure.
    pub(crate) fn commit(self, mooring: &Mooring, shell: &mut Shell) -> Settled<()> {
        let Self {
            prior,
            writes,
            span,
        } = self;
        restore_streams(prior, shell);
        shell.at_site("redirect", span, |shell| {
            commit_writes(writes, mooring, shell).map_err(|b| b.locate("redirect", span, shell))
        })
    }

    /// Restore the streams, then abort every write.
    pub(crate) fn abort(self, mooring: &Mooring, shell: &mut Shell) {
        let Self {
            prior,
            writes,
            span,
        } = self;
        restore_streams(prior, shell);
        shell.at_site("redirect", span, |shell| {
            abort_writes(writes, mooring, shell);
        });
    }

    /// For a panic: restore the streams and drop the writes unobserved.
    /// Dropping an atomic write unlinks its staging file.
    pub(crate) fn abandon(self, shell: &mut Shell) {
        restore_streams(self.prior, shell);
    }
}

/// Swap the prior streams back, then flush the redirect's own sinks so their
/// buffered bytes land, and drop them, closing their handles before the
/// writes settle.
fn restore_streams(prior: IoPatch, shell: &mut Shell) {
    let own = shell.io.swap(prior);
    for mut sink in [own.stdout, own.stderr].into_iter().flatten() {
        let _ = sink.flush();
    }
}

/// Publish every write, settling each even after one fails.  Returns the
/// first failure.
fn commit_writes(writes: Vec<PendingWrite>, mooring: &Mooring, shell: &mut Shell) -> Settled<()> {
    let mut first_failure = Ok(());
    for write in writes {
        let settled = write.settle(Fate::Commit, mooring, shell);
        first_failure = first_failure.and(settled);
    }
    first_failure
}

/// Abort every write.  Aborting publishes nothing, so it cannot fail.
fn abort_writes(writes: Vec<PendingWrite>, mooring: &Mooring, shell: &mut Shell) {
    for write in writes {
        let aborted = write.settle(Fate::Abort, mooring, shell);
        debug_assert!(aborted.is_ok(), "an abort publishes nothing");
    }
}

// ── Opening ─────────────────────────────────────────────────────────────

/// Open the targets of `redirects` as the streams to install, in the order
/// stdin, stdout, stderr, so the audit trail is the same for a block and an
/// external.  Each write opened is pushed onto `writes`, including those
/// opened before a later open fails.
fn open_targets(
    redirects: &Redirects<String>,
    mooring: &Mooring,
    shell: &mut Shell,
    writes: &mut Vec<PendingWrite>,
) -> Settled<IoPatch> {
    let stdin = (redirects.stdin.as_ref())
        .map(|stdin| open_stdin_source(stdin, mooring, shell))
        .transpose()?;
    let stdout = (redirects.stdout.as_ref())
        .map(|(mode, path)| open_output_sink(path, *mode, mooring, shell, writes))
        .transpose()?;
    let stderr = match &redirects.stderr {
        Some(StderrTarget::File(mode, path)) => {
            Some(open_output_sink(path, *mode, mooring, shell, writes)?)
        }
        // `2>&1`: wherever stdout goes, redirected or not.
        Some(StderrTarget::Stdout) => {
            Some(stdout.clone().unwrap_or_else(|| shell.io.stdout.clone()))
        }
        None => None,
    };
    Ok(IoPatch {
        stdin,
        stdout,
        stderr,
    })
}

/// Open `< path` or `<< str` as a source.  A file read is observed here, so
/// it precedes the body it feeds.
fn open_stdin_source(
    stdin: &StdinSource<String>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<Source> {
    let reader = match stdin {
        StdinSource::File(path) => {
            let rp = shell.resolve(path);
            let file = if rp.is_discard() {
                open_discard()
            } else {
                shell.locate::<Reading>(&rp)?.read()
            }
            .map_err(io_error(IoOperation::Open, path))?;
            shell.observe(mooring, Observed::Read(Read { path: path.clone() }));
            SourceReader::file(file)
        }
        StdinSource::Here(word) => {
            // Whatever its source, the value loses one leading LF or CRLF, so
            // a multiline string can start on the line after the command.
            let body = word
                .strip_prefix("\r\n")
                .or_else(|| word.strip_prefix('\n'))
                .unwrap_or(word);
            SourceReader::from_bytes(body.as_bytes().to_vec()).map_err(|error| {
                crate::process::launch::prepare_io(
                    crate::fact::PrepareStep::Stdio,
                    crate::fact::PrepareSubject::Stream(crate::fact::StandardStream::Stdin),
                    &error,
                )
                .with_context("preparing a here-string")
            })?
        }
    };
    Ok(Source::Reader(reader))
}

/// Open a stdout or stderr target as a sink, pushing its write onto
/// `writes`.  A failed open is observed here, since it never reaches a
/// settle.
fn open_output_sink(
    path: &str,
    mode: WriteMode,
    mooring: &Mooring,
    shell: &mut Shell,
    writes: &mut Vec<PendingWrite>,
) -> Settled<Sink> {
    let heard = shell.listening(mooring);
    let (writer, write) = open_write_target(path, mode, heard, shell).inspect_err(|_| {
        let failed = Write {
            path: path.to_owned(),
            mode,
            outcome: WriteOutcome::Failed,
            new_bytes: None,
            old_bytes: None,
        };
        shell.observe(mooring, Observed::Write(failed));
    })?;
    writes.extend(write);
    Ok(Sink::File(Arc::new(writer)))
}

/// Open a write target: the handle to write through, and the write to
/// settle afterwards, `None` for the discard device.
///
/// `>` onto a regular file is staged beside it; everything else streams.  If
/// `heard`, the before-image is read before a streaming `>` truncates.
fn open_write_target(
    path: &str,
    mode: WriteMode,
    heard: bool,
    shell: &mut Shell,
) -> Settled<(File, Option<PendingWrite>)> {
    let rp = shell.resolve(path);
    if rp.is_discard() {
        return Ok((
            open_discard().map_err(io_error(IoOperation::Open, path))?,
            None,
        ));
    }
    let located = shell.locate::<Writing>(&rp)?;
    let streaming = match mode {
        WriteMode::Append => Some(Streaming::Append),
        WriteMode::Stream => Some(Streaming::Truncate),
        // A TTY or a FIFO has no inode to rename over.
        WriteMode::Write => located
            .stat()
            .is_some_and(|s| s.kind != Kind::File)
            .then_some(Streaming::Truncate),
    };
    let mut handle = match streaming {
        Some(access) => WriteHandle::Stream(
            located
                .open_streaming(access)
                .map_err(io_error(IoOperation::Open, path))?,
        ),
        None => WriteHandle::Atomic(
            located
                .stage(Visibility::Public)
                .map_err(io_error(IoOperation::Stage, path))?,
        ),
    };
    let writer = handle
        .file()
        .try_clone()
        .map_err(io_error(IoOperation::Duplicate, path))?;
    let before = if heard {
        handle.before_image(shell)
    } else {
        None
    };
    if let (WriteHandle::Stream(streamed), Some(Streaming::Truncate)) = (&handle, streaming) {
        streamed
            .truncate()
            .map_err(io_error(IoOperation::Write, path))?;
    }
    let write = PendingWrite {
        path: path.to_owned(),
        mode,
        handle,
        before,
    };
    Ok((writer, Some(write)))
}

// ── Settling ────────────────────────────────────────────────────────────

/// How a redirect ends for its writes.
enum Fate {
    /// Publish each atomic write onto its target.
    Commit,
    /// Drop each atomic write, leaving its target unchanged.
    Abort,
}

/// An opened write, kept until the redirect ends so it can be published or
/// dropped, and reported.
struct PendingWrite {
    path: String,
    mode: WriteMode,
    handle: WriteHandle,
    /// The target's content before the write, read only if someone listens.
    before: Option<Vec<u8>>,
}

impl PendingWrite {
    /// Publish or drop the write by `fate`, and observe the outcome.  Fails
    /// only if publishing fails.
    fn settle(self, fate: Fate, mooring: &Mooring, shell: &mut Shell) -> Settled<()> {
        let Self {
            path,
            mode,
            mut handle,
            before,
        } = self;
        // Publishing consumes the staged write, so read it first.
        let after = if shell.listening(mooring) {
            handle.after_image(shell)
        } else {
            None
        };
        let (outcome, published) = match (handle, fate) {
            (WriteHandle::Atomic(_), Fate::Abort) => (WriteOutcome::Aborted, Ok(())),
            (WriteHandle::Atomic(staged), Fate::Commit) => match staged.publish() {
                Ok(()) => (WriteOutcome::Committed, Ok(())),
                Err(e) => (WriteOutcome::Failed, Err(e)),
            },
            // A stream's bytes landed as they were written: it cannot abort.
            (WriteHandle::Stream(_), _) => (WriteOutcome::Committed, Ok(())),
        };
        let published = published.map_err(io_error(IoOperation::Write, &path));
        // A write that did not land changed nothing, so it has no images.
        let landed = outcome == WriteOutcome::Committed;
        let image = |bytes: Option<Vec<u8>>| bytes.filter(|_| landed).map(Bytes::from);
        let write = Write {
            path,
            mode,
            outcome,
            new_bytes: image(after),
            old_bytes: image(before),
        };
        shell.observe(mooring, Observed::Write(write));
        Ok(published?)
    }
}

/// The handle a write lands through.
enum WriteHandle {
    /// `>` onto a regular file: a staging file beside it, renamed over it on
    /// publish and unlinked on drop.
    Atomic(StagedWrite),
    /// Everything else: bytes land as they are written.
    Stream(Streamed),
}

impl WriteHandle {
    /// The file the writer's handle is cloned from, sharing its offset.
    fn file(&mut self) -> &mut File {
        match self {
            Self::Atomic(staged) => staged.file(),
            Self::Stream(streamed) => streamed.file(),
        }
    }

    /// The target, as the grant judges a read of it.
    fn trace(&self) -> &Trace {
        match self {
            Self::Atomic(staged) => staged.target().trace(),
            Self::Stream(streamed) => streamed.trace(),
        }
    }

    /// The target's content before the write: empty if the write creates
    /// it, `None` if the grant forbids reading it, it has no reader, or it
    /// exceeds [`IMAGE_CAP`].
    fn before_image(&mut self, shell: &Shell) -> Option<Vec<u8>> {
        if !shell.admits_fs(FsOp::Read, self.trace()) {
            return None;
        }
        match self {
            Self::Atomic(staged) => match staged.target().stat() {
                None => Some(Vec::new()),
                Some(s) if s.kind == Kind::File => read_image(&mut staged.target().read().ok()?),
                Some(_) => None,
            },
            Self::Stream(streamed) if streamed.created() => Some(Vec::new()),
            Self::Stream(streamed) => read_image(streamed.reader()?),
        }
    }

    /// What ral wrote, read through its own handle.  Never read by name: on
    /// Unix another object may hold the name by now.
    fn after_image(&mut self, shell: &Shell) -> Option<Vec<u8>> {
        match self {
            // The staging file is ral's own; no grant applies to it.
            Self::Atomic(staged) => read_image(staged.file()),
            Self::Stream(streamed) if shell.admits_fs(FsOp::Read, streamed.trace()) => {
                read_image(streamed.reader()?)
            }
            Self::Stream(_) => None,
        }
    }
}

/// Cap on an image, so a large file is never read into memory to show a
/// change.
const IMAGE_CAP: u64 = 64 * 1024;

/// All of `file` from its start, or `None` if it exceeds [`IMAGE_CAP`].  The
/// size is read from the open handle, so it is the size of what is read.
fn read_image(file: &mut File) -> Option<Vec<u8>> {
    if file.metadata().ok()?.len() > IMAGE_CAP {
        return None;
    }
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

// ── Host writes ─────────────────────────────────────────────────────────

impl Shell {
    /// Overwrite `path` exactly as `>` would — following symlinks, keeping
    /// the mode, syncing before the rename — but observe nothing.  For host
    /// builtins such as exarch's `edit-hash` and `edit-replace`, which report
    /// their writes themselves.
    ///
    /// # Errors
    /// The open, the write, or the publish fails.
    pub fn atomic_write(&mut self, path: &str, bytes: &[u8]) -> Settled<()> {
        let (mut writer, write) = open_write_target(path, WriteMode::Write, false, self)?;
        writer
            .write_all(bytes)
            .map_err(io_error(IoOperation::Write, path))?;
        drop(writer);
        if let Some(PendingWrite {
            handle: WriteHandle::Atomic(staged),
            ..
        }) = write
        {
            staged
                .publish()
                .map_err(io_error(IoOperation::Write, path))?;
        }
        Ok(())
    }
}

/// An I/O failure on `name`, as an error.
fn io_error(operation: IoOperation, name: &str) -> impl Fn(io::Error) -> Error + '_ {
    move |e| {
        Error::io(
            operation,
            IoSubject::Path(quote_native(OsStr::new(name))),
            &e,
        )
    }
}
