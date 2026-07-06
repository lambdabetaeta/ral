//! Atomic write recipe (`> file` to a regular file), fd-level redirects
//! for bundled tools and unsupported fd shapes (`apply_redirects` /
//! `restore_redirects`), and the `<file → fd 0` handoff that parks the
//! file in `shell.turn.io.stdin` so every dispatch arm reads input
//! through the same `Source` channel.

use crate::syntax::ast::RedirectMode;
use crate::types::*;

use super::io_event;
use super::process::io_error;
use super::stdio::{EvalRedirect, EvalRedirectV};
#[cfg(any(unix, windows))]
use super::stdio::stderr_mode;

#[cfg(windows)]
use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
#[cfg(windows)]
use windows_sys::Win32::System::Console::{
    GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
};

/// Pending tmp-to-target rename for an atomic `>` redirect.
///
/// Drop removes the tmp; `commit` fsyncs and renames it into place.
///
/// # The atomic-write recipe, and what each step buys you
///
/// 1. **Resolve symlinks via `canonicalize`.**  Without this, writing
///    to a symlink would replace the link itself with a regular file
///    — silently breaking anyone else who held the link as a path.
///    For new files (no canonical form yet) we fall through to the
///    literal path.  Done in [`open_atomic`].
///
/// 2. **Place the tmp file in the same directory as the target.**
///    `rename(2)` is only atomic within a single filesystem; cross-fs
///    rename returns `EXDEV` and the recipe fails outright.  Using
///    `/tmp` for the staging file would break on every machine where
///    `/tmp` is a separate mount (which is most of them, post-tmpfs).
///    Done in [`open_atomic`].
///
/// 3. **Use a cryptographically random tmp name.**  A predictable
///    name (e.g. `target.tmp`) races with concurrent writers and
///    invites symlink-attack-shaped surprises in shared dirs.
///    `tempfile::Builder` picks the name with `O_EXCL` semantics and
///    RAII-deletes the file if we error out before persisting, so we
///    never leak `.tmp` detritus.  Done in [`open_atomic`].
///
/// 4. **Write the contents, then flush them to disk before rename.**
///    Skip this and a power loss in the window between rename and
///    background data flush leaves a renamed-but-zero-length file:
///    the directory entry was committed, the data blocks weren't.
///    This is the ext4 `data=ordered` bug from 2009 that ate config
///    files all over the Linux desktop (see "Further reading").
///    Step 4a (write) is the redirect's writer; step 4b (flush) is
///    the `sync_all` in [`AtomicCommit::commit`].
///
/// 5. **Copy the target's existing mode onto the tmp before rename
///    (or default to 0644 for new files).**  Skip this and a
///    previously-0644 file silently becomes 0600 (tempfile's default),
///    narrowing access — or, on platforms where the default differs,
///    becomes world-readable.  Either way the resulting permissions
///    don't match what the user had before.  Done in [`open_atomic`].
///
/// 6. **Atomic `rename(tmp, target)`.**  Skip this in favour of
///    "truncate target, write" and you reintroduce exactly the bug
///    we're fixing: a crash or `^C` mid-write leaves a half-written
///    file with no recovery path.  Skip it for "copy then unlink"
///    and there's a window where the target doesn't exist; readers
///    racing the swap see `ENOENT`.  Done in [`AtomicCommit::commit`].
///
/// 7. **Best-effort flush the parent directory to disk.**  Skip this
///    and a kernel panic right after the rename can roll back the
///    directory entry: the data is on disk under the tmp name, but
///    on next boot the rename appears never to have happened.
///    Errors here don't roll back the rename, so we ignore them;
///    on platforms that don't support directory-level flush (Windows)
///    the open itself fails and we fall through silently.  Done in
///    [`AtomicCommit::commit`].
///
/// # Things this deliberately does *not* handle
///
/// - **Hardlinks are not preserved.**  Atomic rename creates a fresh
///   inode; any other names that pointed at the old inode keep its
///   old contents.  Every editor accepts this tradeoff — preserving
///   the inode would mean truncate-and-write, which is non-atomic.
///   Pick one.
/// - **Owner/group, xattrs, ACLs, SELinux contexts** are not copied
///   over.  Kernel default-inheritance handles the common case;
///   explicit copying belongs in a separate "preserve" path if it's
///   ever needed.
/// - **Concurrent writers** race normally — atomic rename gives
///   crash safety, not mutual exclusion.  Last writer wins.
///
/// # New failure mode vs. plain `fs::write`
///
/// Atomic rename requires write permission on the *parent
/// directory*, not just the target file.  Callers that previously
/// relied on file-only write perms will now see `EACCES` here.
/// This is the standard tradeoff for crash safety.
///
/// # Platform notes
///
/// On Linux/macOS the recipe is exact: `rename(2)` swaps the inode
/// even if other processes have the target open, and the open
/// handles keep reading the old inode until close.
///
/// On Windows, `tempfile::persist` calls
/// `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`, which is atomic on NTFS
/// in the no-observable-intermediate-state sense — but it *fails*
/// (rather than silently swapping) if another process has the target
/// open with a sharing mode that excludes deletion.  Live readers
/// (editors, tail-followers, AV scanners) therefore turn what would
/// be a silent success on Linux into a hard error on Windows.
/// Mode preservation (step 5) and directory-level flush (step 7) are
/// no-ops on Windows: the former because the POSIX permission
/// bitfield doesn't apply to NTFS DACLs, the latter because NTFS
/// commits directory-entry changes via its own journal and there's
/// no API to force it from a regular file handle.
///
/// # Further reading
///
/// - [`rename(2)`](https://man7.org/linux/man-pages/man2/rename.2.html)
///   — atomicity guarantees and the same-filesystem constraint.
/// - [`fsync(2)`](https://man7.org/linux/man-pages/man2/fsync.2.html)
///   — what gets flushed; why the directory-level flush matters.
/// - [Theodore Ts'o, "Don't fear the fsync!"](https://lwn.net/Articles/322823/)
///   — the ext4 `data=ordered` zero-length-after-rename saga that
///   forced flush-before-rename into common practice.
/// - [Pillai et al., "All File Systems Are Not Created Equal"](https://www.usenix.org/conference/osdi14/technical-sessions/presentation/pillai)
///   — academic survey showing how often application code gets
///   crash-safe updates wrong.  This recipe passes their checks.
/// - [Dan Luu, "Files are hard"](https://danluu.com/file-consistency/)
///   — accessible overview of the surprising failure modes.
pub(crate) struct AtomicCommit {
    tmp: tempfile::NamedTempFile,
    target: std::path::PathBuf,
}

/// The largest prefix of a written file read to seed the host's write-card
/// preview — generous enough to hold the first several lines of any realistic
/// file, so a large write is never pulled into memory wholesale to show a head.
const PREVIEW_CAP: u64 = 64 * 1024;

impl AtomicCommit {
    /// Read a bounded prefix — at most [`PREVIEW_CAP`] bytes — of the temp
    /// file: a best-effort snapshot of the *head* of what will land at `target`
    /// if `commit` succeeds.  The write surface previews only the first few
    /// lines, so the whole file (which may be large) is never read.  Returns
    /// `None` on any OS error.
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

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:surface:atomic-commit] The atomic `>` write door's commit step: re-open the target's parent directory only to fsync the rename durable. The write surface fires when the redirect frame settles (committed once this returns Ok); this open carries written bytes to disk, it is not a separate model read."
    )]
    pub fn commit(self) -> std::io::Result<()> {
        // (4b) Durably commit data blocks before any directory entry change.
        self.tmp.as_file().sync_all()?;
        // (6) Atomic rename.  `tempfile::persist` calls `rename(2)`; on
        // success the tmp's RAII cleanup is disarmed automatically.
        self.tmp
            .persist(&self.target)
            .map(|_| ())
            .map_err(|e| e.error)?;
        // (7) Best-effort directory-level flush.  Errors don't unwind
        // the rename, so we eat them; platforms without directory-level
        // flush (Windows) just fail to open the dir as a regular file.
        if let Ok(dir) = std::fs::File::open(crate::path::parent_or_cwd(&self.target)) {
            let _ = dir.sync_all();
        }
        Ok(())
    }
}

/// True for non-existent paths or regular files.  Non-regular files (TTYs,
/// /dev/null, named pipes) get streaming semantics — atomicity is meaningless.
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
    // (1) Symlink resolution.  `canonicalise_strict` errors on
    // non-existent paths — for a fresh file the literal target is
    // the right thing.
    let target = target
        .canonicalise_strict()
        .unwrap_or_else(|_| target.as_path().to_path_buf());
    // (2) Same-directory tmp.  `Path::parent` returns `Some("")` for
    // a bare filename; [`crate::path::parent_or_cwd`] treats empty as
    // "current directory" so the tempfile call doesn't choke.
    let parent = crate::path::parent_or_cwd(&target);
    // (3) Random tmp name with RAII-cleanup on early return.  Dot
    // prefix hides it from `ls`; `O_EXCL` semantics inside.
    let tmp = tempfile::Builder::new()
        .prefix(".")
        .suffix(".ral-write.tmp")
        .tempfile_in(parent)
        .map_err(|e| io_error(path, e))?;
    // tempfile defaults to 0600 for security; redirects expect umask-style
    // permissions.  Preserve the existing target's mode if it exists; for a
    // new file, mirror what a plain `open(2)` create would produce —
    // `0o666 & !umask` — rather than a hardcoded mode that ignores the
    // process umask.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&target)
            .ok()
            .map(|m| m.permissions().mode() & 0o7777)
            .unwrap_or_else(|| {
                // `libc::umask` is a combined getter/setter with no
                // read-only variant: set a throwaway value to read the
                // current mask, then restore it immediately.
                let mask = unsafe {
                    let prev = libc::umask(0o022);
                    libc::umask(prev);
                    prev
                } as u32;
                0o666 & !mask
            });
        let mut perms = tmp
            .as_file()
            .metadata()
            .map_err(|e| io_error(path, e))?
            .permissions();
        perms.set_mode(mode);
        tmp.as_file()
            .set_permissions(perms)
            .map_err(|e| io_error(path, e))?;
    }
    let file = tmp.as_file().try_clone().map_err(|e| io_error(path, e))?;
    Ok((file, AtomicCommit { tmp, target }))
}

/// Open a redirect target.  Returns the file plus an optional atomic-commit
/// token for `>` redirects to regular files.  Callers must call `.commit()` on
/// the token after the writer completes; dropping it removes the tmp file.
///
/// `>` is atomic for regular files (tmp + fsync + rename), streaming for
/// non-regular targets.  `>~` is always streaming.  `>>` is append.
///
/// Relative paths are resolved against the shell's scoped cwd so that
/// `within [dir: ...]` redirects target the right directory even from
/// builtins, where the host process cwd is not changed.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:open-file] The redirect read/write door for `<`/`>`/`>>`/`>|` on fd 1/2: every model redirect opens here. The read/write card is fused onto the operation by the redirect frame that wraps this open (committed/aborted/failed outcome on settle)."
)]
pub(crate) fn open_file(
    path: &str,
    mode: &RedirectMode,
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
            .map_err(|e| io_error(path, e)),
        RedirectMode::StreamWrite => std::fs::File::create(rp.as_path())
            .map(|f| (f, None))
            .map_err(|e| io_error(path, e)),
        RedirectMode::Read => std::fs::File::open(rp.as_path())
            .map(|f| (f, None))
            .map_err(|e| io_error(path, e)),
        RedirectMode::Write => {
            if atomic_eligible(rp.as_path()) {
                let (file, commit) = open_atomic(path, &rp)?;
                Ok((file, Some(commit)))
            } else {
                std::fs::File::create(rp.as_path())
                    .map(|f| (f, None))
                    .map_err(|e| io_error(path, e))
            }
        }
    }
}

/// One backed-up file descriptor: `dup(fd) → backup` (`F_DUPFD_CLOEXEC`), taken before
/// [`apply_redirects`]'s `dup2` overwrote `fd`.  `Drop` undoes the redirect
/// by `dup2`-ing the backup back over `fd` and closing the backup.
///
/// Restoration is therefore RAII: even when [`apply_redirects`] short-
/// circuits half-built via `?`, every `dup2` already performed is reversed
/// when the partial [`RedirectGuard`] falls out of scope.  The bug class
/// "fd 1 silently keeps pointing at a redirect file forever after a mid-
/// loop failure, *and* the backup fd leaks" is unwritable.
#[cfg(unix)]
pub(crate) struct BackupFd {
    pub(crate) fd: u32,
    pub(crate) backup: i32,
}

#[cfg(unix)]
impl Drop for BackupFd {
    fn drop(&mut self) {
        // Best-effort: dup2 / close failures inside Drop have nowhere to go.
        unsafe {
            libc::dup2(self.backup, self.fd as i32);
            libc::close(self.backup);
        }
    }
}

/// Windows analog of [`BackupFd`].  `apply_redirects` calls
/// `GetStdHandle(std_handle) → prev`, then `SetStdHandle(std_handle, new)`;
/// `Drop` restores the slot.  `owned` carries the redirect target's handle
/// when we opened it ourselves (file redirects) — `Drop` closes it after
/// the slot is restored, mirroring the Unix `dup` + `close(raw)` pair that
/// drives the kernel ref count back to zero.  `owned: None` is used for
/// `Fd→Fd` merges (e.g. `2>&1`), which alias an existing std slot's handle
/// without taking ownership.
#[cfg(windows)]
pub(crate) struct BackupHandle {
    pub(crate) std_handle: u32,
    pub(crate) prev: HANDLE,
    pub(crate) owned: Option<HANDLE>,
}

#[cfg(windows)]
impl Drop for BackupHandle {
    fn drop(&mut self) {
        // Best-effort: SetStdHandle / CloseHandle failures inside Drop have
        // nowhere to go.  Order matters: restore the slot first so the
        // owned handle is no longer reachable through any std slot before
        // we close it.
        unsafe {
            SetStdHandle(self.std_handle, self.prev);
            if let Some(h) = self.owned {
                CloseHandle(h);
            }
        }
    }
}

/// State to undo `apply_redirects`: backed-up fds (LIFO-restored on `Drop`)
/// plus pending atomic commits.
pub(crate) struct RedirectGuard {
    #[cfg(unix)]
    saved: Vec<BackupFd>,
    #[cfg(windows)]
    saved: Vec<BackupHandle>,
    commits: Vec<AtomicCommit>,
}

/// LIFO-restore any backups still in the guard.  Both `restore_redirects`
/// (success path) and `?`-driven early returns from `apply_redirects`
/// (error path) reach the actual fd work via this `Drop`.  `Vec::drop`
/// would unwind in forward order; we want the reverse so that overlapping
/// `dup2`s (e.g. swapping fd 1 and fd 2) compose correctly.
#[cfg(unix)]
impl Drop for RedirectGuard {
    fn drop(&mut self) {
        while let Some(_b) = self.saved.pop() {
            // `_b` drops here — `BackupFd::Drop` performs the dup2 + close.
        }
    }
}

/// Windows analog of the Unix `Drop`: LIFO-restore via `BackupHandle::Drop`
/// so overlapping `SetStdHandle` swaps unwind in the reverse order they
/// were applied.
#[cfg(windows)]
impl Drop for RedirectGuard {
    fn drop(&mut self) {
        while let Some(_b) = self.saved.pop() {
            // `_b` drops here — `BackupHandle::Drop` restores the slot and
            // closes any owned handle.
        }
    }
}

/// Read+File redirects to fd 0 are owned by `shell.turn.io.stdin` (set up by
/// [`install_stdin_redirect`]); `apply_redirects` and `wire_stdin` must agree
/// to leave them alone.  This predicate is the single point where that rule
/// is named.
#[cfg(any(unix, windows))]
fn is_stdin_file_redirect(r: &EvalRedirectV) -> bool {
    matches!(
        r,
        EvalRedirectV {
            fd: 0,
            mode: RedirectMode::Read,
            target: EvalRedirect::File(_)
        }
    )
}

/// Map a redirect fd to the matching Win32 `STD_*_HANDLE` slot id.
/// Bundled uutils tools only touch 0/1/2; higher fds aren't reachable
/// via `SetStdHandle`, so reject them outright rather than silently
/// dropping the redirect.
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

/// Apply fd redirects (for builtins that print via stdout/stderr directly).
/// Returns a guard whose `commits` must be fired by `restore_redirects` after
/// the builtin runs successfully.
///
/// Read+File redirects to fd 0 are skipped: they are owned by
/// [`install_stdin_redirect`], not by the dup2 path.
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
                let effective_mode = if *fd == 2 { stderr_mode(mode) } else { *mode };
                // `?`-propagation is safe: any backups already pushed onto
                // `guard.saved` will LIFO-restore via `RedirectGuard::Drop`
                // when `guard` falls out of scope on the error return.
                let (file, commit) = open_file(path, &effective_mode, shell)?;
                if let Some(c) = commit {
                    guard.commits.push(c);
                }
                use std::os::unix::io::IntoRawFd;
                let raw = file.into_raw_fd();
                if let Err(e) = install_dup2(raw, *fd, &mut guard) {
                    unsafe { libc::close(raw) };
                    return Err(e);
                }
                unsafe { libc::close(raw) };
            }
            EvalRedirect::Fd(target_fd) => {
                install_dup2(*target_fd as i32, *fd, &mut guard)?;
            }
        }
    }
    Ok(guard)
}

/// Redirect `dst_fd` onto `src_fd`, backing the prior `dst_fd` up first so the
/// guard's `Drop` can restore it.
///
/// The backup `dup(dst_fd)` (`F_DUPFD_CLOEXEC`) is the redirect's only restore path.  When it
/// fails — `EBADF` for a `dst_fd` that was never opened, the normal state for
/// an fd ≥ 3 — there is nothing to restore *to*, so installing the `dup2`
/// would leak `dst_fd` pointing at the target for the life of the shell.  Skip
/// the `dup2` and error cleanly instead.  The `dup2` return is checked: a
/// failure leaves `dst_fd` unchanged, and the already-pushed [`BackupFd`]
/// restores it on unwind.
#[cfg(unix)]
fn install_dup2(src_fd: i32, dst_fd: u32, guard: &mut RedirectGuard) -> Settled<()> {
    // `F_DUPFD_CLOEXEC` rather than a bare `dup`: this backup is internal
    // bookkeeping for the redirect guard, not a descriptor meant for any
    // child — a child spawned while the guard is live (a nested external
    // launched from inside the redirected builtin body) must not inherit
    // it.
    let backup = unsafe { libc::fcntl(dst_fd as i32, libc::F_DUPFD_CLOEXEC, 0) };
    if backup < 0 {
        return Err(Break::Error(Error::new(
            format!(
                "redirect to fd {dst_fd}: cannot save the existing descriptor \
                 ({}) — refusing to install a redirect with no restore path",
                std::io::Error::last_os_error()
            ),
            1,
        )));
    }
    guard.saved.push(BackupFd { fd: dst_fd, backup });
    if unsafe { libc::dup2(src_fd, dst_fd as i32) } < 0 {
        return Err(Break::Error(Error::new(
            format!(
                "redirect to fd {dst_fd}: dup2 failed ({})",
                std::io::Error::last_os_error()
            ),
            1,
        )));
    }
    Ok(())
}

/// Windows analog of the Unix `apply_redirects`.  `SetStdHandle` replaces
/// `dup2`: Rust's `std::io::stdout()` calls `GetStdHandle(STD_OUTPUT_HANDLE)`
/// on every write, so `SetStdHandle` retroactively redirects subsequent
/// stdio writes — the kernel-level analog of `dup2` for the std slots.
///
/// The body mirrors the Unix arm structurally: skip stdin-file redirects
/// (owned by `install_stdin_redirect`), open file targets through the
/// shared [`open_file`], push a [`BackupHandle`] for every slot we touch
/// so the guard's `Drop` LIFO-restores under `?`-propagation, and for
/// `Fd→Fd` merges (`2>&1`) alias the existing slot's handle without
/// taking ownership.
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
                let effective_mode = if *fd == 2 { stderr_mode(mode) } else { *mode };
                // `?`-propagation is safe: any backups already pushed onto
                // `guard.saved` will LIFO-restore via `RedirectGuard::Drop`
                // when `guard` falls out of scope on the error return.
                let (file, commit) = open_file(path, &effective_mode, shell)?;
                if let Some(c) = commit {
                    guard.commits.push(c);
                }
                use std::os::windows::io::IntoRawHandle;
                let raw = file.into_raw_handle() as HANDLE;
                let prev = unsafe { GetStdHandle(dst_slot) };
                // Push before SetStdHandle: on success Drop restores the
                // slot and closes `raw`; on failure Drop's SetStdHandle is
                // a no-op (slot unchanged) and Drop still closes `raw`, so
                // the opened file isn't leaked.
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

/// Fallback stub for non-Unix non-Windows targets.  The in-process
/// redirect implementation is `cfg(any(unix, windows))`-gated, so this
/// branch is unreachable in practice; the stub exists so
/// `restore_redirects` and the redirect scaffolding compile on
/// unrelated targets.
#[cfg(not(any(unix, windows)))]
pub(crate) fn apply_redirects(
    _redirects: &[EvalRedirectV],
    _shell: &mut Shell,
) -> Settled<RedirectGuard> {
    Ok(RedirectGuard { commits: vec![] })
}

/// Run after a redirected builtin returns, regardless of success — otherwise
/// the shell's stdout/stderr stays broken.  The fd-level restore happens
/// inside [`RedirectGuard::Drop`] when `guard` falls out of scope at the end
/// of this function; we just need the commits out before that.
pub(crate) fn restore_redirects(mut guard: RedirectGuard) -> Vec<AtomicCommit> {
    std::mem::take(&mut guard.commits)
}

/// Fire pending atomic commits.  Call only on the success path; dropping
/// the Vec on the error path discards each commit's tmp file.
pub(crate) fn commit_atomics(commits: Vec<AtomicCommit>) -> Settled<()> {
    for commit in commits {
        commit
            .commit()
            .map_err(|e| Break::Error(Error::new(format!("atomic write: {e}"), 1)))?;
    }
    Ok(())
}

/// Open `<file` redirects to fd 0 and park the file in `shell.turn.io.stdin`.
///
/// Returns a [`StdinRedirectGuard`] whose `restore` puts back whatever Source
/// was previously installed (a pipeline pipe, an outer redirect, or
/// Terminal).  When several `<file` redirects target fd 0, the last one wins
/// — same as POSIX shells.  No-op when no such redirect is present, in which
/// case `shell.turn.io.stdin` is left untouched.
///
/// Routing `<file` through `Source` rather than `dup2` is what keeps the
/// cached `startup_stdin_tty` from lying to downstream consumers (codecs,
/// `lines`): they consult the cache only when `Source` is `Terminal`, and
/// `Terminal` truly does mean "fall through to the inherited fd 0".
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:surface:stdin-redirect] The `< file` read door on fd 0: opens the model's stdin redirect and emits the read card eagerly (so the read precedes the body/exec it feeds, e.g. `cat < a`). The open IS the surfaced read."
)]
pub(crate) fn install_stdin_redirect(
    redirects: &[EvalRedirectV],
    shell: &mut Shell,
) -> Settled<StdinRedirectGuard> {
    let Some(path) = redirects.iter().rev().find_map(|r| match r {
        EvalRedirectV {
            fd: 0,
            mode: RedirectMode::Read,
            target: EvalRedirect::File(p),
        } => Some(p),
        _ => None,
    }) else {
        return Ok(StdinRedirectGuard::Untouched);
    };
    let rp = shell.resolve(path);
    shell.check_fs_read(&rp)?;
    let f = std::fs::File::open(rp.as_path()).map_err(|e| io_error(path, e))?;
    let prior = std::mem::replace(&mut shell.turn.io.stdin, crate::io::Source::File(f));
    // Door 1 — READ: announce the open eagerly so the read event precedes the
    // body/exec it feeds (e.g. the read card before exec in `cat < a`).  Read
    // has no outcome: a failed open returns above before this point.
    shell.emit_io(io_event::read(path));
    Ok(StdinRedirectGuard::Installed(prior))
}

/// Restore-on-exit token for [`install_stdin_redirect`].
pub(crate) enum StdinRedirectGuard {
    Untouched,
    Installed(crate::io::Source),
}

impl StdinRedirectGuard {
    pub(crate) fn restore(self, shell: &mut Shell) {
        if let StdinRedirectGuard::Installed(prior) = self {
            shell.turn.io.stdin = prior;
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// The fd table is process-global; these tests mutate it and assert on
    /// it, so they must not race each other or concurrent fd allocation.
    static FD_TABLE: Mutex<()> = Mutex::new(());

    /// `F_GETFD` returns `EBADF` exactly when `fd` is not open.
    fn fd_is_open(fd: i32) -> bool {
        unsafe { libc::fcntl(fd, libc::F_GETFD) >= 0 }
    }

    /// An fd ≥ 3 (the shape `3>…` reaches) that is not open and stays so even
    /// once `apply_redirects` opens its own descriptors.  We pin a barrier fd
    /// well above the low range the redirect machinery allocates into, then
    /// target the slot just past it: `open_file`'s descriptors land in the
    /// free low slots, never on the target.  The barrier's guard closes it.
    fn pinned_unopened_fd() -> (u32, BarrierFd) {
        let barrier = unsafe { libc::fcntl(libc::STDOUT_FILENO, libc::F_DUPFD_CLOEXEC, 32) };
        assert!(barrier >= 0, "could not pin a high barrier fd");
        let target = barrier as u32 + 1;
        assert!(
            !fd_is_open(target as i32),
            "target fd {target} must start free"
        );
        (target, BarrierFd(barrier))
    }

    /// Closes the pinned barrier fd on drop, so the test leaks nothing.
    struct BarrierFd(i32);
    impl Drop for BarrierFd {
        fn drop(&mut self) {
            unsafe { libc::close(self.0) };
        }
    }

    /// `echo hi 3>/tmp/x`-shape: a file redirect to an unopened fd has no
    /// descriptor to back up, so it must error rather than install a
    /// `dup2` with no restore path.  The fd stays unopened afterward.
    #[test]
    fn file_redirect_to_unopened_fd_errors_and_leaks_nothing() {
        let _serial = FD_TABLE.lock().unwrap_or_else(|e| e.into_inner());
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
        assert!(
            !fd_is_open(fd as i32),
            "fd {fd} must stay unopened — no unrestorable redirect installed",
        );
    }

    /// The `n>&m`-shape (`EvalRedirect::Fd`) to an unopened fd is rejected
    /// for the same reason — no backup, no restore path.
    #[test]
    fn fd_dup_redirect_to_unopened_fd_errors_and_leaks_nothing() {
        let _serial = FD_TABLE.lock().unwrap_or_else(|e| e.into_inner());
        let (fd, _barrier) = pinned_unopened_fd();
        let mut shell = Shell::default();
        let redirects = [EvalRedirectV {
            fd,
            mode: RedirectMode::Write,
            target: EvalRedirect::Fd(1),
        }];

        let result = apply_redirects(&redirects, &mut shell);

        assert!(result.is_err(), "redirect to unopened fd {fd} must error");
        assert!(
            !fd_is_open(fd as i32),
            "fd {fd} must stay unopened — no unrestorable redirect installed",
        );
    }
}
