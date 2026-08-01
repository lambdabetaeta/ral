//! Opening redirect targets: the atomic `>` recipe, the fd-level path for the
//! shapes a `Sink` cannot model, and the `< file` handoff into `shell.io.stdin`.
//! The frames in `evaluator::redirect` drive all three.

use crate::syntax::ast::RedirectMode;
use crate::types::{Break, Error, Mooring, Settled, Shell};

use super::io_event;

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

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};

/// Pending tmp-to-target rename for an atomic `>`.  `commit` renames; `Drop`
/// discards the tmp, so an abandoned or failed redirect leaves the target
/// exactly as it was.
///
/// The rename mints a fresh inode: hardlinks to the old one keep the old
/// contents, and owner, xattrs and ACLs fall to kernel inheritance.  It also
/// needs write permission on the *parent directory*, not just on the file.
/// Concurrent writers race as usual — this buys crash safety, not exclusion.
pub(crate) struct AtomicCommit {
    tmp: tempfile::NamedTempFile,
    target: std::path::PathBuf,
}

/// Cap on the bytes read to seed a write card's preview, so a large write is
/// never pulled into memory whole to show a head.
const PREVIEW_CAP: u64 = 64 * 1024;

impl AtomicCommit {
    /// The head of the staged file: what will land at `target` if `commit`
    /// succeeds.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:surface:atomic-temp-read] Sub-step of the atomic `>` write door's preview surface: read the head of the tmp file before the rename commits it. The write card surfaces when the redirect frame settles; this read is not a separate model operation."
    )]
    pub(crate) fn temp_preview(&self) -> Option<Vec<u8>> {
        use std::io::Read;
        let mut buf = Vec::new();
        std::fs::File::open(self.tmp.path())
            .ok()?
            .take(PREVIEW_CAP)
            .read_to_end(&mut buf)
            .ok()?;
        Some(buf)
    }

    /// The target's content before the rename — untouched until `commit`, so
    /// the write card can diff against it.  `None` for a new file, and when
    /// either side exceeds [`PREVIEW_CAP`]: a diff of truncated prefixes would
    /// describe a change that never landed.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:surface:atomic-old-read] Sub-step of the atomic `>` write door's diff-eligibility check: stat and read the target's pre-existing content before the rename commits it, exactly mirroring temp_preview's read of the new side. Not a separate model operation — the write/diff card surfaces when the redirect frame settles."
    )]
    pub(crate) fn old_snapshot_for_diff(&self) -> Option<Vec<u8>> {
        let old_meta = std::fs::metadata(&self.target).ok()?;
        if old_meta.len() > PREVIEW_CAP {
            return None;
        }
        let new_meta = self.tmp.as_file().metadata().ok()?;
        if new_meta.len() > PREVIEW_CAP {
            return None;
        }
        std::fs::read(&self.target).ok()
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:surface:atomic-commit] The atomic `>` write door's commit step: re-open the target's parent directory only to fsync the rename durable. The write surface fires when the redirect frame settles (committed once this returns Ok); this open carries written bytes to disk, it is not a separate model read."
    )]
    pub fn commit(self) -> std::io::Result<()> {
        // Data blocks before any directory entry, or a crash can commit the
        // entry with the blocks still unwritten — a renamed zero-length file.
        self.tmp.as_file().sync_all()?;
        // `persist` is `rename(2)`; on Windows it is
        // `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which *fails* rather than
        // swapping when another process holds the target open without delete
        // sharing, so a live reader turns a silent success into a hard error.
        self.tmp
            .persist(&self.target)
            .map(|_| ())
            .map_err(|e| e.error)?;
        // Without the parent fsync a panic can roll the directory entry back.
        // Errors don't unwind the rename, and Windows has no directory handle
        // to flush, so both just fall through.
        if let Ok(dir) = std::fs::File::open(crate::path::parent_or_cwd(&self.target)) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

/// True for new paths and regular files.  TTYs, `/dev/null` and named pipes
/// stream instead — there is no inode to rename.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:atomic-eligible] Stat of the `>` write door's target to choose atomic vs. streaming semantics. A sub-step of the surfacing write door (open_file), not a model read; the write card is the operation's surface."
)]
fn atomic_eligible(path: &std::path::Path) -> bool {
    match std::fs::metadata(path) {
        Ok(meta) => meta.file_type().is_file(),
        Err(_) => true,
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:open-atomic] The `>` write door's tmp-file recipe: stat the target only to preserve its mode onto the tmp. A sub-step of the surfacing write door (open_file); the write card is the operation's surface, not this mode-preservation stat."
)]
fn open_atomic(
    path: &str,
    target: &crate::path::ResolvedPath,
) -> Settled<(std::fs::File, AtomicCommit)> {
    // Resolve symlinks, or the rename replaces the link itself with a regular
    // file.  `canonicalise_strict` errors on a path that does not exist yet;
    // for a fresh file the literal target is the right one.
    let target = target
        .canonicalise_strict()
        .unwrap_or_else(|_| target.as_path().to_path_buf());
    // Stage beside the target: `rename(2)` is atomic only within one
    // filesystem.  `parent_or_cwd` reads `Path::parent`'s `Some("")` for a
    // bare filename as the cwd.
    let parent = crate::path::parent_or_cwd(&target);
    // A random name, not `target.tmp`: a predictable one races concurrent
    // writers in a shared directory.  Dot-prefixed to hide it; `O_EXCL`
    // inside, and RAII removes it on an early return.
    let tmp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".ral-write.tmp")
        .tempfile_in(parent)
        .map_err(|e| io_error(path, &e))?;
    // tempfile creates at 0600, and a redirect must not silently narrow a
    // 0644 file.  Carry the target's mode over; for a new file mirror what
    // `open(2)` would have created.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target).ok().map_or_else(
            || {
                // `rustix::process::umask` has no read-only form: set a
                // throwaway value to read the mask, then put it back.
                let prev = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o022));
                rustix::process::umask(prev);
                #[allow(clippy::useless_conversion)] // libc::mode_t is u16 on macOS/BSD
                let mask = u32::from(prev.as_raw_mode());
                0o666 & !mask
            },
            |m| m.permissions().mode() & 0o7777,
        );
        let mut perms = tmp
            .as_file()
            .metadata()
            .map_err(|e| io_error(path, &e))?
            .permissions();
        perms.set_mode(mode);
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| io_error(path, &e))?;
    }
    let file = tmp.as_file().try_clone().map_err(|e| io_error(path, &e))?;
    Ok((file, AtomicCommit { tmp, target }))
}

/// Open a redirect target.  `>` to a regular file returns an [`AtomicCommit`]
/// the caller must fire once the writer finishes; every other shape streams.
/// Paths resolve against the shell's scoped cwd, so a `within [dir: …]`
/// redirect lands right even from a native, where the host cwd never moves.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:open-file] The redirect read/write door for `<`/`>`/`>>`/`>~` on fd 1/2: every model redirect opens here. The read/write card is fused onto the operation by the redirect frame that wraps this open (committed/aborted/failed outcome on settle)."
)]
pub(crate) fn open_file(
    path: &str,
    mode: RedirectMode,
    shell: &mut Shell,
) -> Settled<(std::fs::File, Option<AtomicCommit>)> {
    let rp = shell.resolve(path);
    match mode {
        RedirectMode::Read => shell.check_fs_read(&rp)?,
        _ => shell.check_fs_write(&rp)?,
    }
    match mode {
        RedirectMode::Append => std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(rp.as_path())
            .map(|f| (f, None))
            .map_err(|e| io_error(path, &e)),
        RedirectMode::StreamWrite => std::fs::File::create(rp.as_path())
            .map(|f| (f, None))
            .map_err(|e| io_error(path, &e)),
        RedirectMode::Read => std::fs::File::open(rp.as_path())
            .map(|f| (f, None))
            .map_err(|e| io_error(path, &e)),
        // The payload is the redirect word itself; `install_stdin_redirect`
        // routes it without touching the filesystem.
        RedirectMode::HereString => {
            unreachable!("here-string redirects never reach the file-open door")
        }
        RedirectMode::Write => {
            if atomic_eligible(rp.as_path()) {
                let (file, commit) = open_atomic(path, &rp)?;
                Ok((file, Some(commit)))
            } else {
                std::fs::File::create(rp.as_path())
                    .map(|f| (f, None))
                    .map_err(|e| io_error(path, &e))
            }
        }
    }
}

/// The whole `>` recipe with no io event — the caller owns the surface.
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

/// One descriptor `F_DUPFD_CLOEXEC`'d aside before [`apply_redirects`] `dup2`'d
/// over `fd`.  Restoration is RAII, so a [`RedirectGuard`] left half-built by a
/// `?` still reverses every `dup2` already installed.
#[cfg(unix)]
pub(crate) struct BackupFd {
    pub(crate) fd: u32,
    pub(crate) backup: std::os::fd::OwnedFd,
}

#[cfg(unix)]
impl Drop for BackupFd {
    fn drop(&mut self) {
        // A failure here has nowhere to go; `backup` closes itself after.
        use std::os::fd::AsFd;
        #[allow(
            clippy::cast_possible_wrap,
            reason = "fd is a non-negative descriptor number (u32 by design); the libc boundary wants c_int and the value never sets the top bit, so no wrap"
        )]
        let _ = unsafe { crate::process::clobber_slot(self.backup.as_fd(), self.fd as i32) };
    }
}

/// Windows analog of `BackupFd`: `Drop` puts `prev` back in the std slot,
/// then closes `owned` — the handle we opened for a file target, mirroring the
/// Unix `close(raw)`.  An `Fd→Fd` merge (`2>&1`) aliases a live slot's handle
/// and so leaves `owned` empty.
#[cfg(windows)]
pub(crate) struct BackupHandle {
    pub(crate) std_handle: u32,
    pub(crate) prev: HANDLE,
    pub(crate) owned: Option<HANDLE>,
}

#[cfg(windows)]
impl Drop for BackupHandle {
    fn drop(&mut self) {
        // Restore the slot first, so the owned handle is unreachable through
        // any std slot before we close it.  Failures have nowhere to go.
        unsafe {
            SetStdHandle(self.std_handle, self.prev);
            if let Some(h) = self.owned {
                CloseHandle(h);
            }
        }
    }
}

/// Undo state for [`apply_redirects`]: backups to restore, commits to fire.
pub(crate) struct RedirectGuard {
    #[cfg(unix)]
    saved: Vec<BackupFd>,
    #[cfg(windows)]
    saved: Vec<BackupHandle>,
    commits: Vec<AtomicCommit>,
}

/// Pop, rather than let `Vec::drop` unwind forwards: overlapping `dup2`s — a
/// swap of fd 1 and fd 2 — only compose back correctly in reverse.
#[cfg(unix)]
impl Drop for RedirectGuard {
    fn drop(&mut self) {
        while let Some(_b) = self.saved.pop() {
            // The pop is the work: `BackupFd::Drop` does the dup2 and close.
        }
    }
}

/// Windows analog, LIFO for the same reason.
#[cfg(windows)]
impl Drop for RedirectGuard {
    fn drop(&mut self) {
        while let Some(_b) = self.saved.pop() {
            // The pop is the work: `BackupHandle::Drop` restores the slot.
        }
    }
}

/// fd-0 file redirects belong to `shell.io.stdin` via
/// [`install_stdin_redirect`]; `apply_redirects` here and `wire_stdin` in
/// `stdio.rs` must both leave them alone.
fn is_stdin_file_redirect(r: &EvalRedirectV) -> bool {
    matches!(
        r,
        EvalRedirectV {
            fd: 0,
            mode: RedirectMode::Read | RedirectMode::HereString,
            target: EvalRedirect::File(_)
        }
    )
}

/// Only fds 0, 1 and 2 have a `SetStdHandle` slot, so a higher one is rejected
/// rather than silently dropped.
#[cfg(windows)]
fn fd_to_std_handle(fd: u32) -> Settled<u32> {
    match fd {
        0 => Ok(STD_INPUT_HANDLE),
        1 => Ok(STD_OUTPUT_HANDLE),
        2 => Ok(STD_ERROR_HANDLE),
        other => Err(Break::Error(Error::new(
            format!(
                "redirect to fd {other} is not supported for bundled tools on Windows \
                 (only 0, 1, 2 map to a Win32 standard-handle slot)"
            ),
            1,
        ))),
    }
}

/// Install, at the fd level, the shapes no `Sink` can model — fd ≥ 3 targets
/// above all — which is everything the redirect frame in `evaluator::redirect`
/// passes down here.  The guard's commits must be taken out by
/// `restore_redirects` before it drops; fd-0 file redirects are skipped, since
/// [`install_stdin_redirect`] owns them.
#[cfg(unix)]
pub(crate) fn apply_redirects(
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
) -> Settled<RedirectGuard> {
    let mut guard = RedirectGuard {
        saved: Vec::new(),
        commits: Vec::new(),
    };
    for r in redirects {
        if is_stdin_file_redirect(r) {
            continue;
        }
        let EvalRedirectV { fd, mode, target } = r;
        match target {
            EvalRedirect::File(path) => {
                let effective_mode = if *fd == 2 { stderr_mode(*mode) } else { *mode };
                // Safe under `?`: backups already pushed LIFO-restore when
                // `guard` drops on the error return.
                let (file, commit) = open_file(path, effective_mode, shell)?;
                if let Some(c) = commit {
                    guard.commits.push(c);
                }
                // `owned` closes at the end of this arm; the `dup2` has taken
                // its own reference to the open file by then.
                use std::os::fd::AsRawFd;
                let owned: std::os::fd::OwnedFd = file.into();
                install_dup2(owned.as_raw_fd(), *fd, &mut guard)?;
            }
            EvalRedirect::Fd(target_fd) => {
                #[allow(
                    clippy::cast_possible_wrap,
                    reason = "fd is a non-negative descriptor number (u32 by design); the libc boundary wants c_int and the value never sets the top bit, so no wrap"
                )]
                install_dup2(*target_fd as i32, *fd, &mut guard)?;
            }
        }
    }
    Ok(guard)
}

/// Point `dst_fd` at `src_fd`, backing `dst_fd` up first.
///
/// That backup is the only restore path, so when it fails — `EBADF` for a
/// `dst_fd` that was never opened, the normal state for an fd ≥ 3 — we error
/// rather than install a `dup2` that would pin `dst_fd` at the target for the
/// life of the shell.
#[cfg(unix)]
fn install_dup2(src_fd: i32, dst_fd: u32, guard: &mut RedirectGuard) -> Settled<()> {
    use std::os::fd::BorrowedFd;

    // `CLOEXEC`, not a bare `dup`: the backup is guard bookkeeping, and an
    // external spawned from inside the redirected body must not inherit it.
    #[allow(
        clippy::cast_possible_wrap,
        reason = "fd is a non-negative descriptor number (u32 by design); the libc boundary wants c_int and the value never sets the top bit, so no wrap"
    )]
    // SAFETY: `dst_fd` names the slot this redirect is about to overwrite,
    // live for the borrow, which ends with this statement.
    let backup =
        unsafe { rustix::io::fcntl_dupfd_cloexec(BorrowedFd::borrow_raw(dst_fd as i32), 0) }
            .map_err(|e| {
                Break::Error(Error::new(
                    format!(
                        "redirect to fd {dst_fd}: cannot save the existing descriptor \
                 ({e}) — refusing to install a redirect with no restore path"
                    ),
                    1,
                ))
            })?;
    guard.saved.push(BackupFd { fd: dst_fd, backup });
    #[allow(
        clippy::cast_possible_wrap,
        reason = "fd is a non-negative descriptor number (u32 by design); the libc boundary wants c_int and the value never sets the top bit, so no wrap"
    )]
    // SAFETY: the caller holds `src_fd` open across this call — either the
    // just-opened file's `OwnedFd` or a live standard slot.
    if let Err(e) =
        unsafe { crate::process::clobber_slot(BorrowedFd::borrow_raw(src_fd), dst_fd as i32) }
    {
        return Err(Break::Error(Error::new(
            format!("redirect to fd {dst_fd}: dup2 failed ({e})"),
            1,
        )));
    }
    Ok(())
}

/// Windows analog, structurally mirroring the Unix arm.  `SetStdHandle` stands
/// in for `dup2`: Rust's `std::io::stdout()` calls `GetStdHandle` on every
/// write, so replacing the slot retargets writes already in flight.
#[cfg(windows)]
pub(crate) fn apply_redirects(
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
) -> Settled<RedirectGuard> {
    let mut guard = RedirectGuard {
        saved: Vec::new(),
        commits: Vec::new(),
    };
    for r in redirects {
        if is_stdin_file_redirect(r) {
            continue;
        }
        let EvalRedirectV { fd, mode, target } = r;
        let dst_slot = fd_to_std_handle(*fd)?;
        match target {
            EvalRedirect::File(path) => {
                let effective_mode = if *fd == 2 { stderr_mode(*mode) } else { *mode };
                // Safe under `?`: backups already pushed LIFO-restore when
                // `guard` drops on the error return.
                let (file, commit) = open_file(path, effective_mode, shell)?;
                if let Some(c) = commit {
                    guard.commits.push(c);
                }
                use std::os::windows::io::IntoRawHandle;
                let raw = file.into_raw_handle() as HANDLE;
                let prev = unsafe { GetStdHandle(dst_slot) };
                // Push before the set, so `Drop` closes `raw` either way and
                // a set that never happens cannot leak the opened file.
                guard.saved.push(BackupHandle {
                    std_handle: dst_slot,
                    prev,
                    owned: Some(raw),
                });
                unsafe {
                    SetStdHandle(dst_slot, raw);
                }
            }
            EvalRedirect::Fd(target_fd) => {
                let src_slot = fd_to_std_handle(*target_fd)?;
                let prev = unsafe { GetStdHandle(dst_slot) };
                let src_handle = unsafe { GetStdHandle(src_slot) };
                guard.saved.push(BackupHandle {
                    std_handle: dst_slot,
                    prev,
                    owned: None,
                });
                unsafe {
                    SetStdHandle(dst_slot, src_handle);
                }
            }
        }
    }
    Ok(guard)
}

/// Take the commits out before `guard` drops at the end of this call, since
/// that `Drop` is the fd-level restore — which must run whether or not the
/// redirected body succeeded.
pub(crate) fn restore_redirects(mut guard: RedirectGuard) -> Vec<AtomicCommit> {
    std::mem::take(&mut guard.commits)
}

/// Fire pending commits.  Success path only — dropping the `Vec` instead
/// discards each staged tmp.
pub(crate) fn commit_atomics(commits: Vec<AtomicCommit>) -> Settled<()> {
    for commit in commits {
        commit
            .commit()
            .map_err(|e| Break::Error(Error::new(format!("atomic write: {e}"), 1)))?;
    }
    Ok(())
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:stdin-redirect] The `< file` read door on fd 0: opens the model's stdin redirect and emits the read card eagerly (so the read precedes the body/exec it feeds, e.g. `cat < a`). The open IS the surfaced read."
)]
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
            let rp = shell.resolve(word);
            shell.check_fs_read(&rp)?;
            let f = std::fs::File::open(rp.as_path()).map_err(|e| io_error(word, &e))?;
            // Door 1 — READ, emitted eagerly so the read card precedes the
            // body or exec it feeds, as in `cat < a`.
            mooring.emit_io(&io_event::read(word));
            crate::io::Source::File(f)
        }
        RedirectMode::HereString => {
            let body = word
                .strip_prefix("\r\n")
                .or_else(|| word.strip_prefix('\n'))
                .unwrap_or(word);
            let (reader, mut writer) = os_pipe::pipe()
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
            crate::io::Source::Pipe(reader)
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The fd table is process-global; these tests mutate it and assert on it.
    static FD_TABLE: Mutex<()> = Mutex::new(());

    /// `F_GETFD` returns `EBADF` exactly when `fd` is not open.
    fn fd_is_open(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
    }

    /// A free fd ≥ 3 that stays free: pin a barrier well above the low slots
    /// `open_file` allocates into, then target the one just past it.
    fn pinned_unopened_fd() -> (u32, BarrierFd) {
        let barrier = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 32) };
        assert!(barrier >= 0, "could not pin a high barrier fd");
        #[allow(
            clippy::cast_sign_loss,
            reason = "barrier is a fcntl-returned fd asserted >= 0 on the line above, so no sign is lost"
        )]
        let target = barrier as u32 + 1;
        #[allow(
            clippy::cast_possible_wrap,
            reason = "fd is a non-negative descriptor number (u32 by design); the libc boundary wants c_int and the value never sets the top bit, so no wrap"
        )]
        let target_fd = target as i32;
        assert!(!fd_is_open(target_fd), "target fd {target} must start free");
        (target, BarrierFd(barrier))
    }

    struct BarrierFd(i32);
    impl Drop for BarrierFd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    /// `echo hi 3>/tmp/x`: nothing to back up, so the redirect must error
    /// rather than install a `dup2` with no restore path.
    #[test]
    fn file_redirect_to_unopened_fd_errors_and_leaks_nothing() {
        let _serial = FD_TABLE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (fd, _barrier) = pinned_unopened_fd();
        let mut shell = Shell::default();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("x").to_string_lossy().into_owned();
        let redirects = [EvalRedirectV {
            fd,
            mode: RedirectMode::Write,
            target: EvalRedirect::File(target),
        }];

        let result = apply_redirects(&redirects, &mut shell);

        assert!(result.is_err(), "redirect to unopened fd {fd} must error");
        #[allow(
            clippy::cast_possible_wrap,
            reason = "fd is a non-negative descriptor number (u32 by design); the libc boundary wants c_int and the value never sets the top bit, so no wrap"
        )]
        let fd_i32 = fd as i32;
        assert!(
            !fd_is_open(fd_i32),
            "fd {fd} must stay unopened — no unrestorable redirect installed",
        );
    }

    /// The `n>&m` shape is rejected for the same reason.
    #[test]
    fn fd_dup_redirect_to_unopened_fd_errors_and_leaks_nothing() {
        let _serial = FD_TABLE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (fd, _barrier) = pinned_unopened_fd();
        let mut shell = Shell::default();
        let redirects = [EvalRedirectV {
            fd,
            mode: RedirectMode::Write,
            target: EvalRedirect::Fd(1),
        }];

        let result = apply_redirects(&redirects, &mut shell);

        assert!(result.is_err(), "redirect to unopened fd {fd} must error");
        #[allow(
            clippy::cast_possible_wrap,
            reason = "fd is a non-negative descriptor number (u32 by design); the libc boundary wants c_int and the value never sets the top bit, so no wrap"
        )]
        let fd_i32 = fd as i32;
        assert!(
            !fd_is_open(fd_i32),
            "fd {fd} must stay unopened — no unrestorable redirect installed",
        );
    }
}
