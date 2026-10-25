//! The traversal engine and its spelling-based entry points.

use std::collections::VecDeque;
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use super::super::error::{dangling_error, loop_error};
use super::super::exclusion::{Excluded, Hold};
use super::super::names::{EntryName, Name, Origin, RootName, Spelling, rooted, split};
use super::super::trace::{Kind, ObjectId, RealPath, Step, Trace};
#[cfg(any(target_os = "linux", windows))]
use super::Installing;
#[cfg(test)]
use super::tests;
use super::{
    Authority, Child, Dir, Held, Leaf, Lists, Located, Node, Sharing, Visibility, counted, host,
    step_into,
};
#[cfg(any(target_os = "linux", windows))]
use crate::authority::sandbox::InstallToken;
use crate::path::lex::{is_absolute, names_a_dir};
use crate::path::site::Silent;
use crate::path::{LexicalPath, shown};
use crate::site::site;

/// `SYMLOOP_MAX`'s customary value: a name still splicing past this is a cycle.
pub(super) const MAX_HOPS: usize = 40;

/// How the walk treats a link at the last component.  A spelling that names a
/// directory (`d/`, `d/..`, a link target `f/`) and [`walk_dir`] have no
/// mode: the directory is entered and a link there followed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum LeafMode {
    /// Splice it and go on: what an open wants.
    Follow,
    /// Stop at the link: what `exists`, `is-link`, `file-info` want.
    NoFollow,
}

/// How the walk treats a link anywhere: splice it (every operation but two), or
/// refuse the walk (the position is already real, and a link met now is a
/// redirection).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(in crate::path::walk) enum Links {
    Splice,
    Refuse,
}

/// What the walk does at the last name: [`LeafMode`]'s two, and `Enter`,
/// where the spelling names a directory and the directory entered is the
/// object.  Never a caller's choice.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::path::walk) enum Mode {
    Follow,
    NoFollow,
    Enter,
}

impl From<LeafMode> for Mode {
    fn from(mode: LeafMode) -> Self {
        match mode {
            LeafMode::Follow => Self::Follow,
            LeafMode::NoFollow => Self::NoFollow,
        }
    }
}

/// What the walk does at a name it cannot enter: refuse (an open names an
/// object that is there; a last name that is absent is a create target), stop
/// and fold the names left onto it lexically, resuming the walk where they
/// fold it away ([`Walker::stopped`]), or make it a directory and step onto
/// it ([`Walker::open`]).  A name a link's target spliced in is never made.
/// `Make` is for [`Mode::Enter`] alone: every name it takes is a directory.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::path::walk) enum OnMissing {
    Fail,
    Stop,
    Make,
}

/// One object the walk opened, offered to a vet before the walk relies on it:
/// its step, the handle just opened, and the directory handle and name it was
/// opened under (`name` is `.` and `dir` the object itself for the root,
/// whose `step` is `None` since it is no entry).  `child` is `None` for a
/// leaf the host refused to hold, or (macOS) a special file — a FIFO, a
/// socket, a device — which the host will not hold, since holding one is
/// opening it; a vet that needs the object refuses either.
/// Every object: the root, each directory on entry, each link before it is
/// spliced, the leaf.
#[cfg_attr(
    windows,
    expect(dead_code, reason = "read by `carriers`, which is Unix-only")
)]
pub(crate) struct Vetted<'a> {
    pub step: Option<&'a Step>,
    pub child: Option<&'a File>,
    pub dir: &'a File,
    pub name: &'a OsStr,
}

impl RealPath {
    /// Where `spelling` leads, which must exist: [`walk`] with
    /// [`LeafMode::Follow`].
    ///
    /// # Errors
    /// `NotFound` for an absent leaf; otherwise the walk's error.
    pub fn of(spelling: &LexicalPath) -> io::Result<Self> {
        let located = walk(spelling, LeafMode::Follow)?;
        if located.observed().is_none() {
            return Err(io::ErrorKind::NotFound.into());
        }
        Ok(located.trace.into_real())
    }

    /// What is at this position now, reached through no link: the walk with
    /// [`Links::Refuse`], held for the OS sandbox side, which proves it is
    /// that side by the token.  `None` where nothing is there; `Err` with
    /// `ELOOP`'s wording ([`is_loop`](super::super::error::is_loop)) where a
    /// link is, since a redirected position refuses the launch.  A file or a directory alike.
    ///
    /// # Errors
    /// The walk's.
    #[cfg(any(target_os = "linux", windows))]
    pub(crate) fn open_nofollow(
        &self,
        _: &InstallToken,
    ) -> io::Result<Option<Located<Installing>>> {
        use io::ErrorKind::{NotADirectory, NotFound};
        match finish(
            self.as_path(),
            Mode::NoFollow,
            OnMissing::Fail,
            Links::Refuse,
            |_| true,
        ) {
            Ok(located) if located.observed().is_some() => Ok(Some(located.marked())),
            Ok(_) => Ok(None),
            Err(e) if matches!(e.kind(), NotFound | NotADirectory) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// [`open_nofollow`](Self::open_nofollow) where the position must be a
    /// directory to descend from: `None` also where it is not one.  Entered
    /// as a directory, so held with directory sharing.
    ///
    /// # Errors
    /// The walk's.
    #[cfg(any(target_os = "linux", windows))]
    pub(crate) fn open_dir_nofollow(
        &self,
        _: &InstallToken,
    ) -> io::Result<Option<Dir<Installing>>> {
        use io::ErrorKind::{NotADirectory, NotFound};
        match finish(
            self.as_path(),
            Mode::Enter,
            OnMissing::Fail,
            Links::Refuse,
            |_| true,
        ) {
            Ok(located) => Ok(located.into_dir().ok().map(Dir::marked)),
            Err(e) if matches!(e.kind(), NotFound | NotADirectory) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

impl<A: Lists> Dir<A> {
    /// The entry `name` as the object it is or, where nothing is there, the
    /// create target it would be: under [`LeafMode::Follow`], through a link
    /// at `name`, the target's, with [`Located::linked_to`] saying where the
    /// link led.  [`locate`](Self::locate) refuses the second.
    ///
    /// # Errors
    /// The walk's.  Where the link at `name` leads to a name beneath a
    /// directory that is not there, `NotFound`, which
    /// [`dangling`](super::super::error::dangling) reads.
    pub(crate) fn target(&self, name: &EntryName, mode: LeafMode) -> io::Result<Located<A::Child>> {
        let names = VecDeque::from([Name::Entry(name.clone(), Origin::Spelled)]);
        let walker = Walker::new(
            Cursor::beneath(self),
            names,
            mode.into(),
            OnMissing::Fail,
            Links::Splice,
            |_| true,
        );
        walker.run().map(Located::marked)
    }

    /// Follow the link entry `name` to the directory it names: the link is
    /// opened as a step, its target read through that handle, and the walk
    /// continued as a spliced link is — a relative target from this
    /// directory's own handle, a `..` in it climbing the directories this one
    /// was opened from and at a root staying; an absolute one from the root;
    /// never from [`real`](Self::real) by name.  The caller judges the result
    /// as a fresh walk.
    ///
    /// # Errors
    /// The walk's; `InvalidInput` where `name` is not a link.
    pub(crate) fn open_dir_via_link(&self, name: &EntryName) -> io::Result<Dir<A::Child>> {
        let link = step_into(&self.node.held, name, Sharing::Leaf)?;
        let (link, step, _) = link.held()?;
        if step.kind != Kind::Symlink {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a link", shown(name.as_os_str())),
            ));
        }
        let target = host::read_link(&link.file)?;
        let mut walker = Walker::new(
            Cursor::beneath(self),
            VecDeque::new(),
            Mode::Enter,
            OnMissing::Fail,
            Links::Splice,
            |_| true,
        );
        walker.splice(&step.name, &target)?;
        let dir = walker
            .run()?
            .into_dir()
            .map_err(|_| io::Error::from(io::ErrorKind::NotADirectory))?;
        Ok(dir.marked())
    }
}

/// Where the walk stands: a base node — a root, or the directory a walk began
/// beneath, with the directories the walk opened above it — its trace, and a
/// stack of `(handle, step)` for every directory entered since.  The cursor's
/// handles are what every open is relative to, and its steps are the trace as
/// it grows.  A `..` pops the stack, so the walk returns to the directory it
/// already holds, never to the kernel's current parent.  Past the stack it
/// climbs the base's own ancestry, and at a root it stays.
struct Cursor {
    base: Arc<Node>,
    trace: Trace,
    stack: Vec<(Held, Step)>,
}

impl Cursor {
    fn root(root: RootName) -> io::Result<Self> {
        counted();
        let held = host::open_root(&root)?;
        let id = ObjectId::of(&held.file)?;
        Ok(Self {
            base: Arc::new(Node { held, above: None }),
            trace: Trace::new(root, id, Vec::new(), Vec::new(), Vec::new()),
            stack: Vec::new(),
        })
    }

    /// A cursor standing in `dir`.
    fn beneath<A: Authority>(dir: &Dir<A>) -> Self {
        Self {
            base: Arc::clone(&dir.node),
            trace: dir.trace.clone(),
            stack: Vec::new(),
        }
    }

    fn top(&self) -> &Held {
        self.stack.last().map_or(&self.base.held, |(h, _)| h)
    }

    fn push(&mut self, handle: Held, step: Step) {
        self.stack.push((handle, step));
    }

    /// Where the cursor stands.
    fn position(&self) -> RealPath {
        (self.stack.iter()).fold(self.trace.real().clone(), |at, (_, step)| {
            at.child(&step.name)
        })
    }

    /// Climb out of the directory on top, which the walk has then passed;
    /// at a root there is nothing above, and the walk stays.
    fn up(&mut self) {
        let left = self.position();
        if self.stack.pop().is_none() {
            let Some(above) = self.base.above.clone() else {
                return;
            };
            self.base = above;
            self.trace = self.trace.above();
        }
        self.trace.passed_mut().push(left);
    }

    /// The trace of where the cursor stands, plus a step opened below it and
    /// the names past.
    fn trace(&self, step: Option<Step>, tail: Vec<EntryName>) -> Trace {
        let steps = self.stack.iter().map(|(_, step)| step.clone());
        self.trace.extended(steps.chain(step), tail)
    }

    /// The directory on top, as the node it is.
    fn into_top(self) -> Arc<Node> {
        self.stack.into_iter().fold(self.base, |above, (held, _)| {
            Arc::new(Node {
                held,
                above: Some(above),
            })
        })
    }
}

/// Where a walk's names ran out: on a leaf, in the directory on top of the
/// cursor, or past it at names that are not there, the tail.
enum Ended {
    Leaf(Child),
    Entered,
    Absent(Vec<EntryName>),
}

/// The walk's loop: a cursor, the names still to take, and how to treat the
/// last one.  `linked` is the target of the link followed at the spelling's
/// last name, which a refusal past it reports as a dangling one.
struct Walker<V> {
    cursor: Cursor,
    names: VecDeque<Name>,
    mode: Mode,
    missing: OnMissing,
    links: Links,
    vet: V,
    splices: usize,
    linked: Option<PathBuf>,
}

impl<V: FnMut(&Vetted<'_>) -> bool> Walker<V> {
    fn new(
        cursor: Cursor,
        names: VecDeque<Name>,
        mode: Mode,
        missing: OnMissing,
        links: Links,
        vet: V,
    ) -> Self {
        let mut walker = Self {
            cursor,
            names,
            mode,
            missing,
            links,
            vet,
            splices: 0,
            linked: None,
        };
        walker.settle_mode();
        walker
    }

    /// A walk that ends on a `..`, or on no name at all, names a directory.
    fn settle_mode(&mut self) {
        if self
            .names
            .back()
            .is_none_or(|name| matches!(name, Name::Parent))
        {
            self.mode = Mode::Enter;
        }
    }

    fn refused() -> io::Error {
        io::Error::new(
            io::ErrorKind::PermissionDenied,
            "the walk passed an object its vetting refused",
        )
    }

    /// Put the root the cursor stands at to the vet.
    fn vet_root(&mut self) -> io::Result<()> {
        let root = &self.cursor.top().file;
        let vetted = Vetted {
            step: None,
            child: Some(root),
            dir: root,
            name: OsStr::new("."),
        };
        (self.vet)(&vetted).then_some(()).ok_or_else(Self::refused)
    }

    /// Put `child`, opened as the entry `name` of the cursor's directory, to
    /// the vet.
    fn check(&mut self, child: &Child, name: &EntryName) -> io::Result<()> {
        let vetted = Vetted {
            step: Some(&child.step),
            child: child.handle.as_ref(),
            dir: &self.cursor.top().file,
            name: name.as_os_str(),
        };
        (self.vet)(&vetted).then_some(()).ok_or_else(Self::refused)
    }

    /// Replace `link`, the stored name of the link just met beneath the cursor,
    /// now passed, with its `target`: the names of a relative one continue from
    /// the cursor, those of an absolute one from its root.
    /// Whether the walk must end in a directory is re-asked of the whole
    /// spliced spelling, as the target's trailing separator and what is still
    /// to come say.
    fn splice(&mut self, link: &EntryName, target: &Path) -> io::Result<()> {
        let passed = self.cursor.position().child(link);
        self.cursor.trace.passed_mut().push(passed);
        self.splices += 1;
        if self.splices > MAX_HOPS {
            return Err(loop_error());
        }
        if target.as_os_str().is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "a symlink with an empty target names nothing",
            ));
        }
        let Spelling { root, names } = split(target)?;
        if let Some(root) = root {
            rooted(target)?;
            let mut restarted = Cursor::root(root)?;
            *restarted.trace.passed_mut() = std::mem::take(self.cursor.trace.passed_mut());
            self.cursor = restarted;
            self.vet_root()?;
        } else if is_absolute(target) {
            rooted(target)?;
        }
        if self.names.is_empty() && names_a_dir(target) {
            self.mode = Mode::Enter;
        }
        for name in names.into_iter().rev() {
            self.names.push_front(name.spliced());
        }
        self.settle_mode();
        Ok(())
    }

    fn run(mut self) -> io::Result<Located> {
        let ended = self.advance()?;
        self.conclude(ended)
    }

    /// Consume the names, to where the walk ends.
    fn advance(&mut self) -> io::Result<Ended> {
        while let Some(name) = self.names.pop_front() {
            if let ControlFlow::Break(ended) = self.take(name)? {
                return Ok(ended);
            }
        }
        Ok(Ended::Entered)
    }

    /// Take `name`: a `..` climbs the cursor, an entry is opened beneath it.
    /// `Break` where the walk ends there.
    fn take(&mut self, name: Name) -> io::Result<ControlFlow<Ended>> {
        let Name::Entry(name, origin) = name else {
            self.cursor.up();
            return Ok(ControlFlow::Continue(()));
        };
        let last = self.names.is_empty();
        let entering = !last || self.mode == Mode::Enter;
        let sharing = if entering {
            Sharing::Directory
        } else {
            Sharing::Leaf
        };
        let child = match self.open(&name, origin, sharing) {
            Ok(child) => child,
            Err(e) => return self.stopped(name, e),
        };
        self.check(&child, &name)?;
        match child.step.kind {
            Kind::Symlink if self.links == Links::Refuse => return Err(loop_error()),
            Kind::Symlink if last && self.mode == Mode::NoFollow => {
                return Ok(ControlFlow::Break(Ended::Leaf(child)));
            }
            Kind::Symlink => {
                let (link, step, _) = child.held()?;
                let spliced = host::read_link(&link.file).and_then(|target| {
                    if last && origin == Origin::Spelled {
                        self.linked = Some(target.clone());
                    }
                    self.splice(&step.name, &target)
                });
                if let Err(e) = spliced {
                    return self.stopped(name, e);
                }
            }
            Kind::Dir if entering => {
                let (handle, step, _) = child.held()?;
                self.cursor.push(handle, step);
            }
            Kind::Dir => return Ok(ControlFlow::Break(Ended::Leaf(child))),
            _ if entering => return self.stopped(name, io::ErrorKind::NotADirectory.into()),
            _ => return Ok(ControlFlow::Break(Ended::Leaf(child))),
        }
        Ok(ControlFlow::Continue(()))
    }

    /// The entry `name` of the directory on top of the cursor, opened.  Where
    /// it is not there, a making walk makes it a directory first, if the
    /// spelling named it: `mkdir` under the cursor's handle, a name taken
    /// meanwhile tolerated, then the step onto what stands there.
    fn open(&self, name: &EntryName, origin: Origin, sharing: Sharing) -> io::Result<Child> {
        let top = self.cursor.top();
        match step_into(top, name, sharing) {
            Err(e)
                if self.missing == OnMissing::Make
                    && origin == Origin::Spelled
                    && e.kind() == io::ErrorKind::NotFound =>
            {
                match host::mkdir(&top.file, name, Visibility::Private) {
                    Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e),
                    _ => {}
                }
                step_into(top, name, sharing)
            }
            stepped => stepped,
        }
    }

    /// The object the walk ended as, as a [`Located`].
    fn conclude(self, ended: Ended) -> io::Result<Located> {
        match ended {
            Ended::Leaf(child) => Ok(self.leaf(child)),
            Ended::Entered => self.entered(),
            Ended::Absent(tail) => Ok(self.absent(tail)),
        }
    }

    /// The walk ended on `child`, the last name's object.
    fn leaf(self, child: Child) -> Located {
        let trace = self.cursor.trace(Some(child.step.clone()), Vec::new());
        let dir = self.cursor.into_top();
        let leaf = match child.handle {
            Some(file) if child.stat.kind == Kind::Dir => Leaf::Dir {
                node: Arc::new(Node {
                    held: Held {
                        file: Arc::new(file),
                        info: child.info,
                    },
                    above: Some(dir),
                }),
                stat: child.stat,
            },
            Some(file) => Leaf::Held {
                dir: Arc::clone(&dir.held.file),
                file,
                stat: child.stat,
            },
            None => Leaf::Stated {
                dir: Arc::clone(&dir.held.file),
                stat: child.stat,
            },
        };
        Located {
            trace,
            leaf,
            linked: self.linked,
            _a: PhantomData,
        }
    }

    /// The names are spent: the directory on top of the cursor is the object,
    /// held as entered, as the node it is.
    fn entered(self) -> io::Result<Located> {
        let stat = host::seen(&self.cursor.top().file)?.stat;
        let trace = self.cursor.trace(None, Vec::new());
        Ok(Located {
            trace,
            leaf: Leaf::Dir {
                node: self.cursor.into_top(),
                stat,
            },
            linked: self.linked,
            _a: PhantomData,
        })
    }

    /// `name` could not be entered, for `error`.  The lenient walks end their
    /// steps at `NotFound`, `NotADirectory` and `PermissionDenied` and fold
    /// the names left onto it, lexically, as the tail: a `..` takes the name
    /// before it.  One that takes `name` itself has folded the stop away, and
    /// the walk resumes from the cursor with the names after it, as though
    /// those folded had never been spelled: `Continue`.  The strict ones fail,
    /// but for an absent last name, which is a create target.  Nothing else
    /// differs between the two.
    fn stopped(&mut self, name: EntryName, error: io::Error) -> io::Result<ControlFlow<Ended>> {
        use io::ErrorKind::{NotADirectory, NotFound, PermissionDenied};
        let last = self.names.is_empty();
        let tail = match (self.missing, error.kind()) {
            (OnMissing::Stop, NotFound | NotADirectory | PermissionDenied) => {
                let mut tail = vec![name];
                while let Some(name) = self.names.pop_front() {
                    match name {
                        Name::Entry(name, _) => tail.push(name),
                        Name::Parent => {
                            tail.pop();
                            if tail.is_empty() {
                                return Ok(ControlFlow::Continue(()));
                            }
                        }
                    }
                }
                tail
            }
            (OnMissing::Fail, NotFound) if last && self.mode != Mode::Enter => vec![name],
            _ => return Err(self.unreached(error)),
        };
        Ok(ControlFlow::Break(Ended::Absent(tail)))
    }

    /// `error`, a refusal of the walk.  A `NotFound` met after a link was
    /// followed at the spelling's last name is that link's target not being
    /// there: it carries the target, which
    /// [`dangling`](super::super::error::dangling) reads.
    fn unreached(&self, error: io::Error) -> io::Error {
        match &self.linked {
            Some(target) if error.kind() == io::ErrorKind::NotFound => {
                dangling_error(error, target.clone())
            }
            _ => error,
        }
    }

    /// The walk ended at `tail`, names past the directory on top of the
    /// cursor that are not there: a create target in it, or the names a
    /// lenient walk could not enter.
    fn absent(self, tail: Vec<EntryName>) -> Located {
        let trace = self.cursor.trace(None, tail);
        Located {
            trace,
            leaf: Leaf::Absent {
                dir: self.cursor.into_top(),
            },
            linked: self.linked,
            _a: PhantomData,
        }
    }
}

/// The engine behind every operation: `rp` walked from its root, `mode` deciding
/// the last name, `missing` whether a name that cannot be entered fails or
/// ends the steps, `links` whether a link is spliced or refused, and every
/// object put to `vet`.  A directory spelling raises `mode` to `Enter`.
pub(in crate::path::walk) fn finish(
    rp: &Path,
    mode: Mode,
    missing: OnMissing,
    links: Links,
    vet: impl FnMut(&Vetted<'_>) -> bool,
) -> io::Result<Located> {
    start(rp, mode, missing, links, vet)?.run()
}

/// `rp` walked from its root, as [`finish`] does, before any name is taken.
fn start<V: FnMut(&Vetted<'_>) -> bool>(
    rp: &Path,
    mode: Mode,
    missing: OnMissing,
    links: Links,
    vet: V,
) -> io::Result<Walker<V>> {
    rooted(rp)?;
    let Spelling { root, names } = split(rp)?;
    let root = root.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
    let mode = if names_a_dir(rp) { Mode::Enter } else { mode };
    let mut walker = Walker::new(Cursor::root(root)?, names, mode, missing, links, vet);
    walker.vet_root()?;
    Ok(walker)
}

/// An access to an object or a create target.
///
/// An absent intermediate directory is an error.  A spelling that names a directory (`d/`, `d/..`,
/// `/`, or a link target so spelled) yields the directory itself as the leaf,
/// entered; there is no spelling `walk` refuses for its shape.
///
/// # Errors
/// A missing or untraversable intermediate directory, a name ending in a
/// separator that is no directory, a link cycle past the splice budget, or
/// `InvalidInput` for a name with no root to walk it from, whether `rp`
/// itself or a link's target spliced into it.
pub fn walk(rp: &LexicalPath, mode: LeafMode) -> io::Result<Located> {
    finish(
        rp.as_path(),
        mode.into(),
        OnMissing::Fail,
        Links::Splice,
        |_| true,
    )
}

/// A directory entered — `cd`, a listing root, `within [dir:]` — whatever the
/// spelling's shape: [`walk`] under `Enter`, viewed as a [`Dir`].  A link at
/// the end is followed.
///
/// # Errors
/// [`walk`]'s; `NotADirectory` for anything but a directory.
pub fn walk_dir(rp: &LexicalPath) -> io::Result<Dir> {
    dir_of(rp, OnMissing::Fail)
}

/// [`walk_dir`], making each absent name of the spelling owner-only (0700) and
/// stepping onto it.  A name a link's target spliced in is never made: a
/// dangling link fails the walk, as it fails `mkdir -p`, with the `NotFound`
/// that [`dangling`](super::super::error::dangling) reads.
///
/// # Errors
/// [`walk_dir`]'s; a make's.
pub(crate) fn walk_dir_made(rp: &LexicalPath) -> io::Result<Dir> {
    dir_of(rp, OnMissing::Make)
}

/// `rp` entered as a directory, a name that cannot be entered treated as
/// `missing` says.
fn dir_of(rp: &LexicalPath, missing: OnMissing) -> io::Result<Dir> {
    finish(rp.as_path(), Mode::Enter, missing, Links::Splice, |_| true)?
        .into_dir()
        .map_err(|_| io::ErrorKind::NotADirectory.into())
}

/// Whether the absolute spelling `path` reaches an object, a final link
/// followed: its walk.  A relative one reaches nothing: no cwd is asked.
pub fn exists(path: &str) -> bool {
    kind_of(path).is_some()
}

/// Whether the absolute spelling `path` reaches a directory, a link to one
/// seen through.
pub fn is_dir(path: &str) -> bool {
    kind_of(path) == Some(Kind::Dir)
}

/// What the absolute spelling `path` reaches, a final link followed: its
/// walk.  `None` where nothing is there, and for a relative one, which reaches
/// nothing: no cwd is asked.
pub fn kind_of(path: &str) -> Option<Kind> {
    walk(&spelled(path)?, LeafMode::Follow).ok()?.trace().kind()
}

/// `path` as a spelling, where absolute: the one lift of a fixed string.  The
/// walk then resolves it, so the lift opens nothing.
pub(crate) fn spelled(path: &str) -> Option<LexicalPath> {
    LexicalPath::absolute(site!(Silent::StrSpelling, Path::new(path)))
}

/// A grant spelling's transcript: as far as the disk goes, the rest lexical,
/// and where the rest folds away the name the disk refused, the disk again.
///
/// # Errors
/// What ends no steps: a splice budget exhausted, `InvalidInput` for a name
/// or a target with no root, any other I/O failure.
pub fn walk_lenient(rp: &LexicalPath, mode: LeafMode) -> io::Result<Trace> {
    finish(
        rp.as_path(),
        mode.into(),
        OnMissing::Stop,
        Links::Splice,
        |_| true,
    )
    .map(|located| located.trace)
}

/// A grant exclusion: the spelling walked leniently without following its
/// last component, and where that is a link, the target read through the
/// link's own handle and walked, following links, from the directory the
/// walk left the cursor in; the spelling is never walked a second time.  Each
/// leaf is held by the walk's own handle, the link before its target is
/// read, so what the link named is what the held link names.
///
/// # Errors
/// What ends no steps, as [`walk_lenient`]'s.
pub(crate) fn walk_excluded(rp: &LexicalPath) -> io::Result<Excluded> {
    let mut walker = start(
        rp.as_path(),
        Mode::NoFollow,
        OnMissing::Stop,
        Links::Splice,
        |_| true,
    )?;
    match walker.advance()? {
        Ended::Leaf(child) if child.step.kind == Kind::Symlink => {
            let (name, named) = (
                child.step.name.clone(),
                walker.cursor.trace(Some(child.step.clone()), Vec::new()),
            );
            let (link, ..) = child.held()?;
            walker.mode = Mode::Follow;
            #[cfg(test)]
            tests::around_link_read(tests::Around::Before);
            let read = host::read_link(&link.file);
            #[cfg(test)]
            tests::around_link_read(tests::Around::After);
            let target = match read.and_then(|target| walker.splice(&name, &target)) {
                Ok(()) => walker.run()?,
                Err(e) => match walker.stopped(name, e)? {
                    ControlFlow::Continue(()) => walker.run()?,
                    ControlFlow::Break(ended) => walker.conclude(ended)?,
                },
            };
            Ok(Excluded::new(
                Hold::new(
                    named,
                    #[cfg(unix)]
                    Some(link.file),
                ),
                Some(target.into_hold()),
            ))
        }
        ended => Ok(Excluded::new(walker.conclude(ended)?.into_hold(), None)),
    }
}

/// [`walk`], with every object it passes put to `vet` before the walk relies
/// on it: the root, each directory as it is entered, each link before it is
/// spliced, and the leaf where it lands.  An absolute splice restarts from the
/// root, so `vet` sees the root again.
///
/// # Errors
/// [`walk`]'s; `PermissionDenied` when `vet` refuses an object.
#[cfg(unix)]
pub(crate) fn walk_with(
    rp: &LexicalPath,
    mode: LeafMode,
    vet: impl FnMut(&Vetted<'_>) -> bool,
) -> io::Result<Located> {
    finish(
        rp.as_path(),
        mode.into(),
        OnMissing::Fail,
        Links::Splice,
        vet,
    )
}
