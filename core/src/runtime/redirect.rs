//! Redirects: the doors that open a target (the atomic `>` recipe, the
//! `< file` handoff into `shell.io.stdin`), and the [`scope`] that installs a
//! redirect list around a body and settles it.
//!
//! No open lives here.  Every one goes through [`Shell::locate`] into
//! `path::walk`, so the object authorised is the object the handle then
//! names — the door never re-walks a string the guard has already judged.

pub(crate) mod scope;

use crate::capability::FsOp;
use crate::fact::Read;
use crate::ir::{StdinSource, WriteMode};
use crate::path::walk::Kind;
use crate::path::{Reading, StagedWrite, Streamed, Streaming, Trace, Writing};
use crate::types::{Break, Error, Mooring, Observed, Settled, Shell};
use std::fs::File;
use std::io::{self, Read as _, Seek as _};

/// Cap on the bytes a preview reads, so a large file is never pulled into
/// memory whole to show a change.
const PREVIEW_CAP: u64 = 64 * 1024;

/// A write redirect's target as opened, held until its frame settles so the
/// frame can report what the write did to it.  No `Located` remains: the
/// object is the handles.
pub(crate) enum OpenedWrite {
    /// `>` onto a regular file, staged beside it until commit; dropped, the
    /// staging file is unlinked.
    Atomic(StagedWrite),
    /// Every other shape: bytes land as they are written.
    Stream(Streamed),
}

impl OpenedWrite {
    /// What stands at the target before the write, where it is ours to read
    /// under the live grant: empty for a leaf the write created, `None` for
    /// an object with no reader.
    pub(crate) fn before(&mut self, shell: &Shell) -> Option<Vec<u8>> {
        match self {
            Self::Atomic(staged) => {
                let target = staged.target();
                admitted(shell, target.trace()).then(|| match target.stat() {
                    None => Some(Vec::new()),
                    Some(s) if s.kind == Kind::File => read_capped(&mut target.read().ok()?),
                    Some(_) => None,
                })?
            }
            Self::Stream(streamed) => admitted(shell, streamed.trace()).then(|| {
                if streamed.created() {
                    Some(Vec::new())
                } else {
                    read_capped(streamed.reader()?)
                }
            })?,
        }
    }

    /// What the write leaves standing: what ral wrote, read through the
    /// staging handle or the stream's own reader.  Never a read of the name,
    /// which may hold another object by now: on Unix the rename takes the
    /// sibling's *name*, so what lands at the target is not this write's to
    /// assert.
    pub(crate) fn after(&mut self, shell: &Shell) -> Option<Vec<u8>> {
        match self {
            Self::Atomic(staged) => read_capped(staged.file()),
            Self::Stream(streamed) => {
                admitted(shell, streamed.trace()).then(|| read_capped(streamed.reader()?))?
            }
        }
    }
}

/// Whether the live grant lets ral read what `trace` names.
fn admitted(shell: &Shell, trace: &Trace) -> bool {
    shell.admits_fs(&FsOp::Read, trace)
}

/// `file` whole, from its start.  `None` past [`PREVIEW_CAP`], judged on the
/// open handle so the size read is the size of the bytes read.
fn read_capped(file: &mut File) -> Option<Vec<u8>> {
    if file.metadata().ok()?.len() > PREVIEW_CAP {
        return None;
    }
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

/// Open a write redirect's target, and the before-image of it where `heard`
/// asks.  `>` to a regular file is staged, to be committed or dropped once
/// the writer finishes; every other shape streams, a truncating one only
/// after its before-image was read.  `None` is the discard device, which no
/// write changes.  Paths resolve against the shell's scoped cwd, so a
/// `within [dir: …]` redirect lands right even from a native, where the host
/// cwd never moves.
pub(crate) fn open_write(
    path: &str,
    mode: WriteMode,
    heard: bool,
    shell: &mut Shell,
) -> Settled<(File, Option<OpenedWrite>, Option<Vec<u8>>)> {
    let io = |e: io::Error| Break::from(Error::io(path, &e));
    let rp = shell.resolve(path);
    if rp.is_discard() {
        return Ok((crate::path::open_discard().map_err(io)?, None, None));
    }
    let target = shell.locate::<Writing>(&rp)?;
    let streaming = match mode {
        WriteMode::Append => Some(Streaming::Append),
        WriteMode::Stream => Some(Streaming::Truncate),
        // TTYs and named pipes stream: there is no inode to rename.
        WriteMode::Write => target
            .stat()
            .is_some_and(|s| s.kind != Kind::File)
            .then_some(Streaming::Truncate),
    };
    let (file, mut opened) = if let Some(access) = streaming {
        let mut streamed = target.open_streaming(access).map_err(io)?;
        let file = streamed.file().try_clone().map_err(io)?;
        (file, OpenedWrite::Stream(streamed))
    } else {
        let mut staged = target.stage().map_err(io)?;
        // The writer's handle shares the staging handle's offset.
        let file = staged.file().try_clone().map_err(io)?;
        (file, OpenedWrite::Atomic(staged))
    };
    let before = heard.then(|| opened.before(shell)).flatten();
    if let (OpenedWrite::Stream(streamed), Some(Streaming::Truncate)) = (&opened, streaming) {
        streamed.truncate().map_err(io)?;
    }
    Ok((file, Some(opened), before))
}

/// Open a `< path` target, against the scoped cwd as [`open_write`] does.
fn open_read(path: &str, shell: &mut Shell) -> Settled<File> {
    let rp = shell.resolve(path);
    let opened = if rp.is_discard() {
        crate::path::open_discard()
    } else {
        shell.locate::<Reading>(&rp)?.read()
    };
    Ok(opened.map_err(|e| Error::io(path, &e))?)
}

impl Shell {
    /// Overwrite `path` through core's whole `>`-redirect recipe —
    /// symlink-resolved, mode-preserving, fsync-durable — while emitting no io
    /// event: the door for a host builtin (exarch's `edit-hash` /
    /// `edit-replace`) that writes *below* the redirect frame and speaks its
    /// own surface, so that it shares the recipe instead of forking a weaker
    /// write that narrows the mode, replaces symlinks, and skips the flush.
    ///
    /// # Errors
    /// Returns `Err` if the target cannot be opened, the write fails, or the
    /// atomic commit (rename and fsync) fails.
    pub fn atomic_write(&mut self, path: &str, bytes: &[u8]) -> Settled<()> {
        use std::io::Write as _;
        let (mut file, opened, _) = open_write(path, WriteMode::Write, false, self)?;
        file.write_all(bytes).map_err(|e| Error::io(path, &e))?;
        drop(file);
        if let Some(OpenedWrite::Atomic(staged)) = opened {
            staged
                .publish()
                .map_err(|e| Error::io("atomic write", &e))?;
        }
        Ok(())
    }
}

/// Park the stdin redirect — `< file` or the here-string `<< str` — on
/// `shell.io.stdin`, returning a [`StdinRedirectGuard`] that puts back
/// whatever `Source` was there.
///
/// `<< str` drops one leading newline, so a body may start on the line below
/// the command, and pushes the payload through a pipe from a detached thread:
/// a payload past the kernel buffer would otherwise deadlock, and a consumer
/// that stops reading merely leaves that thread a broken pipe.
///
/// Routing through `Source` rather than `dup2` is what keeps the cached
/// `startup_stdin_tty` honest — consumers trust it only when `Source` is
/// `Terminal`, which then really does mean the inherited fd 0.
pub(crate) fn install_stdin_redirect(
    stdin: Option<&StdinSource<String>>,
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<StdinRedirectGuard> {
    let Some(stdin) = stdin else {
        return Ok(StdinRedirectGuard::Untouched);
    };
    let source = match stdin {
        StdinSource::File(path) => {
            let f = open_read(path, shell)?;
            // Door 1 — READ, recorded eagerly so it precedes the body or
            // exec it feeds, as in `cat < a`.
            shell.observe(mooring, Observed::Read(Read { path: path.clone() }));
            crate::io::Source::Reader(crate::io::SourceReader::file(f))
        }
        StdinSource::Here(word) => {
            let body = word
                .strip_prefix("\r\n")
                .or_else(|| word.strip_prefix('\n'))
                .unwrap_or(word);
            let (reader, mut writer) =
                crate::process::cloexec_pipe().map_err(|e| Error::io("here-string", &e))?;
            let bytes = body.as_bytes().to_vec();
            std::thread::Builder::new()
                .name("ral-here-string".into())
                .spawn(move || {
                    use std::io::Write;
                    // A broken pipe is the consumer stopping early, not a fault.
                    let _ = writer.write_all(&bytes);
                })
                .map_err(|e| Error::io("here-string", &e))?;
            crate::io::Source::Reader(crate::io::SourceReader::pipe(reader))
        }
    };
    let prior = std::mem::replace(&mut shell.io.stdin, source);
    Ok(StdinRedirectGuard::Installed(prior))
}

/// Restore-on-exit token for [`install_stdin_redirect`].
pub(crate) enum StdinRedirectGuard {
    Untouched,
    Installed(crate::io::Source),
}

impl StdinRedirectGuard {
    pub(crate) fn restore(self, shell: &mut Shell) {
        if let Self::Installed(prior) = self {
            shell.io.stdin = prior;
        }
    }
}
