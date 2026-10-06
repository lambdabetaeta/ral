//! Staging an atomic `>`: a temp file beside the target, renamed over it on
//! commit.  Pure of the shell: judging the write and reporting it belong to
//! `runtime::redirect`.
//!
//! Staging, commit and rollback all act relative to the directory handle the
//! door located, so nothing between the judgment and the rename can rename
//! the directory out from under the write.  The rename mints a fresh inode:
//! hardlinks to the old one keep the old contents, and owner, xattrs and ACLs
//! fall to kernel inheritance.  Concurrent writers race as usual: this buys
//! crash safety, not exclusion.

use super::Located;
use super::walk::{Kind, Stat};
use std::ffi::OsString;
use std::fs::File;
use std::io::{self, Read as _};

/// Cap on the bytes a preview reads, so a large file is never pulled into
/// memory whole to show a change.
const PREVIEW_CAP: u64 = 64 * 1024;

/// An atomic `>` staged in a temp file beside its target, until
/// [`commit`](Self::commit) renames it, and `Drop` unlinks it
/// if nobody committed: a staged temp never outlives the frame that staged it.
///
/// The `Option` is the commit-by-value latch: `commit` takes `self` and
/// empties it first, so the `Drop` that follows sees `None` and does
/// nothing on the path that already decided.
pub(crate) struct PendingWrite(Option<Staged>);

struct Staged {
    target: Located,
    tmp: OsString,
}

impl PendingWrite {
    /// Finish the write: flush the staged bytes, rename onto the target, then
    /// fsync the directory entry.  Any failure unlinks the temp, so the target
    /// is either replaced whole or left exactly as it was.
    ///
    /// # Errors
    /// Returns the first I/O error of the flush or the rename.
    pub(crate) fn commit(mut self) -> io::Result<()> {
        let staged = self
            .0
            .take()
            .expect("commit consumes a freshly staged write");
        let landed = staged.rename_durable();
        if landed.is_err() {
            staged.unlink();
        }
        landed
    }

    /// The target the write is staged beside.
    pub(crate) fn target(&self) -> Option<&Located> {
        self.0.as_ref().map(|staged| &staged.target)
    }

    /// The staged file whole: what lands at the target if `commit` succeeds.
    pub(crate) fn staged(&self) -> Option<Vec<u8>> {
        let staged = self.0.as_ref()?;
        read_capped(staged.target.sibling_read(&staged.tmp).ok()?)
    }
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

    fn rename_durable(&self) -> io::Result<()> {
        // Data blocks before any directory entry, or a crash can commit the
        // entry with the blocks still unwritten: a renamed zero-length file.
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

impl Located {
    /// Stage an atomic `>` beside this target.  `existing` is its stat when it
    /// exists: its mode is carried onto the staged file, since a redirect must
    /// not silently narrow a 0644 file; a new file gets what `open(2)` would
    /// have created under the umask.
    ///
    /// # Errors
    /// The staging create's, or the mode's.
    pub(crate) fn stage(self, existing: Option<&Stat>) -> io::Result<(File, PendingWrite)> {
        let (file, tmp) = self.create_sibling_tmp()?;
        // Latched before the first fallible step, so an early `?` unlinks it.
        let pending = PendingWrite(Some(Staged { target: self, tmp }));
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
            file.set_permissions(std::fs::Permissions::from_mode(mode))?;
        }
        #[cfg(not(unix))]
        let _ = existing;
        Ok((file, pending))
    }

    /// The object's whole content, empty when it does not exist.
    ///
    /// `None` where it cannot be read whole: past [`PREVIEW_CAP`] or not a
    /// regular file.  Never a prefix: a card reads a write as the change it
    /// made, and half a side is not a change.
    pub(crate) fn preview(&self) -> Option<Vec<u8>> {
        match self.stat().ok()? {
            None => Some(Vec::new()),
            Some(s) if s.kind == Kind::File && s.len <= PREVIEW_CAP => {
                read_capped(self.read().ok()?)
            }
            Some(_) => None,
        }
    }
}
