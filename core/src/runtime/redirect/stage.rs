//! Staging an atomic `>`, which lives with the redirect that uses it: a temp
//! file beside the target, renamed over it on commit.  Pure of the shell:
//! judging the write and reporting it belong to `runtime::redirect`.
//!
//! The staged write holds its target and its sibling together, so nothing
//! between the judgment and the rename can rename the directory out from
//! under it.  The rename mints a fresh inode: hardlinks to the old one keep
//! the old contents, and owner, xattrs and ACLs fall to kernel inheritance.
//! Concurrent writers race as usual: this buys crash safety, not exclusion.

use crate::path::walk::Kind;
use crate::path::{Judged, Located, StagedWrite, Writing};
use std::fs::File;
use std::io::{self, Read as _, Seek as _};

/// Cap on the bytes a preview reads, so a large file is never pulled into
/// memory whole to show a change.
const PREVIEW_CAP: u64 = 64 * 1024;

/// An atomic `>` staged beside its target until [`commit`](Self::commit)
/// renames it; dropped, the [`StagedWrite`] unlinks the sibling, so a staged
/// temp never outlives the frame that staged it.
pub(crate) struct PendingWrite(StagedWrite);

impl PendingWrite {
    /// Stage a write beside `target`: the pending write, and a handle for the
    /// writer, which shares the staging handle's offset.
    ///
    /// # Errors
    /// The staging create's, or the mode's.
    pub(crate) fn stage(target: Located<Judged<Writing>>) -> io::Result<(File, Self)> {
        let mut staged = target.stage()?;
        let writer = staged.file().try_clone()?;
        Ok((writer, Self(staged)))
    }

    /// Finish the write: flush the staged bytes, then rename onto the target.
    /// A failure leaves no sibling, so the target is either replaced whole or
    /// left exactly as it was.
    ///
    /// # Errors
    /// Returns the first I/O error of the flush or the rename.
    pub(crate) fn commit(mut self) -> io::Result<()> {
        // Data blocks before any directory entry, or a crash can commit the
        // entry with the blocks still unwritten: a renamed zero-length file.
        self.0.file().sync_all()?;
        self.0.publish()
    }

    /// Abandon the write, leaving the target as it was.
    pub(crate) fn discard(self) {
        let _ = self.0.discard();
    }

    /// The target the write is staged beside.
    pub(crate) fn target(&self) -> &Located<Judged<Writing>> {
        self.0.target()
    }

    /// The target as it stands: empty where it does not exist, `None` where
    /// it cannot be read whole.
    pub(crate) fn before(&self) -> Option<Vec<u8>> {
        let target = self.target();
        match target.stat() {
            None => Some(Vec::new()),
            Some(s) if s.kind == Kind::File => read_capped(&mut target.read().ok()?),
            Some(_) => None,
        }
    }

    /// The staged file whole: what ral wrote, read through the staging
    /// handle.  On Unix the rename takes the sibling's *name*, so what lands
    /// at the target is not this write's to assert.
    pub(crate) fn staged(&mut self) -> Option<Vec<u8>> {
        read_capped(self.0.file())
    }
}

/// `file` whole, from its start.  `None` past [`PREVIEW_CAP`], judged on the
/// open handle so the size read is the size of the bytes read.
pub(super) fn read_capped(file: &mut File) -> Option<Vec<u8>> {
    if file.metadata().ok()?.len() > PREVIEW_CAP {
        return None;
    }
    file.rewind().ok()?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).ok()?;
    Some(bytes)
}
