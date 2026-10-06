//! Opening redirect targets: the atomic `>` recipe and the `< file` handoff
//! into `shell.io.stdin`.  The frames in `evaluator::redirect` drive both.
//!
//! No open lives here.  Every one goes through [`Shell::locate`] into
//! `path::walk`, so the object authorised is the object the handle then
//! names — the door never re-walks a string the guard has already judged.

use crate::capability::FsOp;
use crate::evaluator::audit::observe;
use crate::path::{Located, walk::Kind};
use crate::syntax::ast::{StdinSource, WriteMode};
use crate::types::{Break, Error, Mooring, Observed, Settled, Shell};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read as _;

fn io_error(ctx: &str, e: &std::io::Error) -> Break {
    let msg = match e.kind() {
        std::io::ErrorKind::NotFound => format!("{ctx}: no such file or directory"),
        std::io::ErrorKind::PermissionDenied => format!("{ctx}: permission denied"),
        _ => format!("{ctx}: {e}"),
    };
    Break::Error(Error::new(msg, 1))
}

/// An atomic `>` staged in a temp file beside its target, until
/// [`commit`](Self::commit) renames it, and `Drop` unlinks it
/// if nobody committed: a staged temp never outlives the frame that staged it.
///
/// The `Option` is the commit-by-value latch: `commit` takes `self` and
/// empties it first, so the `Drop` that follows sees `None` and does
/// nothing on the path that already decided.
///
/// Staging, commit and rollback all act relative to the directory handle the
/// door located, so nothing between the judgment and the rename can rename
/// the directory out from under the write.  The rename mints a fresh inode:
/// hardlinks to the old one keep the old contents, and owner, xattrs and ACLs
/// fall to kernel inheritance.  Concurrent writers race as usual — this buys
/// crash safety, not exclusion.
pub(crate) struct PendingWrite(Option<Staged>);

struct Staged {
    target: Located,
    tmp: OsString,
}

impl PendingWrite {
    fn new(target: Located, tmp: OsString) -> Self {
        Self(Some(Staged { target, tmp }))
    }

    /// Finish the write: flush the staged bytes, rename onto the target, then
    /// fsync the directory entry.  Any failure unlinks the temp, so the target
    /// is either replaced whole or left exactly as it was.
    ///
    /// # Errors
    /// Returns the first I/O error of the flush or the rename.
    pub(crate) fn commit(mut self) -> std::io::Result<()> {
        let staged = self
            .0
            .take()
            .expect("commit consumes a freshly staged write");
        if let Err(e) = staged.rename_durable() {
            staged.unlink();
            return Err(e);
        }
        Ok(())
    }

    /// The staged file whole: what lands at the target if `commit` succeeds.
    fn staged(&self) -> Option<Vec<u8>> {
        let staged = self.0.as_ref()?;
        read_capped(staged.target.sibling_read(&staged.tmp).ok()?)
    }
}

/// A write redirect's target as opened, held until its frame settles so the
/// frame can report what the write did to it.
pub(crate) enum OpenedWrite {
    /// `>` onto a regular file, staged beside it until commit.
    Atomic(PendingWrite),
    /// Every other shape: bytes land as they are written.
    Stream(Located),
}

impl OpenedWrite {
    /// What stands at the target now: [`snapshot`] of it.
    pub(crate) fn before(&self, shell: &Shell) -> Option<Vec<u8>> {
        match self {
            Self::Atomic(p) => snapshot(&p.0.as_ref()?.target, shell),
            Self::Stream(target) => snapshot(target, shell),
        }
    }

    /// What the write leaves standing: the staged file for an atomic `>`,
    /// read before its rename takes it away, the target itself for a stream.
    pub(crate) fn after(&self, shell: &Shell) -> Option<Vec<u8>> {
        match self {
            Self::Atomic(p) => p.staged(),
            Self::Stream(target) => snapshot(target, shell),
        }
    }
}

/// A target's whole content, empty when it does not exist.
///
/// `None` where it cannot be read whole: past [`PREVIEW_CAP`], not a regular
/// file, or not ours to read under the live grant.  Never a prefix — a card
/// reads a write as the change it made, and half a side is not a change.
fn snapshot(target: &Located, shell: &Shell) -> Option<Vec<u8>> {
    if !shell.admits_fs_exact(&FsOp::Read, target.real()) {
        return None;
    }
    match target.stat().ok()? {
        None => Some(Vec::new()),
        Some(s) if s.kind == Kind::File && s.len <= PREVIEW_CAP => {
            read_capped(target.read().ok()?)
        }
        Some(_) => None,
    }
}

/// `None` past [`PREVIEW_CAP`], judged on the open handle so the size read
/// is the size of the bytes read.
fn read_capped(mut file: File) -> Option<Vec<u8>> {
    if file.metadata().ok()?.len() > PREVIEW_CAP {
        return None;
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}

impl Drop for PendingWrite {
    fn drop(&mut self) {
        if let Some(staged) = self.0.take() {
            staged.unlink();
        }
    }
}

impl Staged {
    /// Drop the staged bytes, leaving the target as it was.
    fn unlink(&self) {
        let _ = self.target.remove_sibling(&self.tmp);
    }

    fn rename_durable(&self) -> std::io::Result<()> {
        // Data blocks before any directory entry, or a crash can commit the
        // entry with the blocks still unwritten — a renamed zero-length file.
        // Opened for writing, not reading: Windows' `FlushFileBuffers` refuses
        // a read-only handle.
        self.target.sibling_write(&self.tmp)?.sync_all()?;
        // On Windows this is `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which
        // *fails* rather than swapping when another process holds the target
        // open without delete sharing, so a live reader turns a silent success
        // into a hard error.
        self.target.rename_sibling_over(&self.tmp)?;
        // Without the parent fsync a panic can roll the directory entry back.
        // Errors don't unwind the rename, and Windows has no directory flush,
        // so both just fall through.
        let _ = self.target.sync_dir();
        Ok(())
    }
}

/// Cap on the bytes a write's snapshot reads, so a large file is never pulled
/// into memory whole to show a change.
const PREVIEW_CAP: u64 = 64 * 1024;

/// Stage an atomic `>` beside its located target.  `existing` is the
/// target's stat when it exists: its mode is carried onto the staged file,
/// since a redirect must not silently narrow a 0644 file; a new file gets
/// what `open(2)` would have created under the umask.
fn open_atomic(
    path: &str,
    target: Located,
    existing: Option<&crate::path::walk::Stat>,
) -> Settled<(File, PendingWrite)> {
    let (file, tmp) = target
        .create_sibling_tmp()
        .map_err(|e| io_error(path, &e))?;
    // Latched before the first fallible step, so an early `?` unlinks it.
    let pending = PendingWrite::new(target, tmp);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = existing.map_or_else(
            || {
                // `rustix::process::umask` has no read-only form: set a
                // throwaway value to read the mask, then put it back.
                let prev = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
                rustix::process::umask(prev);
                #[allow(clippy::useless_conversion)] // libc::mode_t is u16 on macOS/BSD
                let mask = u32::from(prev.as_raw_mode());
                0o666 & !mask
            },
            |s| s.mode,
        );
        file.set_permissions(std::fs::Permissions::from_mode(mode))
            .map_err(|e| io_error(path, &e))?;
    }
    #[cfg(not(unix))]
    let _ = existing;
    Ok((file, pending))
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
    let opened = |file: std::io::Result<File>| file.map_err(|e| io_error(path, &e));
    let rp = shell.resolve(path);
    if rp.is_discard() {
        return opened(crate::path::walk::open_discard(&rp)).map(|f| (f, None));
    }
    let target = shell.locate(&rp, &FsOp::Write)?;
    let stream = |file, target| opened(file).map(|f| (f, Some(OpenedWrite::Stream(target))));
    match mode {
        WriteMode::Append => stream(target.append(), target),
        WriteMode::Stream => stream(target.truncate(), target),
        WriteMode::Write => match target.stat().map_err(|e| io_error(path, &e))? {
            // TTYs and named pipes stream: there is no inode to rename.
            Some(s) if s.kind != Kind::File => stream(target.truncate(), target),
            existing => {
                let (file, commit) = open_atomic(path, target, existing.as_ref())?;
                Ok((file, Some(OpenedWrite::Atomic(commit))))
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
    opened.map_err(|e| io_error(path, &e))
}

/// The whole `>` recipe with no observation — the caller owns the surface.
/// exarch's `edit-hash` and `edit-replace` write below the redirect frame and
/// speak their own card, so they share this door rather than fork a weaker
/// temp-file write that drops the symlink, mode and fsync steps.
pub(crate) fn atomic_write(path: &str, bytes: &[u8], shell: &mut Shell) -> Settled<()> {
    use std::io::Write as _;
    let (mut file, opened) = open_write(path, WriteMode::Write, shell)?;
    file.write_all(bytes).map_err(|e| io_error(path, &e))?;
    match opened {
        Some(OpenedWrite::Atomic(commit)) => commit.commit().map_err(|e| atomic_write_error(&e)),
        _ => Ok(()),
    }
}

/// Shared by every atomic `>` commit failure.
pub(crate) fn atomic_write_error(e: &std::io::Error) -> Break {
    Break::Error(Error::new(format!("atomic write: {e}"), 1))
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
            observe(shell, mooring, Observed::Read { path: path.clone() });
            crate::io::Source::Reader(crate::io::SourceReader::file(f))
        }
        StdinSource::Here(word) => {
            let body = word
                .strip_prefix("\r\n")
                .or_else(|| word.strip_prefix('\n'))
                .unwrap_or(word);
            let (reader, mut writer) = crate::process::cloexec_pipe()
                .map_err(|e| Break::Error(Error::new(format!("here-string: {e}"), 1)))?;
            let bytes = body.as_bytes().to_vec();
            std::thread::Builder::new()
                .name("ral-here-string".into())
                .spawn(move || {
                    use std::io::Write;
                    // A broken pipe is the consumer stopping early, not a fault.
                    let _ = writer.write_all(&bytes);
                })
                .map_err(|e| Break::Error(Error::new(format!("here-string: {e}"), 1)))?;
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
