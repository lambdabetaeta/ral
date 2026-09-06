//! Opening redirect targets: the atomic `>` recipe and the `< file` handoff
//! into `shell.io.stdin`.  The frames in `evaluator::redirect` drive both.
//!
//! Every open here goes through [`Shell::locate`], so the object authorised
//! is the object the handle then names — the door never re-walks a string
//! the gate has already judged.

use crate::capability::FsOp;
use crate::evaluator::audit::observe;
use crate::path::{Located, ResolvedPath};
use crate::syntax::ast::RedirectMode;
use crate::types::{Break, Error, Mooring, Observed, Settled, Shell};
use std::ffi::OsString;
use std::fs::File;
use std::io::Read as _;

/// A redirect target resolved to a concrete path or fd.
#[derive(Clone, Debug)]
pub(crate) enum EvalRedirect {
    File(String),
    Fd(u32),
}

/// The runtime counterpart of the IR's [`crate::ir::RedirectV`], as
/// `evaluator::redirect::eval_redirects` resolves it.
#[derive(Clone, Debug)]
pub(crate) struct EvalRedirectV {
    pub(crate) fd: u32,
    pub(crate) mode: RedirectMode,
    pub(crate) target: EvalRedirect,
}

/// `>` on stderr streams instead: staging diagnostics for an atomic commit
/// would withhold them until the frame settles.
pub(crate) fn stderr_mode(mode: RedirectMode) -> RedirectMode {
    match mode {
        RedirectMode::Write => RedirectMode::StreamWrite,
        other => other,
    }
}

fn io_error(ctx: &str, e: &std::io::Error) -> Break {
    let msg = match e.kind() {
        std::io::ErrorKind::NotFound => format!("{ctx}: no such file or directory"),
        std::io::ErrorKind::PermissionDenied => format!("{ctx}: permission denied"),
        _ => format!("{ctx}: {e}"),
    };
    Break::Error(Error::new(msg, 1))
}

/// An atomic `>` staged in a temp file beside its target, until
/// [`commit`](Self::commit) renames it or [`abandon`](Self::abandon) unlinks
/// it — and `Drop` does the latter too, for whichever of the two nobody
/// called: a staged temp never outlives the frame that staged it.
///
/// The `Option` is the commit-by-value latch: `commit`/`abandon` each take
/// `self` and empty it first, so the `Drop` that follows sees `None` and
/// does nothing on the path that already decided.
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
        let staged = self.0.take().expect("commit consumes a freshly staged write");
        if let Err(e) = staged.rename_durable() {
            staged.unlink();
            return Err(e);
        }
        Ok(())
    }

    /// Drop the staged bytes, leaving the target as it was.
    pub(crate) fn abandon(mut self) {
        if let Some(staged) = self.0.take() {
            staged.unlink();
        }
    }

    /// The whole staged file: what will land at the target if `commit`
    /// succeeds.
    ///
    /// `None` past [`PREVIEW_CAP`], never a prefix. A card shows a write as the
    /// change it made, and a change cannot be read off part of one side — a
    /// truncated preview would describe a file that never existed. Past the cap
    /// the write is reported and not shown.
    pub(crate) fn new_snapshot_for_diff(&self) -> Option<Vec<u8>> {
        let staged = self.0.as_ref()?;
        read_capped(staged.target.sibling_read(&staged.tmp).ok()?)
    }

    /// The target's content before the rename — untouched until `commit`, so
    /// the write card can diff against it.
    ///
    /// A before-image is a *read*, and is taken only where the live grant
    /// admits one: under a write-only grant the write still lands, and the
    /// card simply has no old side.  `Some` means the before-image is
    /// *known*, and a file that does not yet exist has a known before-image:
    /// the empty one, against which the write reads as every line added.
    /// `None` is reserved for a target that exists and cannot be read whole —
    /// past [`PREVIEW_CAP`], on the same ground as
    /// [`Self::new_snapshot_for_diff`], or not ours to read. Keeping the two
    /// apart is what stops a card diffing an overwrite against nothing and
    /// claiming the file was created.
    pub(crate) fn old_snapshot_for_diff(&self, shell: &Shell) -> Option<Vec<u8>> {
        let staged = self.0.as_ref()?;
        if !shell.admits_fs_exact(&FsOp::Read, staged.target.real()) {
            return None;
        }
        match staged.target.stat().ok()? {
            None => Some(Vec::new()),
            Some(s) if s.len <= PREVIEW_CAP => read_capped(staged.target.read().ok()?),
            Some(_) => None,
        }
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

/// Cap on the bytes read to seed a write card's preview, so a large write is
/// never pulled into memory whole to show a head.
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

/// The discard device is exempt from the grant and from the walk alike: there
/// is no object to locate, and on Windows `NUL` is a name the Win32 layer
/// resolves anywhere rather than an entry in any directory.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:discard-device] `/dev/null` / `NUL` opened by name: no bytes reach or leave the model, and no grant region can contain a device that is not a file."
)]
fn open_discard(rp: &ResolvedPath) -> std::io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(rp.as_path())
}

/// Open a redirect target.  `>` to a regular file returns a [`PendingWrite`]
/// the caller must commit or abandon once the writer finishes; every other
/// shape streams.
/// Paths resolve against the shell's scoped cwd, so a `within [dir: …]`
/// redirect lands right even from a native, where the host cwd never moves.
pub(crate) fn open_file(
    path: &str,
    mode: RedirectMode,
    shell: &mut Shell,
) -> Settled<(File, Option<PendingWrite>)> {
    let rp = shell.resolve(path);
    let stream = |opened: std::io::Result<File>| {
        opened.map(|f| (f, None)).map_err(|e| io_error(path, &e))
    };
    if rp.is_discard() {
        return stream(open_discard(&rp));
    }
    let op = match mode {
        RedirectMode::Read => FsOp::Read,
        _ => FsOp::Write,
    };
    let target = shell.locate(&rp, &op)?;
    match mode {
        RedirectMode::Read => stream(target.read()),
        RedirectMode::Append => stream(target.append()),
        RedirectMode::StreamWrite => stream(target.truncate()),
        // The payload is the redirect word itself; `install_stdin_redirect`
        // routes it without touching the filesystem.
        RedirectMode::HereString => {
            unreachable!("here-string redirects never reach the file-open door")
        }
        RedirectMode::Write => {
            let existing = target.stat().map_err(|e| io_error(path, &e))?;
            // TTYs and named pipes stream: there is no inode to rename.
            match existing {
                Some(s) if !s.is_file => stream(target.truncate()),
                existing => {
                    let (file, commit) = open_atomic(path, target, existing.as_ref())?;
                    Ok((file, Some(commit)))
                }
            }
        }
    }
}

/// The whole `>` recipe with no observation — the caller owns the surface.
/// exarch's `edit-hash` and `edit-replace` write below the redirect frame and
/// speak their own card, so they share this door rather than fork a weaker
/// temp-file write that drops the symlink, mode and fsync steps.
pub(crate) fn atomic_write(path: &str, bytes: &[u8], shell: &mut Shell) -> Settled<()> {
    use std::io::Write as _;
    let (mut file, commit) = open_file(path, RedirectMode::Write, shell)?;
    file.write_all(bytes).map_err(|e| io_error(path, &e))?;
    match commit {
        Some(commit) => commit
            .commit()
            .map_err(|e| Break::Error(Error::new(format!("atomic write: {e}"), 1))),
        None => Ok(()),
    }
}

/// Park the fd-0 redirect — `< file` or the here-string `<< str` — on
/// `shell.io.stdin`, returning a [`StdinRedirectGuard`] that puts back
/// whatever `Source` was there.  When several redirects name fd 0 the last
/// wins, as in POSIX shells.
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
    redirects: &[EvalRedirectV],
    mooring: &Mooring,
    shell: &mut Shell,
) -> Settled<StdinRedirectGuard> {
    let Some((mode, word)) = redirects.iter().rev().find_map(|r| match r {
        EvalRedirectV {
            fd: 0,
            mode: mode @ (RedirectMode::Read | RedirectMode::HereString),
            target: EvalRedirect::File(w),
        } => Some((mode, w)),
        _ => None,
    }) else {
        return Ok(StdinRedirectGuard::Untouched);
    };
    let source = match mode {
        RedirectMode::Read => {
            let (f, _) = open_file(word, RedirectMode::Read, shell)?;
            // Door 1 — READ, recorded eagerly so it precedes the body or
            // exec it feeds, as in `cat < a`.
            observe(shell, mooring, Observed::Read { path: word.clone() });
            crate::io::Source::Reader(crate::io::SourceReader::file(f))
        }
        RedirectMode::HereString => {
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
        _ => unreachable!("find_map above only yields Read or HereString"),
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
