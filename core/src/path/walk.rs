//! The one resolver: a spelling walked to the object it names, and the
//! transcript of that walk, which is what a judgment reads.
//!
//! A name is never authority.  [`walk`] descends from the root one component
//! at a time through directory handles, never letting the kernel follow a
//! link, splicing each link it meets into the names still to take; every
//! object it opened is a [`Step`] of the [`Trace`], [passed](Trace::passed), or
//! an ancestor of one passed, and the position it lands on ([`Trace::real`]) is
//! a memo of the steps.  Every operation a [`Located`] offers is relative to
//! the handle of its directory, so the object judged is the object touched.
//!
//! What the walk returns is [`Unjudged`]: it offers its trace and nothing more.
//! Bytes, a stat, a listing come out of a handle only once someone vouched for
//! it ([`Authority`]): the grant stack, by `GrantStack::judge`, or the OS
//! sandbox side, at a position it is installing rules at.
//!
//! This is the one file that opens a model-named object, so the typed sites
//! (`path/site.rs`) can be read off it and its host modules: [`open_discard`]
//! is the sole exception, a device with no object to locate.
//!
//! # The host seam
//!
//! The engine never asks the OS directly: it asks `host`, the module of this
//! host's `cfg` (`linux.rs`, `macos.rs`, `windows.rs`).  Shared by the Unix
//! hosts, `unix.rs` holds what reads alike on each, which a Unix host
//! re-exports.  A host module provides, as `pub(super)` items:
//!
//! - `step`, the one primitive that opens a child of a held directory, with
//!   `Child`; `DirInfo`, what the host records of a directory as it opens it
//!   (Windows: its normalized path and volume, which `step` checks the child
//!   against; Unix: nothing), kept beside the handle in a `Held`;
//! - `open_root`, `seen`, `read_link`, `derive`, `list` and its
//!   `Lister`, which name the identity, kind and stat of an object, a link's
//!   target through its handle, a data handle derived from a held leaf, and a
//!   listing;
//! - `create`, `create_sibling`, `mkdir`, `rename_over`, `discard`,
//!   `remove`, `rename` and `open_discard`, which write, `may_write`, which
//!   asks whether the user may write, and on Unix `sync_dir` and
//!   `copy_mode`, which Windows has no use for;
//! - on Unix, `seen_at`, which stats a listed entry by name, and `impl
//!   From<&rustix::fs::Stat> for ObjectId`, the identity a by-name `stat`
//!   reports, which `carriers` compares with [`ObjectId::of`];
//! - on Windows, `reopen_shared` and `is_local`, which the `Installing` methods
//!   of the same names ask.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
use std::ops::ControlFlow;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;
use strum::{IntoStaticStr, VariantArray};

use super::lex::{is_absolute, is_unparsed_head, names_a_dir};
use super::site::Silent;
use super::{LexicalPath, PathRules, shown};
use crate::label;
use crate::site::site;
// Inverted edge, deliberate: `Verdict`'s field is private to its module, which `pub(in ..)` cannot name from a sibling.
use crate::capability::{FsOp, Verdict};
// Inverted edge, deliberate: the twin of `Verdict`, its field private to `sandbox.rs`.
#[cfg(any(target_os = "linux", windows))]
use crate::sandbox::InstallToken;

#[cfg(unix)]
mod unix;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
use linux as host;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
use macos as host;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as host;

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

#[cfg(any(test, feature = "test-util"))]
thread_local! {
    /// How many opens the walks on this thread have made — the root each
    /// starts from and every component [`step_into`] takes — which a test
    /// reads to show what a walk cost.
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

/// `SYMLOOP_MAX`'s customary value: a name still splicing past this is a cycle.
const MAX_HOPS: usize = 40;

/// `ELOOP`'s wording; `io::ErrorKind::FilesystemLoop` is not yet stable.
const LOOP_MESSAGE: &str = "too many levels of symbolic links";

/// A walk's refusal of a link, or of a name still splicing past the budget.
#[derive(Debug)]
struct Looped;

impl fmt::Display for Looped {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(LOOP_MESSAGE)
    }
}

impl std::error::Error for Looped {}

fn loop_error() -> io::Error {
    io::Error::other(Looped)
}

/// Whether `e` is the walk's `ELOOP`: a link met where none is allowed, or a
/// splice budget spent.
pub(crate) fn is_loop(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Looped>)
}

/// A walk's refusal where the link at the spelling's last name leads nowhere:
/// the `NotFound` met past it, and the target the link named.  Shows as that
/// `NotFound`.
#[derive(Debug)]
struct Dangling {
    target: PathBuf,
    error: io::Error,
}

impl fmt::Display for Dangling {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(f)
    }
}

impl std::error::Error for Dangling {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// The target a link at the spelling's last name named, as the link spelled
/// it, where the walk could not reach it: read off its `NotFound`, as
/// [`is_loop`] reads `ELOOP`.  `None` for any other error.
pub(crate) fn dangling(e: &io::Error) -> Option<&Path> {
    e.get_ref()?
        .downcast_ref::<Dangling>()
        .map(|d| d.target.as_path())
}

/// A staged write's refusal where the sibling's create was refused: the
/// directory's refusal, not the leaf's.  Shows as the words that say so.
#[derive(Debug)]
struct DirectoryRefused;

impl fmt::Display for DirectoryRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("its directory is not writable")
    }
}

impl std::error::Error for DirectoryRefused {}

/// Whether `e` is a staged write's [`DirectoryRefused`]: read off its
/// `PermissionDenied`, as [`is_loop`] reads `ELOOP`.  `false` for any other
/// error, including the leaf's own refusal.
pub(crate) fn directory_refused(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<DirectoryRefused>)
}

/// The error [`Located::stage`] returns where the sibling's create is refused,
/// built as it builds it, for the tests that render it where no directory can
/// be made unwritable (root writes every directory).
#[cfg(test)]
pub(crate) fn directory_refusal_for_test() -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, DirectoryRefused)
}

/// What every name of an object shares and no other object has while the
/// object exists.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ObjectId {
    #[cfg(unix)]
    dev: u64,
    #[cfg(unix)]
    ino: u64,
    #[cfg(windows)]
    volume: u64,
    #[cfg(windows)]
    id: u128,
}

impl ObjectId {
    /// Whether both objects are on one device or volume.
    pub(crate) fn same_volume(self, other: Self) -> bool {
        #[cfg(unix)]
        return self.dev == other.dev;
        #[cfg(windows)]
        return self.volume == other.volume;
    }

    /// The identity of the object `f` holds, read from the handle.
    pub(crate) fn of(f: &File) -> io::Result<Self> {
        host::seen(f).map(|seen| seen.id)
    }
}

/// What the object is.  One of these and no other, so an enum rather than a
/// row of bools that can contradict one another.
///
/// Its tag is the `type` ral's `list-dir` and `file-info` give it.
#[derive(
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Debug,
    IntoStaticStr,
    VariantArray,
    Serialize,
    Deserialize,
)]
#[strum(serialize_all = "kebab-case")]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other,
}

label!(typed Kind);

/// What a `stat` of the object found, when it exists.
///
/// `kind` is `Symlink` only for a [`LeafMode::NoFollow`] walk; a following
/// walk has already spliced the link, so what it stats is the target.  A
/// timestamp the filesystem does not record is `None`, not an epoch: the two
/// are different answers, and only the caller knows how to say so.
#[derive(Clone, Debug)]
pub struct Stat {
    pub kind: Kind,
    pub len: u64,
    pub readonly: bool,
    pub mtime: Option<SystemTime>,
    pub atime: Option<SystemTime>,
    pub btime: Option<SystemTime>,
    #[cfg(unix)]
    pub mode: u32,
    /// The reparse tag, where the object is a reparse point.
    #[cfg(windows)]
    pub reparse_tag: Option<u32>,
    #[cfg(windows)]
    pub hidden: bool,
}

/// What a host reads of an object from a handle on it, or from its name in a
/// directory: the stat and the identity, one lookup.
pub(crate) struct Seen {
    pub stat: Stat,
    pub id: ObjectId,
}

/// Why a name is not an entry name.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Rule {
    Empty,
    Dots,
    Separator,
    Stream,
}

/// A name refused as an entry name by [`EntryName::parse`].  Shows as the
/// sentence naming the rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NotAnEntryName {
    name: OsString,
    rule: Rule,
}

impl fmt::Display for NotAnEntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = shown(&self.name);
        match self.rule {
            Rule::Empty => f.write_str("an entry name cannot be empty"),
            Rule::Dots => write!(
                f,
                "{name} is not an entry name: it is a directory's own or its parent's"
            ),
            Rule::Separator => write!(
                f,
                "{name} is not an entry name: it holds a path separator, so it names a path"
            ),
            Rule::Stream => write!(
                f,
                "{name} is not an entry name: it holds `:`, which names a stream of a file, not an entry"
            ),
        }
    }
}

impl std::error::Error for NotAnEntryName {}

impl From<NotAnEntryName> for io::Error {
    fn from(refused: NotAnEntryName) -> Self {
        Self::new(io::ErrorKind::InvalidInput, refused)
    }
}

/// One name of a directory entry.
///
/// A single normal component: non-empty, not `.` or `..`, holding no
/// separator, and on Windows no `:` (a stream is reached through its file; no
/// listing shows one and ral has no way to name one).  [`parse`](Self::parse) is
/// the one constructor and checks that; the walk's parse of a spelling, a
/// listing, [`fresh`](Self::fresh) and `Deserialize` all go through it, so
/// [`Dir::open_dir`] cannot be handed `../x` or `a/b`.  Crosses the wire as
/// serde's lossless `OsString` form, never through `Path`'s UTF-8-only one.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize)]
pub struct EntryName(OsString);

impl EntryName {
    /// `name` as an entry name.
    ///
    /// # Errors
    /// [`NotAnEntryName`] saying which rule `name` breaks.
    pub fn parse(name: &OsStr) -> Result<Self, NotAnEntryName> {
        let bytes = name.as_encoded_bytes();
        let rule = if bytes.is_empty() {
            Some(Rule::Empty)
        } else if bytes == b"." || bytes == b".." {
            Some(Rule::Dots)
        } else if bytes
            .iter()
            .any(|&b| PathRules::HOST.is_separator(char::from(b)))
        {
            Some(Rule::Separator)
        } else {
            (cfg!(windows) && bytes.contains(&b':')).then_some(Rule::Stream)
        };
        rule.map_or_else(
            || Ok(Self(name.to_owned())),
            |rule| {
                Err(NotAnEntryName {
                    name: name.to_owned(),
                    rule,
                })
            },
        )
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }

    /// `prefix`, sixteen random alphanumerics, `suffix`: the text of a name
    /// ral invents.  The one place that rule is written; it is not yet
    /// judged to be an entry name.
    pub(crate) fn fresh_text(prefix: &str, suffix: &str) -> String {
        let mut text = String::from(prefix);
        text.extend((0..16).map(|_| fastrand::alphanumeric()));
        text.push_str(suffix);
        text
    }

    /// `prefix` plus sixteen random alphanumerics: a name ral invents.
    /// `prefix` is parsed, so a caller cannot smuggle a separator in it.
    ///
    /// # Errors
    /// [`NotAnEntryName`] where the name made is not one.
    pub(crate) fn fresh(prefix: &str) -> Result<Self, NotAnEntryName> {
        Self::parse(OsStr::new(&Self::fresh_text(prefix, "")))
    }
}

impl<'de> Deserialize<'de> for EntryName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&OsString::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// A parsed root spelling, what [`rooted`] accepts: `/`; `C:\`,
/// `\\server\share\` and their verbatim forms.  Crosses the wire as an
/// `OsString`, checked on the way in.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
pub struct RootName(PathBuf);

impl RootName {
    fn parse(raw: &OsStr) -> io::Result<Self> {
        let path = lexical(raw);
        rooted(path)?;
        match split(path)? {
            Spelling {
                root: Some(root),
                names,
            } if names.is_empty() => Ok(root),
            _ => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is more than a root", shown(path)),
            )),
        }
    }
}

impl Serialize for RootName {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.as_os_str().serialize(s)
    }
}

impl<'de> Deserialize<'de> for RootName {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&OsString::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// `raw` as a path, read lexically: no disk, no fold.
fn lexical(raw: &OsStr) -> &Path {
    site!(Silent::LexicalLift, Path::new(raw))
}

/// Where an entry name came from: the spelling the walk was asked for, or the
/// target of a link it met.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Spelled,
    Spliced,
}

/// One component of a spelling past its root.  An entry carries its
/// [`Origin`], which a making walk reads: a name a link's target spliced in is
/// never made.
enum Name {
    Entry(EntryName, Origin),
    Parent,
}

impl Name {
    /// This name as a link's target put it in the spelling.
    fn spliced(self) -> Self {
        match self {
            Self::Entry(name, _) => Self::Entry(name, Origin::Spliced),
            Self::Parent => Self::Parent,
        }
    }
}

/// A spelling split into its root, where it has one, and its names.
struct Spelling {
    root: Option<RootName>,
    names: VecDeque<Name>,
}

/// `path` as its root and the names past it, `.` dropped.
fn split(path: &Path) -> io::Result<Spelling> {
    let mut root = PathBuf::new();
    let mut names = VecDeque::new();
    for component in site!(Silent::SpellingSplit, path => components()) {
        match component {
            Component::Prefix(_) | Component::RootDir => root.push(component),
            Component::CurDir => {}
            Component::ParentDir => names.push_back(Name::Parent),
            Component::Normal(name) => {
                names.push_back(Name::Entry(EntryName::parse(name)?, Origin::Spelled));
            }
        }
    }
    Ok(Spelling {
        root: (!root.as_os_str().is_empty()).then_some(RootName(root)),
        names,
    })
}

/// Whether a walk can start `path` from a root: every name it descends, the
/// one it was given and each a splice restarts it on, so a link's target
/// can no more lead it off the path than the name could.
///
/// # Errors
/// `InvalidInput` for a relative `path`, a Windows link target such as
/// `C:x` that is relative to a drive's current directory included, or for
/// one whose head `std::path` cannot parse ([`is_unparsed_head`]), which
/// the walk would descend from the current drive's root.
fn rooted(path: &Path) -> io::Result<()> {
    if !is_absolute(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "a walk starts from the root, and a relative name has none",
        ));
    }
    if is_unparsed_head(path) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no well-formed share, drive or namespace opens this path, so there is no root to walk it from",
        ));
    }
    Ok(())
}

/// One component the walk opened: its name as the filesystem stores it, its
/// identity and its kind, all read from the handle the walk opened on it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct Step {
    name: EntryName,
    id: ObjectId,
    kind: Kind,
}

impl Step {
    pub(super) fn name(&self) -> &EntryName {
        &self.name
    }

    pub(super) fn id(&self) -> ObjectId {
        self.id
    }

    pub(crate) fn kind(&self) -> Kind {
        self.kind
    }
}

/// A position: an absolute path with no link, `.` or `..` in it.
///
/// Each name is spelled as the filesystem stores it.  Made by the walk
/// ([`Trace::real`]),
/// and read off the wire by a hand-written `Deserialize` that applies the
/// lexical check (absolute, parsed root, no `.` or `..`) through a private
/// constructor.  No `TryFrom` and no `From`: nothing mints a position from a
/// string.  On the wire it is its `OsString`, serde's lossless form, so a
/// position that is not Unicode crosses whole.  `Eq` and `Ord` compare
/// stored components on Windows and components elsewhere.
#[derive(Clone, Debug)]
pub struct RealPath(PathBuf);

impl RealPath {
    /// `root` and `names`, joined.
    fn assemble<'a>(root: &RootName, names: impl IntoIterator<Item = &'a EntryName>) -> Self {
        let mut path = root.0.clone();
        path.extend(names.into_iter().map(EntryName::as_os_str));
        Self(path)
    }

    /// `raw` as a position, lexically checked: absolute, a parsed root, no
    /// `.` or `..`, and spelled as the walk spells one.
    fn parse(raw: &OsStr) -> io::Result<Self> {
        let path = lexical(raw);
        rooted(path)?;
        let Spelling { root, names } = split(path)?;
        let root = root.ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let names = names
            .into_iter()
            .map(|name| match name {
                Name::Entry(name, _) => Ok(name),
                Name::Parent => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("{} holds `..`, which no position does", shown(path)),
                )),
            })
            .collect::<io::Result<Vec<_>>>()?;
        let real = Self::assemble(&root, &names);
        if real.0.as_os_str() != raw {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "{} is not spelled as the walk spells a position: no `.`, no doubled or trailing separator",
                    shown(path)
                ),
            ));
        }
        Ok(real)
    }

    /// This position's entry `name`.
    fn child(&self, name: &EntryName) -> Self {
        Self(self.0.join(name.as_os_str()))
    }

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
        Ok(located.trace.real)
    }

    /// What is at this position now, reached through no link: the walk with
    /// [`Links::Refuse`], held for the OS sandbox side, which proves it is
    /// that side by the token.  `None` where nothing is there; `Err` with
    /// `ELOOP`'s wording ([`is_loop`]) where a link is, since a redirected
    /// position refuses the launch.  A file or a directory alike.
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
            &self.0,
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
        match finish(&self.0, Mode::Enter, OnMissing::Fail, Links::Refuse, |_| {
            true
        }) {
            Ok(located) => Ok(located.into_dir().ok().map(Dir::marked)),
            Err(e) if matches!(e.kind(), NotFound | NotADirectory) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// The position `path` spells, which no walk made: for the tests that need
    /// one on a drive the host does not have (the Windows ceiling tests with
    /// a `D:\` position), where no walk can reach one.
    ///
    /// # Panics
    /// If `path` is not a position's spelling.
    #[cfg(test)]
    pub(crate) fn unwalked_for_test(path: &str) -> Self {
        Self::parse(path.as_ref()).expect("a test spells a position")
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// The final component, which a command name can speak of: `None` for a
    /// root, and for a name that is not Unicode, which no command name is.
    /// Never a lossy decoding, which would make such a file the one named
    /// with U+FFFD in its place.
    pub(crate) fn name(&self) -> Option<&str> {
        site!(Silent::RealName, self.0.file_name()? => to_str())
    }

    /// The position above this one, `None` at a root.
    #[cfg(any(target_os = "macos", windows))]
    pub(crate) fn parent(&self) -> Option<Self> {
        site!(Silent::RealParent, &self.0 => parent()).map(|p| Self(p.to_path_buf()))
    }

    /// This position's proper ancestors at or beneath `prefix`, nearest
    /// first: what the Seatbelt profile pins.
    pub(crate) fn ancestors_within<'a>(
        &'a self,
        prefix: &'a Self,
    ) -> impl Iterator<Item = Self> + 'a {
        site!(Silent::RealAncestors, &self.0 => ancestors())
            .skip(1)
            .map(|a| Self(a.to_path_buf()))
            .take_while(|a| prefix.contains(a))
    }

    /// Whether this position is the root of its filesystem: `/`, `C:\`.
    pub fn is_root(&self) -> bool {
        site!(Silent::RealRoot, &self.0 => parent()).is_none()
    }

    /// `/`, the position every path lies beneath.  No walk makes it on
    /// Windows, where a root is one drive's, so no Windows code names it.
    #[cfg(unix)]
    pub(crate) fn universal() -> Self {
        Self(PathBuf::from("/"))
    }

    /// Whether this position holds every path: `/`, where `/` is a root; a
    /// drive root holds its own drive alone.
    pub(crate) fn is_universal(&self) -> bool {
        PathRules::HOST == PathRules::Posix && self.is_root()
    }
}

impl Ord for RealPath {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match PathRules::HOST {
            PathRules::Windows => {
                let identity =
                    |p: &Self| super::identity::windows_stored_components(p.0.as_os_str());
                identity(self).cmp(&identity(other))
            }
            PathRules::Posix => self.0.cmp(&other.0),
        }
    }
}

impl PartialOrd for RealPath {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl PartialEq for RealPath {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for RealPath {}

impl fmt::Display for RealPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        shown(&self.0).fmt(f)
    }
}

impl Serialize for RealPath {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.as_os_str().serialize(s)
    }
}

impl<'de> Deserialize<'de> for RealPath {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::parse(&OsString::deserialize(d)?).map_err(D::Error::custom)
    }
}

/// The transcript of a walk.
///
/// `root` is the parsed root spelling; `root_id`
/// the root's identity; `steps` one per component opened, in order; `tail`
/// the names past the last step, from the first the walk could not enter,
/// lexically folded: a `..` in it takes the name before it, and one that
/// takes the first resumes the walk from the last step with the names after
/// it, as though the names folded away had never been spelled; so the tail
/// is empty exactly where the walk reached an object.  `real` is `root`
/// plus the steps' names plus `tail`, computed by the one constructor: a memo
/// of the steps, never a separate observation.  `passed` is, in order, the
/// position of each link whose target the walk read and of each directory a
/// `..` climbed out of: what else a lookup of the spelling looks up.
/// `Deserialize` by hand: each field checks itself, the shape is checked —
/// every step but the last has kind `Dir`, and a non-empty tail follows a
/// `Dir` step or the root — and `real` is recomputed, so a trace read off the
/// wire is one the walk could have made.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize)]
pub struct Trace {
    root: RootName,
    root_id: ObjectId,
    steps: Vec<Step>,
    tail: Vec<EntryName>,
    real: RealPath,
    passed: Vec<RealPath>,
}

impl Trace {
    fn new(
        root: RootName,
        root_id: ObjectId,
        steps: Vec<Step>,
        tail: Vec<EntryName>,
        passed: Vec<RealPath>,
    ) -> Self {
        let real = RealPath::assemble(&root, steps.iter().map(Step::name).chain(&tail));
        Self {
            root,
            root_id,
            steps,
            tail,
            real,
            passed,
        }
    }

    /// This trace, a directory's, without its last step: the trace of the
    /// directory above it; a root's is itself.
    fn above(&self) -> Self {
        Self::new(
            self.root.clone(),
            self.root_id,
            self.steps
                .split_last()
                .map_or_else(Vec::new, |(_, above)| above.to_vec()),
            Vec::new(),
            self.passed.clone(),
        )
    }

    /// This trace, a directory's, with `more` steps below it and `tail` past.
    fn extended(&self, more: impl IntoIterator<Item = Step>, tail: Vec<EntryName>) -> Self {
        let steps = self.steps.iter().cloned().chain(more).collect();
        Self::new(
            self.root.clone(),
            self.root_id,
            steps,
            tail,
            self.passed.clone(),
        )
    }

    pub fn real(&self) -> &RealPath {
        &self.real
    }

    /// What `identity.rs` reads, nothing outside `path/`.
    pub(super) fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub(super) fn tail(&self) -> &[EntryName] {
        &self.tail
    }

    /// What the walk passed besides its steps, by position.
    pub(crate) fn passed(&self) -> &[RealPath] {
        &self.passed
    }

    pub(super) fn root_id(&self) -> ObjectId {
        self.root_id
    }

    /// The kind of the object at this position: the last step's where the
    /// tail is empty; `Dir` for the root; `None` for a missing tail.
    pub fn kind(&self) -> Option<Kind> {
        if !self.tail.is_empty() {
            return None;
        }
        Some(self.steps.last().map_or(Kind::Dir, Step::kind))
    }

    /// The identity of the object at this position, where the tail is empty.
    /// A present root has one: `C:\` and `D:\` are two objects.
    pub(crate) fn leaf_id(&self) -> Option<ObjectId> {
        self.tail.is_empty().then(|| self.last_id())
    }

    /// The identity of the last object the walk opened: the last step's,
    /// else the root's.
    pub(crate) fn last_id(&self) -> ObjectId {
        self.steps.last().map_or(self.root_id, Step::id)
    }
}

/// A trace as it crosses the wire, before its shape is checked.
#[derive(Deserialize)]
struct WireTrace {
    root: RootName,
    root_id: ObjectId,
    steps: Vec<Step>,
    tail: Vec<EntryName>,
    real: RealPath,
    passed: Vec<RealPath>,
}

impl<'de> Deserialize<'de> for Trace {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire = WireTrace::deserialize(d)?;
        let walkable = wire.steps.split_last().is_none_or(|(last, above)| {
            above.iter().all(|s| s.kind == Kind::Dir)
                && (wire.tail.is_empty() || last.kind == Kind::Dir)
        });
        if !walkable {
            return Err(D::Error::custom(
                "a trace descends through directories alone: a file or a link has nothing beneath it",
            ));
        }
        let trace = Self::new(wire.root, wire.root_id, wire.steps, wire.tail, wire.passed);
        if trace.real != wire.real {
            return Err(D::Error::custom(format!(
                "a trace of {} claims to be of {}",
                trace.real, wire.real
            )));
        }
        Ok(trace)
    }
}

/// An exclusion's hold on the last object its walk opened.
///
/// On Unix `handle` is the walk's own, `Some` while that object was found at its
/// position; `None` is the unheld state: after a `rehold` that found another
/// object there or none, after a `Deserialize` (the wire carries the trace
/// alone), or (macOS) a hold the object's permissions refused.  Windows holds
/// nothing.  Unheld is sound: the trace's positions and identities still
/// match, and a recycled identity over-denies.  `Serialize` as the trace;
/// `Deserialize` unheld; `PartialEq`, `Eq`, `Ord` by trace.
#[derive(Clone, Debug)]
pub struct Hold {
    trace: Trace,
    #[cfg(unix)]
    handle: Option<Arc<File>>,
}

impl Hold {
    pub(crate) fn trace(&self) -> &Trace {
        &self.trace
    }

    fn unheld(trace: Trace) -> Self {
        Self {
            trace,
            #[cfg(unix)]
            handle: None,
        }
    }

    /// Take hold of the object again on this host: walk `trace.real()`
    /// without following the leaf, and keep the handle where the last object
    /// the walk opened is the one recorded, by identity; else unheld.  The
    /// recorded trace stays as it was, and a walk error is the unheld state.
    #[cfg(unix)]
    pub(crate) fn rehold(&mut self) {
        self.handle = finish(
            self.trace.real().as_path(),
            Mode::NoFollow,
            OnMissing::Stop,
            Links::Splice,
            |_| true,
        )
        .ok()
        .filter(|found| found.trace.last_id() == self.trace.last_id())
        .and_then(|found| found.into_hold().handle);
    }

    /// Windows holds nothing, so nothing is taken again.
    #[cfg(windows)]
    #[expect(clippy::unused_self, reason = "Windows holds nothing")]
    pub(crate) fn rehold(&mut self) {}
}

impl PartialEq for Hold {
    fn eq(&self, other: &Self) -> bool {
        self.trace == other.trace
    }
}

impl Eq for Hold {}

impl PartialOrd for Hold {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for Hold {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.trace.cmp(&other.trace)
    }
}

impl Serialize for Hold {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.trace.serialize(s)
    }
}

impl<'de> Deserialize<'de> for Hold {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Trace::deserialize(d).map(Self::unheld)
    }
}

/// What an exclusion holds: the object at the name as named and, where that
/// was a link, what the link reached.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Excluded {
    named: Hold,
    target: Option<Hold>,
}

impl Excluded {
    pub(crate) fn named(&self) -> &Trace {
        self.named.trace()
    }

    pub(crate) fn target(&self) -> Option<&Trace> {
        self.target.as_ref().map(Hold::trace)
    }

    pub(crate) fn rehold(&mut self) {
        self.named.rehold();
        self.target.iter_mut().for_each(Hold::rehold);
    }

    /// The identity of each object held, the name's first, `None` where
    /// unheld.
    #[cfg(all(test, unix))]
    pub(crate) fn held_ids(&self) -> Vec<Option<ObjectId>> {
        let held = |hold: &Hold| hold.handle.as_deref().map(|f| ObjectId::of(f).unwrap());
        self.holds().map(held).collect()
    }

    /// The holds, the name's first.
    pub(super) fn holds(&self) -> impl Iterator<Item = &Hold> {
        std::iter::once(&self.named).chain(&self.target)
    }
}

/// What a grant spelling's walk makes, by the polarity of the rule that
/// names it: the transcript an allowance reads, or the holds an exclusion
/// keeps.
pub trait Walked: Sized {
    /// The spelling walked, as far as the disk goes.
    ///
    /// # Errors
    /// [`walk_lenient`]'s, or [`walk_excluded`]'s.
    fn walk(spelling: &LexicalPath) -> io::Result<Self>;

    /// Where an access to the spelling lands, for the refusals that judge it.
    fn followed(&self) -> &Trace;
}

impl Walked for Trace {
    fn walk(spelling: &LexicalPath) -> io::Result<Self> {
        walk_lenient(spelling, LeafMode::Follow)
    }

    fn followed(&self) -> &Trace {
        self
    }
}

impl Walked for Excluded {
    fn walk(spelling: &LexicalPath) -> io::Result<Self> {
        walk_excluded(spelling)
    }

    /// The link's target, else the object at the name.
    fn followed(&self) -> &Trace {
        self.target().unwrap_or_else(|| self.named())
    }
}

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
enum Links {
    Splice,
    Refuse,
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
/// [`InstallToken`], whose field is private to `sandbox.rs`.  What a holder
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

/// What the walk does at the last name: [`LeafMode`]'s two, and `Enter`,
/// where the spelling names a directory and the directory entered is the
/// object.  Never a caller's choice.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
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
enum OnMissing {
    Fail,
    Stop,
    Make,
}

/// How a child is opened where a host shares by handle: a directory step
/// holds the directory itself in place (Windows: `FILE_TRAVERSE` without
/// delete sharing), a leaf step blocks no one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sharing {
    Directory,
    Leaf,
}

/// What a data open is for: [`Located::open_data`]'s argument, and what each
/// of the walk's other data opens asks.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Append,
    Write,
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

    /// The same directory under another marker: this file's own step.
    fn marked<B: Authority>(self) -> Dir<B> {
        Dir {
            node: self.node,
            trace: self.trace,
            stat: self.stat,
            _a: PhantomData,
        }
    }
}

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

    /// The entry `name` as the object it is or, where nothing is there, the
    /// create target it would be: under [`LeafMode::Follow`], through a link
    /// at `name`, the target's, with [`Located::linked_to`] saying where the
    /// link led.  [`locate`](Self::locate) refuses the second.
    ///
    /// # Errors
    /// The walk's.  Where the link at `name` leads to a name beneath a
    /// directory that is not there, `NotFound`, which [`dangling`] reads.
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

/// A listing in progress: the sorted names, stated as they are yielded.
pub struct Listing<'a> {
    parent: &'a Trace,
    lister: host::Lister<'a>,
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

impl DirEntry<'_> {
    pub fn name(&self) -> &EntryName {
        &self.name
    }

    /// The position of this entry in the directory it was listed from, for
    /// the region that judges an unseen one: no object is behind it.
    pub(crate) fn real(&self) -> RealPath {
        self.parent.real.child(&self.name)
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

/// A listing's record of one entry: the parent's trace and one step made from
/// the listing's kind and identity.  Not a [`Trace`]: its step was stated by
/// name after the listing, not opened, and the type says so, so nothing that
/// wants a walk's evidence can be handed a listing's.
pub(crate) struct Listed<'a> {
    parent: &'a Trace,
    step: Step,
}

impl Listed<'_> {
    pub(crate) fn real(&self) -> RealPath {
        self.parent.real.child(&self.step.name)
    }

    pub(super) fn parent(&self) -> &Trace {
        self.parent
    }

    pub(super) fn step(&self) -> &Step {
        &self.step
    }
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

    /// The same object under another marker: this file's own step.
    fn marked<B: Authority>(self) -> Located<B> {
        Located {
            trace: self.trace,
            leaf: self.leaf,
            linked: self.linked,
            _a: PhantomData,
        }
    }

    /// What the walk stated of the leaf, for this file's own use: `None` when
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
    fn into_hold(self) -> Hold {
        Hold {
            #[cfg(unix)]
            handle: match self.leaf {
                Leaf::Held { file, .. } => Some(Arc::new(file)),
                Leaf::Dir { node, .. } | Leaf::Absent { dir: node } => {
                    Some(Arc::clone(&node.held.file))
                }
                Leaf::Stated { .. } => None,
            },
            trace: self.trace,
        }
    }

    /// The leaf's name: the last step's, or the one name of a create target.
    /// `None` for the root.
    fn leaf_name(&self) -> Option<&EntryName> {
        self.trace
            .tail
            .first()
            .or_else(|| self.trace.steps.last().map(Step::name))
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

impl<A: Peeks> Located<A> {
    /// `None` when the leaf is absent.  No syscall: taken at the walk.
    pub fn stat(&self) -> Option<&Stat> {
        self.observed()
    }

    pub(crate) fn kind(&self) -> Option<Kind> {
        self.stat().map(|stat| stat.kind)
    }

    /// Kind `File` and, on Unix, `mode & 0o111`; on Windows what
    /// `is_executable_file` decides today.  Asked of an admitted program,
    /// never before the judgment.
    pub(crate) fn is_executable(&self) -> bool {
        #[cfg(unix)]
        let executable = self.stat().is_some_and(|s| s.mode & 0o111 != 0);
        #[cfg(not(unix))]
        let executable = super::is_executable_file(self.real().as_path());
        self.kind() == Some(Kind::File) && executable
    }

    /// The link's target, read through the held leaf handle.
    ///
    /// # Errors
    /// `InvalidInput` where the leaf is not a link: a `Stated` leaf is never
    /// one, since a link is always held.  `NotFound` for an absent leaf.
    pub fn read_link(&self) -> io::Result<PathBuf> {
        match &self.leaf {
            Leaf::Held { file, stat, .. } if stat.kind == Kind::Symlink => host::read_link(file),
            Leaf::Absent { .. } => Err(io::ErrorKind::NotFound.into()),
            _ => Err(io::ErrorKind::InvalidInput.into()),
        }
    }

    /// `access(2)` for writing against the real uid/gid (Windows: an open for
    /// writing), never the stat's mode bits, which ignore ownership.  Follows,
    /// as `test -w` does.
    ///
    /// # Errors
    /// The host's refusal.
    pub(crate) fn may_write(&self) -> io::Result<()> {
        host::may_write(self.dir(), self.entry())
    }
}

impl Located<Installing> {
    /// The held leaf, for a bind source; `None` for an absent or stated leaf.
    #[cfg(target_os = "linux")]
    pub(crate) fn leaf_fd(&self) -> Option<BorrowedFd<'_>> {
        self.handle().map(AsFd::as_fd)
    }

    /// A handle on the leaf with `access`, sharing read, write and delete,
    /// derived from the held one: what DACL stamping needs beyond attributes.
    ///
    /// # Errors
    /// The reopen's; `NotFound` for a leaf that is not held.
    #[cfg(windows)]
    pub(crate) fn reopen(&self, access: u32) -> io::Result<File> {
        self.handle()
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
            .and_then(|file| host::reopen_shared(file, access))
    }

    /// Whether the leaf is on a local fixed volume.
    ///
    /// # Errors
    /// `NotFound` for a leaf that is not held.
    #[cfg(windows)]
    pub(crate) fn is_local(&self) -> io::Result<bool> {
        self.handle()
            .map(host::is_local)
            .ok_or_else(|| io::ErrorKind::NotFound.into())
    }
}

impl<A: Reads> Located<A> {
    /// A data handle on the leaf.  Held: derived from the held handle where
    /// the host can, else opened by name under `dir` and verified.  A special
    /// file (macOS): opened by name under `dir` and verified against the
    /// identity its step recorded, and to be a special file still.  Any
    /// other stated leaf: `PermissionDenied`, the walk's; nothing is opened.
    /// Absent: created exclusively with `visibility` for `Append` and `Write`,
    /// `NotFound` for `Read`.
    ///
    /// # Errors
    /// The open's; `IsADirectory` for a directory leaf, `InvalidInput` for a
    /// link, which has no data.
    fn open(&self, access: Access, visibility: Visibility) -> io::Result<File> {
        let named = || {
            self.leaf_name()
                .expect("a leaf that is not a directory has a name")
        };
        match (self.data()?, access) {
            (Data::Held(file, stat), _) => host::derive(file, self.dir(), named(), stat, access),
            #[cfg(target_os = "macos")]
            (Data::Special, _) => {
                host::open_special(self.dir(), named(), self.trace.last_id(), access)
            }
            #[cfg(not(target_os = "macos"))]
            (Data::Special, _) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the object could not be held, so nothing is opened through it",
            )),
            (Data::Absent, Access::Read) => Err(io::ErrorKind::NotFound.into()),
            (Data::Absent, _) => host::create(self.dir(), self.absent_name()?, access, visibility),
        }
    }

    /// # Errors
    /// The open's, including `NotFound`.
    pub fn read(&self) -> io::Result<File> {
        // A read makes nothing, so no visibility is asked.
        self.open(Access::Read, Visibility::Public)
    }
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
    /// leaf's refusal, the host's own; `PermissionDenied` carrying
    /// [`DirectoryRefused`] where the sibling's create is refused, which is the
    /// directory's; the create's otherwise, but for `AlreadyExists`, which is
    /// retried under another name; the mode's.
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
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        DirectoryRefused,
                    ));
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
        (self.stack.iter()).fold(self.trace.real.clone(), |at, (_, step)| {
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
        self.trace.passed.push(left);
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
/// last name, which a refusal past it reports as a [`Dangling`] one.
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
        self.cursor.trace.passed.push(passed);
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
            restarted.trace.passed = std::mem::take(&mut self.cursor.trace.passed);
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
    /// there: it carries the target, which [`dangling`] reads.
    fn unreached(&self, error: io::Error) -> io::Error {
        match &self.linked {
            Some(target) if error.kind() == io::ErrorKind::NotFound => io::Error::new(
                io::ErrorKind::NotFound,
                Dangling {
                    target: target.clone(),
                    error,
                },
            ),
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
fn finish(
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
/// that [`dangling`] reads.
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
///
/// Exec grants spell their two kinds of path key apart by a trailing slash,
/// and `guard::decode` checks that spelling against disk.
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
            Ok(Excluded {
                named: Hold {
                    trace: named,
                    #[cfg(unix)]
                    handle: Some(link.file),
                },
                target: Some(target.into_hold()),
            })
        }
        ended => Ok(Excluded {
            named: walker.conclude(ended)?.into_hold(),
            target: None,
        }),
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

/// The discard device, opened by its fixed name — `/dev/null`, `\\.\NUL` —
/// and never by a spelling: a spelling that folds to the device might open
/// whatever a link on the way leads to.  No argument, so there is nothing to
/// open but the device.
///
/// The one object this module opens without walking to it: exempt from the
/// walk and from the grant alike, since there is nothing to locate.
///
/// # Errors
/// The open's.
pub(crate) fn open_discard() -> io::Result<File> {
    host::open_discard()
}
