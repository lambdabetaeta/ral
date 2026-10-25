//! Creates, replacements and removals through judged handles.

use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::sync::Arc;

use super::super::error::directory_error;
use super::{
    Data, Dir, EntryName, Judged, Kind, Leaf, LeafMode, Located, Node, Sharing, StagedWrite,
    Streamed, Trace, Writing, host, step_into,
};

/// What a data open is for: [`Located::open_data`]'s argument, and what each
/// of the walk's other data opens asks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Append,
    Write,
}

/// Who may reach a leaf made absent.
///
/// `Public` is under the process umask, as a shell's `>` or `mkdir` make it.
/// `Private` is owner-only (0600/0700), as `mktemp(1)` makes a name the program
/// invented; Windows inherits the directory's ACL for both.  `Secret` is
/// owner-only on every host, whatever the directory would hand down: 0600, and
/// on Windows a protected DACL naming the owner alone.  A directory made on
/// Windows inherits for all three, and on Unix is 0700 for the last two.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Visibility {
    Public,
    Private,
    Secret,
}

/// How a streaming write opens its target.
#[derive(Clone, Copy)]
pub(crate) enum Streaming {
    Append,
    Truncate,
}

impl Dir<Judged<Writing>> {
    /// The absent entry `name` of this directory, as a create target: a
    /// [`Located`] whose leaf is absent, whose trace is this directory's plus
    /// a one-name tail, and whose directory is this one, by the walk's own
    /// node: no syscall.  It is unjudged: a name under a write-judged
    /// directory is still a distinct position, and an exclusion can name an
    /// absent one, so the caller judges it before anything makes it.
    pub(crate) fn absent(&self, name: EntryName) -> Located {
        Located {
            trace: self.trace.extended([], vec![name]),
            leaf: Leaf::Absent {
                dir: Arc::clone(&self.node),
            },
            linked: None,
            _a: PhantomData,
        }
    }

    /// Remove everything beneath this directory, leaving it empty: each entry
    /// located without following and removed through this handle, so a link
    /// goes as the link and its target is never touched.  An entry gone
    /// before its turn is gone.
    ///
    /// # Errors
    /// The listing's, a locate's other than `NotFound`, an entry's removal.
    fn empty(&self) -> io::Result<()> {
        let names: Vec<EntryName> = self.read_dir()?.map(|entry| entry.name().clone()).collect();
        for name in names {
            let removed = self
                .locate(&name, LeafMode::NoFollow)
                .and_then(|entry| entry.marked::<Judged<Writing>>().remove());
            match removed {
                Err(e) if e.kind() != io::ErrorKind::NotFound => return Err(e),
                _ => {}
            }
        }
        Ok(())
    }
}

impl Streamed {
    pub(crate) fn file(&mut self) -> &mut File {
        &mut self.file
    }

    /// Seek to 0 before each image.
    pub(crate) fn reader(&mut self) -> Option<&mut File> {
        self.reader.as_mut()
    }

    pub(crate) fn trace(&self) -> &Trace {
        &self.trace
    }

    pub(crate) fn created(&self) -> bool {
        self.created
    }

    /// `set_len(0)` where `stat` is a regular file; a no-op otherwise, so a
    /// FIFO or a device streams.
    ///
    /// # Errors
    /// The truncate's.
    pub(crate) fn truncate(&self) -> io::Result<()> {
        if self.stat.kind == Kind::File {
            self.file.set_len(0)?;
        }
        Ok(())
    }
}

impl Located<Judged<Writing>> {
    /// A streaming write — `>>`, or a `>` that cannot stage — opened through
    /// the held leaf and consumed.  `Truncate` does not truncate:
    /// [`Streamed::truncate`] does, after the caller has read its
    /// before-image.  The read handle is `None` for a FIFO, a device, or a
    /// file the caller may not read.  Takes `self`: after this the object is
    /// the handles.
    ///
    /// # Errors
    /// The open's: `AlreadyExists` where the leaf was absent at the walk and
    /// has appeared since.
    pub(crate) fn open_streaming(self, access: Streaming) -> io::Result<Streamed> {
        let access = match access {
            Streaming::Append => Access::Append,
            Streaming::Truncate => Access::Write,
        };
        let created = matches!(self.leaf, Leaf::Absent { .. });
        let file = self.open(access, Visibility::Public)?;
        let stat = host::seen(&file)?.stat;
        let reader = self
            .leaf_name()
            .filter(|_| stat.kind == Kind::File)
            .and_then(|name| host::derive(&file, self.dir(), name, &stat, Access::Read).ok());
        Ok(Streamed {
            file,
            reader,
            trace: self.trace,
            stat,
            created,
        })
    }

    /// Stage an atomic write beside the leaf: a fresh exclusive sibling in
    /// `dir`, read and write, made with `visibility`.  A present leaf is
    /// staged over only where the user may write it: the rename asks the
    /// directory alone.  Only `Public` takes the leaf's mode, copied from its
    /// stat taken at the walk; the others are owner-only whatever the leaf
    /// was.  Takes `self`: the staged write owns its target from here, so a
    /// sibling can land only over the leaf it was staged beside.
    ///
    /// # Errors
    /// [`data`](Self::data)'s; `InvalidInput` for a special file, held or
    /// stated, which is streamed, not replaced;
    /// [`may_write`](Self::may_write)'s for a present leaf, which is the
    /// leaf's refusal, the host's own; `PermissionDenied` carrying the
    /// directory's refusal where the sibling's create is refused; the
    /// create's otherwise, but for `AlreadyExists`, which is retried under
    /// another name; the mode's.
    pub(crate) fn stage(self, visibility: Visibility) -> io::Result<StagedWrite> {
        match self.data()? {
            Data::Held(_, stat) if stat.kind == Kind::File => self.may_write()?,
            Data::Held(..) | Data::Special => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a special file is streamed, not replaced",
                ));
            }
            Data::Absent => {}
        }
        let (file, name) = loop {
            let name = EntryName::fresh(".ral-write-")?;
            match host::create_sibling(self.dir(), &name, visibility) {
                Ok(file) => break (file, name),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                    return Err(directory_error());
                }
                Err(e) => return Err(e),
            }
        };
        // Latched before the first fallible step, so an early `?` unlinks it.
        let staged = StagedWrite {
            target: self,
            file,
            name,
            settled: false,
        };
        // A Windows file carries no mode beyond its create's attributes.
        #[cfg(unix)]
        if visibility == Visibility::Public
            && let Some(stat) = staged.target.stat()
        {
            host::copy_mode(&staged.file, stat)?;
        }
        Ok(staged)
    }

    /// A data handle for `access`, consuming the object: derived from the held
    /// leaf, or the absent leaf made exclusively with `visibility`.  Never
    /// truncates.  After this the object is the handle.
    ///
    /// # Errors
    /// The open's: `AlreadyExists` where the leaf was absent at the walk and
    /// has appeared since; `IsADirectory` for a directory leaf,
    /// `InvalidInput` for a link, which has no data.
    pub(crate) fn open_data(self, access: Access, visibility: Visibility) -> io::Result<File> {
        self.open(access, visibility)
    }

    /// A data handle for `access` on an absent leaf, made exclusively
    /// (`O_CREAT|O_EXCL|O_NOFOLLOW`) with `visibility`: never over anything
    /// there.  [`open_data`](Self::open_data) makes an absent leaf too, but
    /// opens a present one; this is the make alone.  Takes `self`: after this
    /// the object is the handle.
    ///
    /// # Errors
    /// `AlreadyExists` where the leaf is present, a link included, or has
    /// appeared since the walk; the create's.
    pub(crate) fn create_new(self, access: Access, visibility: Visibility) -> io::Result<File> {
        host::create(self.dir(), self.absent_name()?, access, visibility)
    }

    /// An empty file made exclusively at an absent leaf, its handle dropped:
    /// the name is made and nothing is written.  Takes `self`: after this the
    /// object is the file made.
    ///
    /// # Errors
    /// [`create_new`](Self::create_new)'s.
    pub(crate) fn create_file(self, visibility: Visibility) -> io::Result<()> {
        self.create_new(Access::Write, visibility).map(drop)
    }

    /// Remove the leaf from its directory: a file or a link as the entry, a
    /// directory with everything beneath it, each entry located without
    /// following and removed through the handle of the directory it is in.
    /// Takes `self`: after this the object is gone.  The tree is one object
    /// removed under one judgment, the root's: its entries are not judged
    /// again.  Where the host removes by handle (Windows), the leaf must have
    /// been located as a leaf: a directory the walk entered shares no delete.
    ///
    /// # Errors
    /// `NotFound` for an absent leaf, or one gone before it was removed;
    /// `InvalidInput` for a root; the removal's, a directory that is not
    /// empty by then included.
    pub(crate) fn remove(self) -> io::Result<()> {
        let (kind, name) = self.present()?;
        if let Leaf::Dir { node, stat } = &self.leaf {
            Dir::<Judged<Writing>> {
                node: Arc::clone(node),
                trace: self.trace.clone(),
                stat: stat.clone(),
                _a: PhantomData,
            }
            .empty()?;
        }
        host::remove(self.dir(), self.handle(), name, kind)
    }

    /// Rename the leaf to `to` in its own directory, over whatever `to` is.
    /// Takes `self`: after this the object is at another name.  Where the
    /// host renames by handle (Windows), the leaf must have been located as a
    /// leaf: a directory the walk entered shares no delete.
    ///
    /// # Errors
    /// `NotFound` for an absent leaf; `InvalidInput` for a root; the
    /// rename's.
    pub(crate) fn rename(self, to: &EntryName) -> io::Result<()> {
        let (_, from) = self.present()?;
        host::rename(self.dir(), self.handle(), from, to)
    }

    /// `mkdir` on an absent leaf, then one step onto the name made: the
    /// directory found there, as entered.  Unjudged: the caller judges it as
    /// it would any directory (not provably fresh).
    ///
    /// # Errors
    /// `AlreadyExists` where the leaf is present; the make's or the step's.
    pub(crate) fn create_dir(self, visibility: Visibility) -> io::Result<Dir> {
        let (Leaf::Absent { dir }, Some(name)) = (&self.leaf, self.leaf_name()) else {
            return Err(io::ErrorKind::AlreadyExists.into());
        };
        host::mkdir(&dir.held.file, name, visibility)?;
        let (held, step, stat) = step_into(&dir.held, name, Sharing::Directory)?.held()?;
        if step.kind != Kind::Dir {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(Dir {
            node: Arc::new(Node {
                held,
                above: Some(Arc::clone(dir)),
            }),
            trace: self.trace.extended([step], Vec::new()),
            stat,
            _a: PhantomData,
        })
    }
}

impl StagedWrite {
    /// Written through before [`publish`](Self::publish).
    pub(crate) fn file(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn target(&self) -> &Located<Judged<Writing>> {
        &self.target
    }

    /// Commit: sync the sibling, then rename it over the leaf's name.  Where
    /// the host renames by name, as Unix does, the source is `name` under
    /// `dir`: POSIX has no publish-by-handle, so a process with write
    /// permission on the directory can put a substitute at the sibling's name
    /// between the sync and the rename.  On failure the sibling is unlinked
    /// before the error returns, so a failed publish leaves the target as it
    /// was and nothing beside it.  Then the directory is synced, best effort.
    /// No check follows: what the name holds later is not this write's to
    /// assert.
    ///
    /// # Errors
    /// The sync's, or the rename's.
    pub(crate) fn publish(mut self) -> io::Result<()> {
        // Data blocks before any directory entry, or a crash can commit the
        // entry with the blocks still unwritten: a renamed zero-length file.
        self.file.sync_all()?;
        self.settled = true;
        let to = self
            .target
            .leaf_name()
            .expect("a staged write targets a leaf with a name");
        let landed = host::rename_over(self.target.dir(), &self.file, &self.name, to);
        if landed.is_err() {
            let _ = host::discard(self.target.dir(), &self.file, &self.name);
        } else {
            // Windows has no directory flush.
            #[cfg(unix)]
            let _ = host::sync_dir(self.target.dir());
        }
        landed
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        if !self.settled {
            let _ = host::discard(self.target.dir(), &self.file, &self.name);
        }
    }
}
