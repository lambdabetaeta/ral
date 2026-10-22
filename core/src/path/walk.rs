//! Stage 4 of path resolution: locating the *object* a resolved name denotes,
//! so the thing a guard authorises is the thing the kernel then operates on.
//!
//! A path-based guard judges a canonicalised string and the open that follows
//! re-walks the original one; whatever differs between those two walks — a
//! dangling link the canonicaliser could not follow, a directory swapped for a
//! symlink in between — is an object the guard never saw.  [`walk`] does one
//! walk: from the root, one component at a time through directory handles,
//! never letting the kernel follow a symlink, splicing each link it meets
//! into the remaining name itself.  Where it lands is [`Located::real`],
//! canonical by construction, and every operation a [`Located`] offers is
//! relative to the handle of its directory.
//!
//! What the walk returns is [`Unjudged`]: it offers its trace and nothing more.
//! Bytes, a stat, a listing come out of a handle only once someone vouched for
//! it ([`Authority`]): the grant stack, by `GrantStack::judge`, or the OS
//! sandbox side, at a position it is installing rules at.
//!
//! This is the one file that opens a model-named object, so the reviewed
//! syscall sites (`core/tests/syscall_sites.rs`) can be read off it and its
//! host modules: [`open_discard`] is the sole exception, a device with no
//! object to locate.
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
//! - `create`, `create_sibling`, `mkdir`, `rename_over`,
//!   `unlink`, `sync_dir`, `copy_mode`, `is_writable` and `open_discard`,
//!   which write;
//! - on Unix, `seen_at`, which stats a listed entry by name, and `impl
//!   From<&rustix::fs::Stat> for ObjectId`, the identity a by-name `stat`
//!   reports, which `carriers` compares with [`ObjectId::of`];
//! - on Windows, `reopen_shared` and `is_local`, which the `Installing` doors
//!   `reopen` and `is_local` ask.

use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io;
use std::marker::PhantomData;
#[cfg(target_os = "linux")]
use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Component, Path, PathBuf};
#[cfg(unix)]
use std::sync::Arc;
use std::time::SystemTime;
use strum::{IntoStaticStr, VariantArray};

use super::lex::{is_absolute, is_unparsed_head, names_a_dir};
use super::{LexicalPath, PathRules, shown};
use crate::label;
// Inverted edge, deliberate: `Verdict`'s field is private to its module, which `pub(in ..)` cannot name from a sibling.
use crate::capability::{FsOp, Verdict};
// Inverted edge, deliberate: the twin of `Verdict`, its field private to `sandbox.rs`.
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
mod tests;

#[cfg(any(test, feature = "test-util"))]
thread_local! {
    /// How many components [`step_into`] has opened on this thread, which a
    /// test reads to show what a walk cost.
    static STEPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many components have been opened on this thread.
#[cfg(any(test, feature = "test-util"))]
pub(crate) fn steps_taken() -> usize {
    STEPS.with(std::cell::Cell::get)
}

/// [`host::step`], counted under test.
fn step_into(parent: &Held, name: &EntryName, sharing: Sharing) -> io::Result<Child> {
    #[cfg(any(test, feature = "test-util"))]
    STEPS.with(|taken| taken.set(taken.get() + 1));
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
#[cfg_attr(
    not(any(target_os = "linux", windows)),
    expect(dead_code, reason = "no macOS renderer holds a position")
)]
pub(crate) fn is_loop(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<Looped>)
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
/// listing shows one and ral has no door for one).  [`parse`](Self::parse) is
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

    /// `prefix` plus sixteen random alphanumerics: a name ral invents.
    /// `prefix` is parsed, so a caller cannot smuggle a separator in it.
    ///
    /// # Errors
    /// [`NotAnEntryName`] where the name made is not one.
    pub(crate) fn fresh(prefix: &str) -> Result<Self, NotAnEntryName> {
        let mut name = String::from(prefix);
        name.extend((0..16).map(|_| fastrand::alphanumeric()));
        Self::parse(OsStr::new(&name))
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
#[allow(
    clippy::disallowed_methods,
    reason = "path-form: lifts a name the wire or the walk holds for a lexical split; nothing is resolved"
)]
fn lexical(raw: &OsStr) -> &Path {
    Path::new(raw)
}

/// One component of a spelling past its root.
enum Name {
    Entry(EntryName),
    Parent,
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
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => root.push(component),
            Component::CurDir => {}
            Component::ParentDir => names.push_back(Name::Parent),
            Component::Normal(name) => names.push_back(Name::Entry(EntryName::parse(name)?)),
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
                Name::Entry(name) => Ok(name),
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
    #[cfg_attr(
        not(any(target_os = "linux", windows, test)),
        expect(dead_code, reason = "no macOS renderer holds a position")
    )]
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
    /// directory to descend from: `None` also where it is not one.
    ///
    /// # Errors
    /// The walk's.
    #[cfg_attr(
        not(any(target_os = "linux", windows, test)),
        expect(dead_code, reason = "no macOS renderer holds a position")
    )]
    pub(crate) fn open_dir_nofollow(
        &self,
        token: &InstallToken,
    ) -> io::Result<Option<Dir<Installing>>> {
        Ok(self
            .open_nofollow(token)?
            .and_then(|located| located.into_dir().ok()))
    }

    /// A rendered object: already the frozen real path, so no lookup.
    #[cfg(target_os = "linux")]
    pub(crate) fn rendered(object: &super::Rendered) -> Self {
        Self(object.as_str().into())
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
    #[allow(
        clippy::disallowed_methods,
        reason = "native: a final component, refused where it is not UTF-8"
    )]
    pub(crate) fn name(&self) -> Option<&str> {
        self.0.file_name()?.to_str()
    }

    /// The names of this position past `root`, for a key or a shown name;
    /// `None` where `root` does not contain it.
    pub fn relative_to(&self, root: &Self) -> Option<Vec<EntryName>> {
        let spellings = |p: &Self| {
            std::iter::once(p.0.clone())
                .chain(super::canon::firmlink_toggle(&p.0))
                .collect::<Vec<_>>()
        };
        root.contains(self).then_some(())?;
        let (own, base) = (spellings(self), spellings(root));
        let rest = own
            .iter()
            .find_map(|p| base.iter().find_map(|q| p.strip_prefix(q).ok()))?;
        rest.components()
            .map(|c| EntryName::parse(c.as_os_str()).ok())
            .collect()
    }
}

impl RealPath {
    /// The position above this one, `None` at a root.
    #[cfg(any(target_os = "macos", windows))]
    pub(crate) fn parent(&self) -> Option<Self> {
        self.0.parent().map(|p| Self(p.to_path_buf()))
    }

    /// This position's proper ancestors at or beneath `prefix`, nearest
    /// first: what the Seatbelt profile pins.
    pub(crate) fn ancestors_within<'a>(
        &'a self,
        prefix: &'a Self,
    ) -> impl Iterator<Item = Self> + 'a {
        self.0
            .ancestors()
            .skip(1)
            .map(|a| Self(a.to_path_buf()))
            .take_while(|a| prefix.contains(a))
    }

    /// This position with its prefix `from` replaced by `to`; `None` where
    /// `from` does not contain it.  Where a bind shows a deny (`linux.rs`).
    #[cfg(target_os = "linux")]
    pub(crate) fn rebased(&self, from: &Self, to: &Self) -> Option<Self> {
        let rest = self.relative_to(from)?;
        Some(rest.iter().fold(to.clone(), |at, name| at.child(name)))
    }
}

impl RealPath {
    /// Whether this position is the root of its filesystem: `/`, `C:\`.
    pub fn is_root(&self) -> bool {
        self.0.parent().is_none()
    }

    /// `/`, the position every path lies beneath.  No walk makes it on
    /// Windows, where a root is one drive's.
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
/// the components past the last step, lexically folded (a `..` in the tail
/// takes the name before it in the tail, never a step).  `real` is `root`
/// plus the steps' names plus `tail`, computed by the one constructor: a memo
/// of the steps, never a separate observation.  `Deserialize` by hand: each
/// field checks itself, the shape is checked — every step but the last has
/// kind `Dir`, and a non-empty tail follows a `Dir` step or the root — and
/// `real` is recomputed, so a trace read off the wire is one the walk could
/// have made.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize)]
pub struct Trace {
    root: RootName,
    root_id: ObjectId,
    steps: Vec<Step>,
    tail: Vec<EntryName>,
    real: RealPath,
}

impl Trace {
    fn new(root: RootName, root_id: ObjectId, steps: Vec<Step>, tail: Vec<EntryName>) -> Self {
        let real = RealPath::assemble(&root, steps.iter().map(Step::name).chain(&tail));
        Self {
            root,
            root_id,
            steps,
            tail,
            real,
        }
    }

    /// This trace, a directory's, with `more` steps below it and `tail` past.
    fn extended(&self, more: impl IntoIterator<Item = Step>, tail: Vec<EntryName>) -> Self {
        let steps = self.steps.iter().cloned().chain(more).collect();
        Self::new(self.root.clone(), self.root_id, steps, tail)
    }

    pub fn real(&self) -> &RealPath {
        &self.real
    }
}

impl Trace {
    /// What `identity.rs` reads, nothing outside `path/`.
    pub(super) fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub(super) fn tail(&self) -> &[EntryName] {
        &self.tail
    }

    pub(super) fn root_id(&self) -> ObjectId {
        self.root_id
    }
}

impl Trace {
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
}

impl Trace {
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
        let trace = Self::new(wire.root, wire.root_id, wire.steps, wire.tail);
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
    /// without following the leaf, and keep the handle where the walk's trace
    /// has as many steps and the same last identity; else unheld.  The
    /// recorded trace stays whatever the walk found, and a walk error is the
    /// unheld state.
    #[cfg_attr(
        windows,
        allow(
            clippy::unused_self,
            reason = "Windows holds nothing, so nothing is taken again"
        )
    )]
    pub(crate) fn rehold(&mut self) {
        #[cfg(unix)]
        {
            let trace = &self.trace;
            self.handle = finish(
                trace.real().as_path(),
                Mode::NoFollow,
                OnMissing::Stop,
                Links::Splice,
                |_| true,
            )
            .ok()
            .filter(|found| {
                found.trace.steps.len() == trace.steps.len()
                    && found.trace.last_id() == trace.last_id()
            })
            .and_then(|found| found.into_hold().handle);
        }
    }
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
    /// Whether this walk is an exclusion's, which names an object it holds.
    const EXCLUDES: bool;

    /// The spelling walked, as far as the disk goes.
    ///
    /// # Errors
    /// [`walk_lenient`]'s, or [`walk_excluded`]'s.
    fn walk(spelling: &LexicalPath) -> io::Result<Self>;

    /// Where an access to the spelling lands, for the refusals that judge it.
    fn followed(&self) -> &Trace;
}

impl Walked for Trace {
    const EXCLUDES: bool = false;

    fn walk(spelling: &LexicalPath) -> io::Result<Self> {
        walk_lenient(spelling, LeafMode::Follow)
    }

    fn followed(&self) -> &Trace {
        self
    }
}

impl Walked for Excluded {
    const EXCLUDES: bool = true;

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

/// How the walk treats a link anywhere: splice it (every door but two), or
/// refuse the walk (the position is already real, and a link met now is a
/// redirection).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Links {
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
/// object that is there; a last name that is absent is a create target), or
/// stop and keep the names left lexically.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnMissing {
    Fail,
    Stop,
}

/// How a child is opened where a host shares by handle: a directory step
/// holds the directory itself in place, a leaf step blocks no one.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Sharing {
    Directory,
    Leaf,
}

/// What a data open is for.  Private: the public methods are its values.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Access {
    Read,
    Append,
    Write,
}

/// A directory the walk holds: its handle, and what the host recorded of it
/// as it opened it (`host::DirInfo`: Windows keeps the normalized path and
/// volume the next step checks its child against, Unix nothing).
struct Held {
    file: File,
    info: host::DirInfo,
}

impl Held {
    fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
            info: self.info.clone(),
        })
    }
}

/// What [`host::step`] returns: the handle opened, what it is, and the
/// host's record of it, which it keeps if it is a directory.  `handle` is
/// `None` only where the host refused the hold for permission (macOS, a leaf
/// the user cannot open), which is then [`Leaf::Stated`]: a link or a
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
            file,
            info: self.info,
        };
        Ok((held, self.step, self.stat))
    }
}

/// One object the walk opened, offered to a vet before the walk relies on it:
/// its step, the handle just opened, and the directory handle and name it was
/// opened under (`name` is `.` and `dir` the object itself for the root,
/// whose `step` is `None` since it is no entry).  `child` is `None` for a
/// leaf the host refused to hold, which a vet that needs the object refuses.
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

/// An open directory: its handle, its trace, and its stat taken once at the
/// open.  `A` says who vouched for it.
#[must_use]
pub struct Dir<A: Authority = Unjudged> {
    handle: Held,
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

    /// This directory, vouched for: the one door from `Unjudged` to
    /// `Judged<O>`, through the token only `capability/fs.rs` can make.
    pub(crate) fn judged<O: Op>(self, _: Verdict<O>) -> Dir<Judged<O>> {
        self.marked()
    }

    /// The same directory under another marker: a door of this file's.
    fn marked<B: Authority>(self) -> Dir<B> {
        Dir {
            handle: self.handle,
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
        self.handle.file.as_fd()
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
        host::reopen_shared(&self.handle.file, access)
    }

    /// Whether the directory is on a local fixed volume.
    ///
    /// # Errors
    /// The volume query's.
    #[expect(dead_code, reason = "used from step 7")]
    pub(crate) fn is_local(&self) -> io::Result<bool> {
        host::is_local(&self.handle.file)
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
            lister: host::list(&self.handle.file)?,
        })
    }

    /// Open the entry `name` as a directory: one step with directory
    /// sharing.  A link or a non-directory at `name` is an error.  The one
    /// child-descent primitive, keeping its authority or losing it as
    /// [`Lists::Child`] says.
    ///
    /// # Errors
    /// The open's; `NotADirectory` for a link or anything else but a directory.
    pub(crate) fn open_dir(&self, name: &EntryName) -> io::Result<Dir<A::Child>> {
        let child = step_into(&self.handle, name, Sharing::Directory)?;
        match child.step.kind {
            Kind::Dir => {
                let (handle, step, stat) = child.held()?;
                Ok(Dir {
                    handle,
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
    /// `mode` deciding a link.
    ///
    /// # Errors
    /// The walk's; `NotFound` where there is no entry `name`.
    pub(crate) fn locate(&self, name: &EntryName, mode: LeafMode) -> io::Result<Located<A::Child>> {
        let names = VecDeque::from([Name::Entry(name.clone())]);
        let walker = Walker::new(
            Cursor::beneath(self, &[])?,
            names,
            mode.into(),
            OnMissing::Fail,
            Links::Splice,
            |_| true,
        );
        let located = walker.run()?;
        match located.leaf {
            Leaf::Absent => Err(io::ErrorKind::NotFound.into()),
            _ => Ok(located.marked()),
        }
    }

    /// Follow the link entry `name` to the directory it names: the link is
    /// opened as a step, its target read through that handle, and the walk
    /// continued as a spliced link is — a relative target from this
    /// directory's own handle, an absolute one from the root; never from
    /// [`real`](Self::real) by name.  `chain` is the directories above this
    /// one that the caller entered, nearest last, which a tree walk keeps for
    /// its cycle check; a `..` pops it, and one that climbs past its first
    /// entry is `NotFound`, since nothing above was walked.  The caller
    /// judges the result as a fresh walk.
    ///
    /// # Errors
    /// The walk's; `InvalidInput` where `name` is not a link.
    pub(crate) fn open_dir_via_link(
        &self,
        name: &EntryName,
        chain: &[&Self],
    ) -> io::Result<Dir<A::Child>> {
        let link = step_into(&self.handle, name, Sharing::Leaf)?;
        let (link, step, _) = link.held()?;
        if step.kind != Kind::Symlink {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{} is not a link", shown(name.as_os_str())),
            ));
        }
        let target = host::read_link(&link.file)?;
        let mut walker = Walker::new(
            Cursor::beneath(self, chain)?,
            VecDeque::new(),
            Mode::Enter,
            OnMissing::Fail,
            Links::Splice,
            |_| true,
        );
        walker.splice(&target)?;
        Ok(walker.run()?.into_dir()?.marked())
    }
}

impl Dir<Judged<Writing>> {
    /// The absent entry `name` of this directory, as a create target: a
    /// [`Located`] whose leaf is absent, whose trace is this directory's plus
    /// a one-name tail, and whose `dir` is a dup of the held handle.  It is
    /// unjudged: a name under a write-judged directory is still a distinct
    /// position, and an exclusion can name an absent one, so the caller
    /// judges it before anything makes it.
    ///
    /// # Errors
    /// The dup's.
    pub(crate) fn absent(&self, name: EntryName) -> io::Result<Located> {
        Ok(Located {
            dir: self.handle.try_clone()?,
            trace: self.trace.extended([], vec![name]),
            leaf: Leaf::Absent,
            _a: PhantomData,
        })
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
/// name after the listing, not opened, and the type says so, so no door that
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

/// An object named exactly: the handle of its directory, its trace, its leaf.
///
/// The leaf is absent exactly when `trace.tail()` is non-empty (only the walk
/// and [`Dir::absent`] make one), and `trace` is the only copy of the leaf's
/// name.  A directory reached as one (`Mode::Enter`)
/// is the leaf too, held as entered; for the root, `dir` is the root itself —
/// its own container, as `/..` says — and every door that would use `dir`
/// refuses a directory leaf first.  `A` says who vouched for it.
#[must_use]
pub struct Located<A: Authority = Unjudged> {
    dir: Held,
    trace: Trace,
    leaf: Leaf,
    _a: PhantomData<A>,
}

/// What the walk has of the leaf: nothing (a create target); the handle, held
/// as the host holds it, and the stat taken through it; or — on macOS alone,
/// where the hold was refused for permission — the identity, kind and stat
/// one lookup by name gave, and no handle.  Host-neutral in the type; only the
/// macOS host makes `Stated`, and nothing opens a `Stated` leaf afterwards.
enum Leaf {
    Absent,
    Held {
        file: File,
        stat: Stat,
        info: host::DirInfo,
    },
    Stated {
        stat: Stat,
    },
}

impl<A: Authority> Located<A> {
    pub fn trace(&self) -> &Trace {
        &self.trace
    }

    pub fn real(&self) -> &RealPath {
        self.trace.real()
    }

    /// This object, vouched for: the one door from `Unjudged` to
    /// `Judged<O>`, through the token only `capability/fs.rs` can make.
    pub(crate) fn judged<O: Op>(self, _: Verdict<O>) -> Located<Judged<O>> {
        self.marked()
    }

    /// The same object under another marker: a door of this file's.
    fn marked<B: Authority>(self) -> Located<B> {
        Located {
            dir: self.dir,
            trace: self.trace,
            leaf: self.leaf,
            _a: PhantomData,
        }
    }

    /// What the walk stated of the leaf, for this file's own use: `None` when
    /// it is absent.
    fn observed(&self) -> Option<&Stat> {
        match &self.leaf {
            Leaf::Absent => None,
            Leaf::Held { stat, .. } | Leaf::Stated { stat, .. } => Some(stat),
        }
    }

    /// The object the walk ended on, as an exclusion holds it: its trace and,
    /// on Unix, the handle the walk opened on the deepest object it reached,
    /// which an absent leaf's is its directory.  A `Stated` leaf has none.
    fn into_hold(self) -> Hold {
        Hold {
            #[cfg(unix)]
            handle: match self.leaf {
                Leaf::Held { file, .. } => Some(file),
                Leaf::Absent => Some(self.dir.file),
                Leaf::Stated { .. } => None,
            }
            .map(Arc::new),
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

    fn into_dir(self) -> io::Result<Dir<A>> {
        match self.leaf {
            Leaf::Held { file, stat, info } if stat.kind == Kind::Dir => Ok(Dir {
                handle: Held { file, info },
                trace: self.trace,
                stat,
                _a: PhantomData,
            }),
            _ => Err(io::ErrorKind::NotADirectory.into()),
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
            Leaf::Absent => Err(io::ErrorKind::NotFound.into()),
            _ => Err(io::ErrorKind::InvalidInput.into()),
        }
    }

    /// `access(2)` against the real uid/gid — not `permissions().readonly()`,
    /// which ignores ownership, so another user's 0644 file would read as
    /// writable.  Follows, as `test -w` does.
    pub(crate) fn is_writable(&self) -> bool {
        host::is_writable(&self.dir.file, self.entry())
    }
}

impl Located<Installing> {
    /// The held leaf, for a bind source; `None` for an absent or stated leaf.
    #[cfg(target_os = "linux")]
    pub(crate) fn leaf_fd(&self) -> Option<BorrowedFd<'_>> {
        match &self.leaf {
            Leaf::Held { file, .. } => Some(file.as_fd()),
            _ => None,
        }
    }

    /// A handle on the leaf with `access`, sharing read, write and delete,
    /// derived from the held one: what DACL stamping needs beyond attributes.
    ///
    /// # Errors
    /// The reopen's; `NotFound` for a leaf that is not held.
    #[cfg(windows)]
    pub(crate) fn reopen(&self, access: u32) -> io::Result<File> {
        match &self.leaf {
            Leaf::Held { file, .. } => host::reopen_shared(file, access),
            _ => Err(io::ErrorKind::NotFound.into()),
        }
    }

    /// Whether the leaf is on a local fixed volume.
    ///
    /// # Errors
    /// The volume query's; `NotFound` for a leaf that is not held.
    #[cfg(windows)]
    pub(crate) fn is_local(&self) -> io::Result<bool> {
        match &self.leaf {
            Leaf::Held { file, .. } => host::is_local(file),
            _ => Err(io::ErrorKind::NotFound.into()),
        }
    }
}

impl<A: Reads> Located<A> {
    /// A data handle on the leaf.  Held: derived from the held handle where
    /// the host can, else opened by name under `dir` and verified.  Stated:
    /// `PermissionDenied`, the walk's; nothing is opened.  Absent: created
    /// exclusively for `Append` and `Write`, `NotFound` for `Read`.
    ///
    /// # Errors
    /// The open's; `IsADirectory` for a directory leaf, `InvalidInput` for a
    /// link, which has no data.
    fn open(&self, access: Access) -> io::Result<File> {
        let name = self.leaf_name();
        match &self.leaf {
            Leaf::Held { stat, .. } if stat.kind == Kind::Dir => {
                Err(io::ErrorKind::IsADirectory.into())
            }
            Leaf::Held { stat, .. } if stat.kind == Kind::Symlink => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "a link has no data",
            )),
            Leaf::Held { file, stat, .. } => {
                let name = name.expect("a leaf that is not a directory has a name");
                host::derive(file, &self.dir.file, name, stat, access)
            }
            Leaf::Stated { .. } => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "the object could not be held, so nothing is opened through it",
            )),
            Leaf::Absent => match (access, name) {
                (Access::Read, _) | (_, None) => Err(io::ErrorKind::NotFound.into()),
                (_, Some(name)) => host::create(&self.dir.file, name, access, Visibility::Public),
            },
        }
    }

    /// # Errors
    /// The open's, including `NotFound`.
    pub fn read(&self) -> io::Result<File> {
        self.open(Access::Read)
    }
}

/// How an absent leaf is created.  `Public` under the process umask, as a
/// shell's `>` or `mkdir` make it; `Private` owner-only (0600/0700), as
/// `mktemp(1)` makes a name the program invented, whatever the umask.
/// Windows inherits the directory's ACL either way.
#[derive(Clone, Copy)]
pub(crate) enum Visibility {
    Public,
    Private,
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
        let created = matches!(self.leaf, Leaf::Absent);
        let file = self.open(access)?;
        let stat = host::seen(&file)?.stat;
        let reader = self
            .leaf_name()
            .filter(|_| stat.kind == Kind::File)
            .and_then(|name| host::derive(&file, &self.dir.file, name, &stat, Access::Read).ok());
        Ok(Streamed {
            file,
            reader,
            trace: self.trace,
            stat,
            created,
        })
    }

    /// Stage an atomic write beside the leaf: a fresh exclusive sibling in
    /// `dir`, read and write, with the leaf's mode copied from its stat
    /// taken at the walk.  Takes `self`: the staged write owns its target
    /// from here, so a sibling can land only over the leaf it was staged
    /// beside.
    ///
    /// # Errors
    /// `IsADirectory` where the leaf is a directory; the create's, other than
    /// `AlreadyExists`, which is retried under another name; the mode's.
    pub(crate) fn stage(self) -> io::Result<StagedWrite> {
        match self.kind() {
            Some(Kind::Dir) => return Err(io::ErrorKind::IsADirectory.into()),
            Some(Kind::Symlink) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a link has no data",
                ));
            }
            _ => {}
        }
        let (file, name) = loop {
            let name = EntryName::fresh(".ral-write-")?;
            match host::create_sibling(&self.dir.file, &name) {
                Ok(file) => break (file, name),
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
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
        if let Some(stat) = staged.target.stat() {
            host::copy_mode(&staged.file, stat)?;
        }
        Ok(staged)
    }
}

impl Located<Judged<Writing>> {
    /// An empty file made exclusively at an absent leaf (`O_CREAT|O_EXCL|
    /// O_NOFOLLOW`), its handle dropped: the name is made and nothing is
    /// written.  Takes `self`: after this the object is the file made.
    ///
    /// # Errors
    /// `AlreadyExists` where the leaf is present; the create's.
    pub(crate) fn create_file(self, visibility: Visibility) -> io::Result<Trace> {
        let (Leaf::Absent, Some(name)) = (&self.leaf, self.leaf_name()) else {
            return Err(io::ErrorKind::AlreadyExists.into());
        };
        host::create(&self.dir.file, name, Access::Write, visibility)?;
        Ok(self.trace)
    }

    /// `mkdir` on an absent leaf, then one step onto the name made: the
    /// directory found there, as entered.  Unjudged: the caller judges it as
    /// it would any directory (not provably fresh).
    ///
    /// # Errors
    /// `AlreadyExists` where the leaf is present; the make's or the step's.
    pub(crate) fn create_dir(self, visibility: Visibility) -> io::Result<Dir> {
        let (Leaf::Absent, Some(name)) = (&self.leaf, self.leaf_name()) else {
            return Err(io::ErrorKind::AlreadyExists.into());
        };
        host::mkdir(&self.dir.file, name, visibility)?;
        let (handle, step, stat) = step_into(&self.dir, name, Sharing::Directory)?.held()?;
        if step.kind != Kind::Dir {
            return Err(io::ErrorKind::NotADirectory.into());
        }
        Ok(Dir {
            handle,
            trace: self.trace.extended([step], Vec::new()),
            stat,
            _a: PhantomData,
        })
    }
}

/// A staged atomic write: the target, the exclusive sibling and its name,
/// owned together until [`publish`](Self::publish) or
/// [`discard`](Self::discard) consumes them, so the two never part and the
/// sibling lands only over the leaf it was staged beside.  `Drop` discards: a
/// staged sibling never outlives the frame that staged it, whatever the
/// frame does.
#[must_use]
pub(crate) struct StagedWrite {
    target: Located<Judged<Writing>>,
    file: File,
    name: EntryName,
    settled: bool,
}

impl StagedWrite {
    /// Written through and synced before [`publish`](Self::publish).
    pub(crate) fn file(&mut self) -> &mut File {
        &mut self.file
    }

    pub(crate) fn target(&self) -> &Located<Judged<Writing>> {
        &self.target
    }

    /// Rename the sibling over the leaf's name.  The target's leaf handle is
    /// dropped first.  Where the host renames by name, as Unix does, the
    /// source is `name` under `dir`: POSIX has no publish-by-handle, so a
    /// process with write permission on the directory can put a substitute at
    /// the sibling's name between the sync and the rename.  On failure the
    /// sibling is unlinked before the error returns, so a failed publish
    /// leaves the target as it was and nothing beside it.  Then the directory
    /// is synced, best effort.  No check follows: what the name holds later
    /// is not this write's to assert.
    ///
    /// # Errors
    /// The rename's.
    pub(crate) fn publish(mut self) -> io::Result<()> {
        self.settled = true;
        self.target.leaf = Leaf::Absent;
        let to = self
            .target
            .leaf_name()
            .expect("a staged write targets a leaf with a name");
        let landed = host::rename_over(&self.target.dir.file, &self.file, &self.name, to);
        if landed.is_err() {
            let _ = host::unlink(&self.target.dir.file, &self.name);
        } else {
            let _ = host::sync_dir(&self.target.dir.file);
        }
        landed
    }

    /// Unlink the sibling by its name under `dir`: the write is abandoned.
    ///
    /// # Errors
    /// The unlink's.
    pub(crate) fn discard(mut self) -> io::Result<()> {
        self.settled = true;
        host::unlink(&self.target.dir.file, &self.name)
    }
}

impl Drop for StagedWrite {
    fn drop(&mut self) {
        if !self.settled {
            let _ = host::unlink(&self.target.dir.file, &self.name);
        }
    }
}

/// A frame of the cursor: a directory handle and the trace to it.
struct Frame {
    handle: Held,
    trace: Trace,
}

/// Where the walk stands: the root, or the directory a walk began beneath
/// (`base`), and a stack of `(handle, step)` for every directory entered
/// since.  The cursor's handles are what every open is relative to, and its
/// steps are the trace as it grows.  A `..` pops the stack — the walk returns
/// to the directory it already holds, never to the kernel's current parent —
/// and at the root stays.  Where the walk began beneath a directory (`chain`
/// is `Some`), a `..` past the stack pops the chain of directories the caller
/// entered, and past that it is `NotFound`, since nothing above was walked.
struct Cursor {
    base: Frame,
    chain: Option<Vec<Frame>>,
    stack: Vec<(Held, Step)>,
}

impl Cursor {
    fn root(root: RootName) -> io::Result<Self> {
        let handle = host::open_root(&root)?;
        let id = ObjectId::of(&handle.file)?;
        Ok(Self {
            base: Frame {
                handle,
                trace: Trace::new(root, id, Vec::new(), Vec::new()),
            },
            chain: None,
            stack: Vec::new(),
        })
    }

    /// A cursor standing in `dir`, with `chain` above it.
    fn beneath<A: Authority>(dir: &Dir<A>, chain: &[&Dir<A>]) -> io::Result<Self> {
        let frame = |d: &Dir<A>| -> io::Result<Frame> {
            Ok(Frame {
                handle: d.handle.try_clone()?,
                trace: d.trace.clone(),
            })
        };
        Ok(Self {
            base: frame(dir)?,
            chain: Some(chain.iter().map(|d| frame(d)).collect::<io::Result<_>>()?),
            stack: Vec::new(),
        })
    }

    fn top(&self) -> &Held {
        self.stack.last().map_or(&self.base.handle, |(h, _)| h)
    }

    fn push(&mut self, handle: Held, step: Step) {
        self.stack.push((handle, step));
    }

    fn up(&mut self) -> io::Result<()> {
        if self.stack.pop().is_none()
            && let Some(chain) = &mut self.chain
        {
            self.base = chain.pop().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    "no directory above this one was walked",
                )
            })?;
        }
        Ok(())
    }

    /// The trace of where the cursor stands, plus a step opened below it and
    /// the names past.
    fn trace(&self, step: Option<Step>, tail: Vec<EntryName>) -> Trace {
        let steps = self.stack.iter().map(|(_, step)| step.clone());
        self.base.trace.extended(steps.chain(step), tail)
    }

    fn into_top(self) -> Held {
        self.stack
            .into_iter()
            .next_back()
            .map_or(self.base.handle, |(h, _)| h)
    }

    /// The directory on top and the one beneath it; the root is its own.
    fn into_pair(self) -> io::Result<(Held, Held)> {
        let Self {
            base, mut stack, ..
        } = self;
        match stack.pop() {
            None => Ok((base.handle.try_clone()?, base.handle)),
            Some((top, _)) => Ok((stack.pop().map_or(base.handle, |(h, _)| h), top)),
        }
    }
}

/// The walk's loop: a cursor, the names still to take, and how to treat the
/// last one.
/// Where a walk's names ran out: on a leaf, in the directory on top of the
/// cursor, or at a name that could not be entered, and why.
enum Ended {
    Leaf(Child),
    Entered,
    Stopped(EntryName, io::Error),
}

struct Walker<V> {
    cursor: Cursor,
    names: VecDeque<Name>,
    mode: Mode,
    missing: OnMissing,
    links: Links,
    vet: V,
    splices: usize,
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

    /// Replace the link just met with its `target`: the names of a relative
    /// one continue from the cursor, those of an absolute one from its root.
    /// Whether the walk must end in a directory is re-asked of the whole
    /// spliced spelling, as the target's trailing separator and what is still
    /// to come say.
    fn splice(&mut self, target: &Path) -> io::Result<()> {
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
            self.cursor = Cursor::root(root)?;
            self.vet_root()?;
        } else if is_absolute(target) {
            rooted(target)?;
        }
        if self.names.is_empty() && names_a_dir(target) {
            self.mode = Mode::Enter;
        }
        for name in names.into_iter().rev() {
            self.names.push_front(name);
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
            let Name::Entry(name) = name else {
                self.cursor.up()?;
                continue;
            };
            let last = self.names.is_empty();
            let entering = !last || self.mode == Mode::Enter;
            let sharing = if entering {
                Sharing::Directory
            } else {
                Sharing::Leaf
            };
            let child = match step_into(self.cursor.top(), &name, sharing) {
                Ok(child) => child,
                Err(e) => return Ok(Ended::Stopped(name, e)),
            };
            self.check(&child, &name)?;
            match child.step.kind {
                _ if child.step.kind == Kind::Symlink && self.links == Links::Refuse => {
                    return Err(loop_error());
                }
                Kind::Symlink if last && self.mode == Mode::NoFollow => {
                    return Ok(Ended::Leaf(child));
                }
                Kind::Symlink => {
                    let (link, ..) = child.held()?;
                    let spliced = host::read_link(&link.file).and_then(|t| self.splice(&t));
                    if let Err(e) = spliced {
                        return Ok(Ended::Stopped(name, e));
                    }
                }
                Kind::Dir if entering => {
                    let (handle, step, _) = child.held()?;
                    self.cursor.push(handle, step);
                }
                Kind::Dir => return Ok(Ended::Leaf(child)),
                _ if entering => {
                    return Ok(Ended::Stopped(name, io::ErrorKind::NotADirectory.into()));
                }
                _ => return Ok(Ended::Leaf(child)),
            }
        }
        Ok(Ended::Entered)
    }

    /// The object the walk ended as, as a [`Located`].
    fn conclude(self, ended: Ended) -> io::Result<Located> {
        match ended {
            Ended::Leaf(child) => Ok(self.leaf(child)),
            Ended::Entered => self.entered(),
            Ended::Stopped(name, error) => self.stopped(name, error),
        }
    }

    /// The walk ended on `child`, the last name's object.
    fn leaf(self, child: Child) -> Located {
        let trace = self.cursor.trace(Some(child.step.clone()), Vec::new());
        let leaf = match child.handle {
            Some(file) => Leaf::Held {
                file,
                stat: child.stat,
                info: child.info,
            },
            None => Leaf::Stated { stat: child.stat },
        };
        Located {
            dir: self.cursor.into_top(),
            trace,
            leaf,
            _a: PhantomData,
        }
    }

    /// The names are spent: the directory on top of the cursor is the object,
    /// held as entered, with the one beneath it as `dir`.
    fn entered(self) -> io::Result<Located> {
        let stat = host::seen(&self.cursor.top().file)?.stat;
        let trace = self.cursor.trace(None, Vec::new());
        let (dir, top) = self.cursor.into_pair()?;
        let Held { file, info } = top;
        Ok(Located {
            dir,
            trace,
            leaf: Leaf::Held { file, stat, info },
            _a: PhantomData,
        })
    }

    /// `name` could not be entered, for `error`.  The lenient walks end their
    /// steps at `NotFound`, `NotADirectory` and `PermissionDenied` and keep
    /// the names left as the tail, folded; the strict ones fail, but for an
    /// absent last name, which is a create target.  Nothing else differs
    /// between the two.
    fn stopped(self, name: EntryName, error: io::Error) -> io::Result<Located> {
        use io::ErrorKind::{NotADirectory, NotFound, PermissionDenied};
        let last = self.names.is_empty();
        let tail = match (self.missing, error.kind()) {
            (OnMissing::Stop, NotFound | NotADirectory | PermissionDenied) => {
                let mut tail = vec![name];
                for name in self.names {
                    match name {
                        Name::Entry(name) => tail.push(name),
                        Name::Parent => {
                            tail.pop();
                        }
                    }
                }
                tail
            }
            (OnMissing::Fail, NotFound) if last && self.mode != Mode::Enter => vec![name],
            _ => return Err(error),
        };
        let trace = self.cursor.trace(None, tail);
        Ok(Located {
            dir: self.cursor.into_top(),
            trace,
            leaf: Leaf::Absent,
            _a: PhantomData,
        })
    }
}

/// The engine behind every door: `rp` walked from its root, `mode` deciding
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
    finish(
        rp.as_path(),
        Mode::Enter,
        OnMissing::Fail,
        Links::Splice,
        |_| true,
    )?
    .into_dir()
}

/// A grant spelling's transcript: as far as the disk goes, the rest lexical.
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
/// leaf is held by the walk's own handle (§2), the link before its target is
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
            let target = match read.and_then(|target| walker.splice(&target)) {
                Ok(()) => walker.run()?,
                Err(e) => walker.stopped(name, e)?,
            };
            Ok(Excluded {
                named: Hold {
                    trace: named,
                    #[cfg(unix)]
                    handle: Some(Arc::new(link.file)),
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
#[cfg_attr(
    windows,
    expect(dead_code, reason = "called by `carriers`, which is Unix-only")
)]
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
