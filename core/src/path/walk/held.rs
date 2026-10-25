//! Held handles and the sealed authority that permits their use.
//!
//! Traversal, operations and host adapters are children to share the private
//! representation. `host::step` opens one child without following links;
//! `unix` supplies the primitives the Unix hosts share.

use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::exclusion::Hold;
use super::names::{EntryName, RootName};
use super::trace::{Kind, ObjectId, RealPath, Seen, Stat, Step, Trace};
// Inverted edge, deliberate: `Verdict`'s field is private to its module.
use crate::authority::policy::{FsOp, Verdict};

mod directory;
mod read;
mod write;

#[path = "resolve.rs"]
pub(super) mod resolve;

pub(crate) use read::open_discard;
pub(crate) use resolve::walk_dir_made;
pub use resolve::{LeafMode, exists, is_dir, kind_of, walk, walk_dir, walk_lenient};
#[cfg(unix)]
pub(crate) use resolve::{Vetted, walk_with};
pub use write::Visibility;
pub(crate) use write::{Access, Streaming};

#[cfg(unix)]
#[path = "unix.rs"]
mod unix;

#[cfg(target_os = "linux")]
#[path = "linux.rs"]
mod linux;
#[cfg(target_os = "linux")]
use linux as host;

#[cfg(target_os = "macos")]
#[path = "macos.rs"]
mod macos;
#[cfg(target_os = "macos")]
use macos as host;

#[cfg(windows)]
#[path = "windows.rs"]
mod windows;
#[cfg(windows)]
use windows as host;

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
#[path = "tests.rs"]
mod tests;

#[cfg(any(test, feature = "test-util"))]
thread_local! {
    /// How many roots and components the walks on this thread have opened.
    static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many roots and components the walks on this thread have opened.
#[cfg(any(test, feature = "test-util"))]
pub(crate) fn steps_taken() -> usize {
    STEPS.with(std::cell::Cell::get)
}

/// One open a walk makes, counted under test.
fn counted() {
    #[cfg(any(test, feature = "test-util"))]
    STEPS.with(|taken| taken.set(taken.get() + 1));
}

/// [`host::step`], counted under test.
fn step_into(parent: &Held, name: &EntryName, sharing: Sharing) -> io::Result<Child> {
    counted();
    host::step(parent, name, sharing)
}

mod private {
    pub trait Sealed {}
}

/// Who vouched for a held handle, and so what may be done through it.
///
/// [`Unjudged`]: nobody, what the walk returns.  [`Judged<O>`]: the grant
/// stack, for the op `O`; made by `Located::judged` and `Dir::judged` against
/// a [`Verdict<O>`], whose field is private to `capability/fs.rs`.
/// [`Installing`]: the OS sandbox side, at a position it is installing rules
/// at; made by the two `RealPath::open_*_nofollow` against an
/// `InstallToken`, whose field is private to `sandbox.rs`.  What a holder
/// may do is a trait on its marker: [`Peeks`] (metadata), [`Lists`]
/// (enumerate and descend), [`Reads`] (bytes out), and writes on
/// `Judged<Writing>` alone.  Sealed.
pub trait Authority: private::Sealed {}

/// Nobody has vouched: what the walk returns.
pub enum Unjudged {}

/// The grant stack vouched, for the op `O`.
pub struct Judged<O: Op>(PhantomData<O>);

/// The OS sandbox side holds it, at a position it is installing rules at.
pub enum Installing {}

impl private::Sealed for Unjudged {}
impl<O: Op> private::Sealed for Judged<O> {}
impl private::Sealed for Installing {}
impl Authority for Unjudged {}
impl<O: Op> Authority for Judged<O> {}
impl Authority for Installing {}

/// May stat and read a link's target: the grant said so, or the sandbox is
/// rendering.  `file-info` on a denied name is refused, and a stat is what it
/// answers with.
pub trait Peeks: Authority {}
impl<O: Op> Peeks for Judged<O> {}
impl Peeks for Installing {}

/// May list a directory and descend.
///
/// `Child` is what a descent is marked: the sandbox side keeps its authority,
/// since it has no judge to ask again and renders a denied tree too; a judged
/// directory's children are unjudged, and the region judges each on entry.
pub trait Lists: Peeks {
    type Child: Authority;
}
impl<O: Op> Lists for Judged<O> {
    type Child = Unjudged;
}
impl Lists for Installing {
    type Child = Self;
}

/// May read bytes: the grant said so.  Writing reads, so every `Judged<O>`.
pub trait Reads: Peeks {}
impl<O: Op> Reads for Judged<O> {}

/// The two fs ops as types, so a judgment's op is in the handle's type;
/// `FS` is the runtime value the fold uses.  `Reading` and `Writing`, not
/// `Read` and `Write`: those are `std::io`'s traits.  Sealed.
pub trait Op: private::Sealed {
    const FS: FsOp;
}

pub enum Reading {}
pub enum Writing {}

impl private::Sealed for Reading {}
impl private::Sealed for Writing {}
impl Op for Reading {
    const FS: FsOp = FsOp::Read;
}
impl Op for Writing {
    const FS: FsOp = FsOp::Write;
}

/// How a child is opened where a host shares by handle: a directory step
/// holds the directory itself in place (Windows: `FILE_TRAVERSE` without
/// delete sharing), a leaf step blocks no one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sharing {
    Directory,
    Leaf,
}

/// A directory the walk holds: its handle, shared by the node and by the
/// leaves found in it, and what the host recorded of it as it opened it
/// (`host::DirInfo`: Windows keeps the normalized path and volume the next
/// step checks its child against, Unix nothing).
struct Held {
    file: Arc<File>,
    #[cfg_attr(
        unix,
        expect(
            dead_code,
            reason = "read by Windows' step, which checks a child against its directory"
        )
    )]
    info: host::DirInfo,
}

/// A directory the walk holds and the one it was opened from.
///
/// `above` is `None` exactly at a root.  Shared: every directory opened
/// beneath holds this one by reference, so a walk of depth d holds d handles
/// whatever it hands out.
struct Node {
    held: Held,
    above: Option<Arc<Self>>,
}

impl Node {
    /// The directory this one is in; a root is its own, as `/..` says.
    fn container(self: &Arc<Self>) -> &Arc<Self> {
        self.above.as_ref().unwrap_or(self)
    }
}

/// What [`host::step`] returns: the handle opened, what it is, and the
/// host's record of it, which it keeps if it is a directory.  `handle` is
/// `None` only where the host will not hold the leaf: macOS, for a leaf the
/// user cannot open or a special file — a FIFO, a socket, a device — since
/// holding one is opening it.  It is then [`Leaf::Stated`]: a link or a
/// directory is always held.
struct Child {
    handle: Option<File>,
    step: Step,
    stat: Stat,
    info: host::DirInfo,
}

impl Child {
    /// The child as a held directory, with its step and stat; an error for
    /// one that could not be held.
    fn held(self) -> io::Result<(Held, Step, Stat)> {
        let file = self.handle.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the object could not be held, so it is not entered",
            )
        })?;
        let held = Held {
            file: Arc::new(file),
            info: self.info,
        };
        Ok((held, self.step, self.stat))
    }
}

/// An open directory: the node the walk holds it by, its trace, and its stat
/// taken once at the open.
///
/// The node is its handle and, shared, those of the directories the walk
/// opened above it, one per step of its trace.  `A` says who vouched for it.
#[must_use]
pub struct Dir<A: Authority = Unjudged> {
    node: Arc<Node>,
    trace: Trace,
    stat: Stat,
    _a: PhantomData<A>,
}

impl<A: Authority> Dir<A> {
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn real(&self) -> &RealPath {
        self.trace.real()
    }

    /// This directory, vouched for: the one place `Unjudged` becomes
    /// `Judged<O>`, through the token only `capability/fs.rs` can make.
    pub(crate) fn judged<O: Op>(self, _: Verdict<O>) -> Dir<Judged<O>> {
        self.marked()
    }

    /// The same directory under another marker: this module's own step.
    fn marked<B: Authority>(self) -> Dir<B> {
        Dir {
            node: self.node,
            trace: self.trace,
            stat: self.stat,
            _a: PhantomData,
        }
    }
}

/// A listing in progress: the sorted names, stated as they are yielded.
pub struct Listing<'a> {
    parent: &'a Trace,
    lister: host::Lister<'a>,
}

/// One listing entry, borrowing the trace of the directory it was listed from.
///
/// It is judged only as that directory's entry: its name and, where
/// the stat after the listing succeeded, what it found; else why it did not
/// (`NotFound` for an entry gone meanwhile), so a lister tells "unstatable"
/// from "absent".
pub struct DirEntry<'a> {
    parent: &'a Trace,
    name: EntryName,
    seen: io::Result<Seen>,
}

/// A listing's record of one entry: the parent's trace and one step made from
/// the listing's kind and identity.  Not a [`Trace`]: its step was stated by
/// name after the listing, not opened, and the type says so, so nothing that
/// wants a walk's evidence can be handed a listing's.
pub(crate) struct Listed<'a> {
    parent: &'a Trace,
    step: Step,
}

/// An object named exactly: its trace and its leaf, which carries what the
/// operations on it need.
///
/// The leaf is absent exactly when `trace.tail()` is non-empty, and `trace` is
/// the only copy of the leaf's name.  `linked` is where a link at the
/// spelling's last name led, as its target spelled it, where the walk followed
/// one there.  `A` says who vouched for it.
#[must_use]
pub struct Located<A: Authority = Unjudged> {
    trace: Trace,
    leaf: Leaf,
    linked: Option<PathBuf>,
    _a: PhantomData<A>,
}

/// What the walk has of the leaf.  Host-neutral in the type; only the macOS
/// host makes `Stated`, and nothing opens a `Stated` leaf afterwards.
enum Leaf {
    /// Nothing at the name: a create target in `dir`, which keeps the
    /// directories above it, since what `create_dir` makes there is entered.
    Absent { dir: Arc<Node> },
    /// A directory, held as a node — entered (directory sharing) or reached
    /// as the last name (leaf sharing); `node.above` is the directory it is
    /// in, and the root is its own.
    Dir { node: Arc<Node>, stat: Stat },
    /// Anything else, held: its handle and stat, beside the handle of the
    /// directory it is in — that handle alone, never the ones above.
    Held {
        dir: Arc<File>,
        file: File,
        stat: Stat,
    },
    /// macOS alone: a leaf the host will not hold — its permissions refuse
    /// it, or it is a special file (a FIFO, a socket, a device), which macOS
    /// cannot hold without opening — its stat, and its directory's handle.
    Stated { dir: Arc<File>, stat: Stat },
}

/// What a leaf offers a data operation: nothing yet (a create target), the handle
/// held and its stat, or a special file — a FIFO, a socket, a device — which
/// only macOS states without holding.
enum Data<'a> {
    Absent,
    Held(&'a File, &'a Stat),
    Special,
}

impl<A: Authority> Located<A> {
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn real(&self) -> &RealPath {
        self.trace.real()
    }

    /// Where a link at the spelling's last name led, as its target spelled it;
    /// `None` where that name was no link, or was not followed.
    pub(crate) fn linked_to(&self) -> Option<&Path> {
        self.linked.as_deref()
    }

    /// This object, vouched for: the one place `Unjudged` becomes
    /// `Judged<O>`, through the token only `capability/fs.rs` can make.
    pub(crate) fn judged<O: Op>(self, _: Verdict<O>) -> Located<Judged<O>> {
        self.marked()
    }

    /// The same object under another marker: this module's own step.
    fn marked<B: Authority>(self) -> Located<B> {
        Located {
            trace: self.trace,
            leaf: self.leaf,
            linked: self.linked,
            _a: PhantomData,
        }
    }

    /// What the walk stated of the leaf, for this module's own use: `None` when
    /// it is absent.
    fn observed(&self) -> Option<&Stat> {
        match &self.leaf {
            Leaf::Absent { .. } => None,
            Leaf::Dir { stat, .. } | Leaf::Held { stat, .. } | Leaf::Stated { stat, .. } => {
                Some(stat)
            }
        }
    }

    /// The handle of the directory the leaf is in: the root's own for the
    /// root.
    fn dir(&self) -> &File {
        match &self.leaf {
            Leaf::Absent { dir } => &dir.held.file,
            Leaf::Dir { node, .. } => &node.container().held.file,
            Leaf::Held { dir, .. } | Leaf::Stated { dir, .. } => dir,
        }
    }

    /// The leaf's own handle, where the walk holds one.
    fn handle(&self) -> Option<&File> {
        match &self.leaf {
            Leaf::Held { file, .. } => Some(file),
            Leaf::Dir { node, .. } => Some(&node.held.file),
            Leaf::Absent { .. } | Leaf::Stated { .. } => None,
        }
    }

    /// The object the walk ended on, as an exclusion holds it: its trace and,
    /// on Unix, the one handle the walk opened on the deepest object it
    /// reached, which an absent leaf's is its directory; a hold keeps nothing
    /// above it.  A `Stated` leaf has none.
    pub(super) fn into_hold(self) -> Hold {
        Hold::new(
            self.trace,
            #[cfg(unix)]
            match self.leaf {
                Leaf::Held { file, .. } => Some(Arc::new(file)),
                Leaf::Dir { node, .. } | Leaf::Absent { dir: node } => {
                    Some(Arc::clone(&node.held.file))
                }
                Leaf::Stated { .. } => None,
            },
        )
    }

    /// The leaf's name: the last step's, or the one name of a create target.
    /// `None` for the root.
    fn leaf_name(&self) -> Option<&EntryName> {
        self.trace
            .tail()
            .first()
            .or_else(|| self.trace.steps().last().map(Step::name))
    }

    /// The leaf's name as an entry of `dir`, `.` for the root.
    fn entry(&self) -> &OsStr {
        self.leaf_name()
            .map_or_else(|| OsStr::new("."), EntryName::as_os_str)
    }

    /// The leaf as one whose data may be opened or replaced: held, with its
    /// handle and stat; a special file, which macOS states and never holds;
    /// or a create target.
    ///
    /// # Errors
    /// `IsADirectory` for a directory; `InvalidInput` for a link, which has
    /// no data; `PermissionDenied` for any other stated leaf, the walk's
    /// refusal to hold it, after which nothing is opened or replaced through
    /// its name.
    fn data(&self) -> io::Result<Data<'_>> {
        match &self.leaf {
            Leaf::Dir { .. } => Err(io::ErrorKind::IsADirectory.into()),
            Leaf::Held { stat, .. } if stat.kind == Kind::Symlink => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a link has no data",
            )),
            Leaf::Held { file, stat, .. } => Ok(Data::Held(file, stat)),
            Leaf::Stated { stat, .. } if stat.kind == Kind::Other => Ok(Data::Special),
            Leaf::Stated { .. } => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the object could not be held, so nothing is opened through it",
            )),
            Leaf::Absent { .. } => Ok(Data::Absent),
        }
    }

    /// The name of an absent leaf, a create target.
    ///
    /// # Errors
    /// `AlreadyExists` where the leaf is present.
    fn absent_name(&self) -> io::Result<&EntryName> {
        match (&self.leaf, self.leaf_name()) {
            (Leaf::Absent { .. }, Some(name)) => Ok(name),
            _ => Err(io::ErrorKind::AlreadyExists.into()),
        }
    }

    /// The leaf's kind and its name as an entry of its directory, for what
    /// acts on the entry: a removal, a rename.
    ///
    /// # Errors
    /// `NotFound` for an absent leaf; `InvalidInput` for a root, which is no
    /// directory's entry.
    fn present(&self) -> io::Result<(Kind, &EntryName)> {
        let kind = self
            .observed()
            .map(|stat| stat.kind)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))?;
        let name = self.leaf_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a root is no directory's entry",
            )
        })?;
        Ok((kind, name))
    }

    /// This object as a directory, under the same authority, where the walk
    /// holds it as one, entered or reached as a leaf; handed back otherwise.
    ///
    /// # Errors
    /// `self`, where the leaf is not a held directory.
    #[expect(
        clippy::result_large_err,
        reason = "the object handed back is not an error"
    )]
    pub fn into_dir(self) -> Result<Dir<A>, Self> {
        match self.leaf {
            Leaf::Dir { node, stat } => Ok(Dir {
                node,
                trace: self.trace,
                stat,
                _a: PhantomData,
            }),
            leaf => Err(Self { leaf, ..self }),
        }
    }
}

/// A streaming write, opened through the held leaf: the write handle, a read
/// handle on the same object for the before- and after-images where the leaf
/// is a regular file the caller may read, the stat of the handle opened, and
/// whether the leaf was created.
#[must_use]
pub(crate) struct Streamed {
    file: File,
    reader: Option<File>,
    trace: Trace,
    stat: Stat,
    created: bool,
}

/// A staged atomic write: the target, the exclusive sibling and its name,
/// owned together until [`publish`](Self::publish) consumes them, so the two
/// never part and the sibling lands only over the leaf it was staged beside.
/// `Drop` unlinks the sibling: a staged write never outlives the frame that
/// staged it, whatever the frame does.  The rename mints a fresh inode: a
/// hard link to the old one keeps the old contents, and owner, xattrs and
/// ACLs fall to kernel inheritance.
#[must_use]
pub(crate) struct StagedWrite {
    target: Located<Judged<Writing>>,
    file: File,
    name: EntryName,
    settled: bool,
}
