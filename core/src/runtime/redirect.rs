//! Redirects: the doors that open a target (the atomic `>` recipe, the
//! `< file` handoff into `shell.io.stdin`), and the [`scope`] that installs a
//! redirect list around a body and settles it.
//!
//! No open lives here.  Every one goes through [`Shell::locate`] into
//! `path::walk`, so the object authorised is the object the handle then
//! names — the door never re-walks a string the guard has already judged.
//! Staging the write is pure and lives with the path (`path::stage`).

pub(crate) mod scope;

use crate::capability::FsOp;
use crate::fact::Read;
use crate::ir::{StdinSource, WriteMode};
use crate::path::stage::PendingWrite;
use crate::path::{Located, walk::Kind};
use crate::types::{Break, Error, Mooring, Observed, Settled, Shell};
use std::fs::File;

/// A write redirect's target as opened, held until its frame settles so the
/// frame can report what the write did to it.
pub(crate) enum OpenedWrite {
    /// `>` onto a regular file, staged beside it until commit.
    Atomic(PendingWrite),
    /// Every other shape: bytes land as they are written.
    Stream(Located),
}

impl OpenedWrite {
    fn target(&self) -> Option<&Located> {
        match self {
            Self::Atomic(pending) => pending.target(),
            Self::Stream(target) => Some(target),
        }
    }

    /// What stands at the target now: [`snapshot`] of it.
    pub(crate) fn before(&self, shell: &Shell) -> Option<Vec<u8>> {
        snapshot(self.target()?, shell)
    }

    /// What the write leaves standing: the staged file for an atomic `>`,
    /// read before its rename takes it away, the target itself for a stream.
    pub(crate) fn after(&self, shell: &Shell) -> Option<Vec<u8>> {
        match self {
            Self::Atomic(pending) => pending.staged(),
            Self::Stream(target) => snapshot(target, shell),
        }
    }
}

/// A target's [`Located::preview`], where it is ours to read under the live
/// grant.
fn snapshot(target: &Located, shell: &Shell) -> Option<Vec<u8>> {
    shell
        .admits_fs_exact(&FsOp::Read, target.real())
        .then(|| target.preview())
        .flatten()
}

/// Open a write redirect's target.  `>` to a regular file is staged, to be
/// committed or dropped once the writer finishes; every other shape streams.
/// `None` is the discard device, which no write changes.
/// Paths resolve against the shell's scoped cwd, so a `within [dir: …]`
/// redirect lands right even from a native, where the host cwd never moves.
pub(crate) fn open_write(
    path: &str,
    mode: WriteMode,
    shell: &mut Shell,
) -> Settled<(File, Option<OpenedWrite>)> {
    let opened = |file: std::io::Result<File>| file.map_err(|e| Break::from(Error::io(path, &e)));
    let rp = shell.resolve(path);
    if rp.is_discard() {
        return opened(crate::path::walk::open_discard(&rp)).map(|f| (f, None));
    }
    let target = shell.locate(&rp, &FsOp::Write)?;
    let stream = |file, target| opened(file).map(|f| (f, Some(OpenedWrite::Stream(target))));
    match mode {
        WriteMode::Append => stream(target.append(), target),
        WriteMode::Stream => stream(target.truncate(), target),
        WriteMode::Write => match target.stat().map_err(|e| Error::io(path, &e))? {
            // TTYs and named pipes stream: there is no inode to rename.
            Some(s) if s.kind != Kind::File => stream(target.truncate(), target),
            existing => {
                let (file, pending) = target
                    .stage(existing.as_ref())
                    .map_err(|e| Error::io(path, &e))?;
                Ok((file, Some(OpenedWrite::Atomic(pending))))
            }
        },
    }
}

/// Open a `< path` target, against the scoped cwd as [`open_write`] does.
fn open_read(path: &str, shell: &mut Shell) -> Settled<File> {
    let rp = shell.resolve(path);
    let opened = if rp.is_discard() {
        crate::path::walk::open_discard(&rp)
    } else {
        shell.locate(&rp, &FsOp::Read)?.read()
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
        let (mut file, opened) = open_write(path, WriteMode::Write, self)?;
        file.write_all(bytes).map_err(|e| Error::io(path, &e))?;
        if let Some(OpenedWrite::Atomic(pending)) = opened {
            pending
                .commit()
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
