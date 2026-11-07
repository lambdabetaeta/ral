//! Metadata, listing and descent through a held directory.

#[cfg(windows)]
use std::fs::File;
use std::io;
use std::marker::PhantomData;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, BorrowedFd};
use std::sync::Arc;

#[cfg(any(target_os = "linux", windows))]
use super::Installing;
use super::{
    Dir, DirEntry, EntryName, Kind, Leaf, LeafMode, Listed, Listing, Lists, Located, Node, Peeks,
    RealPath, Seen, Sharing, Stat, Step, Trace, host, step_into,
};
use crate::path::shown;

impl<A: Peeks> Dir<A> {
    pub(crate) fn stat(&self) -> &Stat {
        &self.stat
    }
}

/// The bwrap handoff and Landlock lend the handle, never a name.
/// `Installing` alone: an `O_PATH` directory descriptor opens children,
/// which an unjudged holder may not.
#[cfg(target_os = "linux")]
impl AsFd for Dir<Installing> {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.node.held.file.as_fd()
    }
}

#[cfg(windows)]
impl Dir<Installing> {
    /// A handle on the directory with `access`, sharing read, write and
    /// delete, derived from the held one: what a DACL stamp needs beyond
    /// attributes.
    ///
    /// # Errors
    /// The reopen's.
    pub(crate) fn reopen(&self, access: u32) -> io::Result<File> {
        host::reopen_shared(&self.node.held.file, access)
    }
}

impl<A: Lists> Dir<A> {
    /// The entries of this directory from the held handle, in name order,
    /// each stated as it is yielded, nothing followed: the names are read
    /// first, from a handle reopened for listing, then each is stated as the
    /// iterator reaches it, so a caller polls its cancel scope between
    /// entries.  An entry whose stat fails is yielded with that error, since
    /// "listed, then unstatable" is a state a manifest keeps apart from
    /// absence, never dropped.
    ///
    /// # Errors
    /// The reopen's: a directory that cannot be listed refuses here, not at
    /// the walk.
    pub fn read_dir(&self) -> io::Result<Listing<'_>> {
        Ok(Listing {
            parent: &self.trace,
            lister: host::list(&self.node.held.file)?,
        })
    }

    /// Open the entry `name` as a directory: one step with directory
    /// sharing.  A link or a non-directory at `name` is an error.  The one
    /// child-descent primitive, keeping its authority or losing it as
    /// [`Lists::Child`] says.
    ///
    /// # Errors
    /// The open's; `NotADirectory` for a link or anything else but a directory.
    pub fn open_dir(&self, name: &EntryName) -> io::Result<Dir<A::Child>> {
        let child = step_into(&self.node.held, name, Sharing::Directory)?;
        match child.step.kind {
            Kind::Dir => {
                let (held, step, stat) = child.held()?;
                Ok(Dir {
                    node: Arc::new(Node {
                        held,
                        above: Some(Arc::clone(&self.node)),
                    }),
                    trace: self.trace.extended([step], Vec::new()),
                    stat,
                    _a: PhantomData,
                })
            }
            Kind::Symlink => Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                format!("{} is a link, not a directory", shown(name.as_os_str())),
            )),
            _ => Err(io::ErrorKind::NotADirectory.into()),
        }
    }

    /// The entry `name` as an object: one leaf step under the held handle,
    /// `mode` deciding a link.  A link at `name` is followed as any walk
    /// follows one: a `..` in its target climbs the directories this one was
    /// opened from, and at a root stays.
    ///
    /// # Errors
    /// The walk's; `NotFound` where there is no entry `name`.
    pub fn locate(&self, name: &EntryName, mode: LeafMode) -> io::Result<Located<A::Child>> {
        let located = self.target(name, mode)?;
        match located.leaf {
            Leaf::Absent { .. } => Err(io::ErrorKind::NotFound.into()),
            _ => Ok(located),
        }
    }
}

impl<'a> Iterator for Listing<'a> {
    type Item = DirEntry<'a>;

    fn next(&mut self) -> Option<DirEntry<'a>> {
        let (name, seen) = self.lister.next()?;
        Some(DirEntry {
            parent: self.parent,
            name,
            seen,
        })
    }
}

impl DirEntry<'_> {
    pub fn name(&self) -> &EntryName {
        &self.name
    }

    /// The position of this entry in the directory it was listed from, for
    /// the region that judges an unseen one: no object is behind it.
    pub(crate) fn real(&self) -> RealPath {
        self.parent.real().child(&self.name)
    }

    pub(crate) fn seen(&self) -> Result<&Seen, &io::Error> {
        self.seen.as_ref()
    }

    /// What the listing saw of this entry, for judging the name shown, never
    /// an object used; an object used is walked.  `None` where the entry was
    /// not seen, since it has no identity to record.
    pub(crate) fn listed(&self) -> Option<Listed<'_>> {
        let seen = self.seen.as_ref().ok()?;
        Some(Listed {
            parent: self.parent,
            step: Step {
                name: self.name.clone(),
                id: seen.id,
                kind: seen.stat.kind,
            },
        })
    }
}

impl Listed<'_> {
    pub(crate) fn real(&self) -> RealPath {
        self.parent.real().child(&self.step.name)
    }

    pub(in crate::path) fn parent(&self) -> &Trace {
        self.parent
    }

    pub(in crate::path) fn step(&self) -> &Step {
        &self.step
    }
}
