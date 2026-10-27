use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use strum::{IntoStaticStr, VariantArray};

use super::names::{EntryName, Name, RootName, Spelling, lexical, rooted, split};
use crate::label;
use crate::path::identity::windows_stored_components;
use crate::path::site::Silent;
use crate::path::{PathRules, shown};
use crate::site::site;

/// What every name of an object shares and no other object has while the
/// object exists.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
pub struct ObjectId {
    #[cfg(unix)]
    pub(super) dev: u64,
    #[cfg(unix)]
    pub(super) ino: u64,
    #[cfg(windows)]
    pub(super) volume: u64,
    #[cfg(windows)]
    pub(super) id: u128,
}

impl ObjectId {
    /// Whether both objects are on one device or volume.
    pub(crate) fn same_volume(self, other: Self) -> bool {
        #[cfg(unix)]
        return self.dev == other.dev;
        #[cfg(windows)]
        return self.volume == other.volume;
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
/// `kind` is `Symlink` only for a [`LeafMode::NoFollow`](crate::path::LeafMode::NoFollow)
/// walk; a following walk has already spliced the link, so what it stats is
/// the target.  A timestamp the filesystem does not record is `None`, not an
/// epoch: the two are different answers, and only the caller knows how to
/// say so.
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

/// One component the walk opened: its name as the filesystem stores it, its
/// identity and its kind, all read from the handle the walk opened on it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Serialize, Deserialize)]
pub struct Step {
    pub(super) name: EntryName,
    pub(super) id: ObjectId,
    pub(super) kind: Kind,
}

impl Step {
    pub(in crate::path) fn name(&self) -> &EntryName {
        &self.name
    }

    pub(in crate::path) fn id(&self) -> ObjectId {
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
    pub(super) fn child(&self, name: &EntryName) -> Self {
        Self(self.0.join(name.as_os_str()))
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
                let identity = |p: &Self| windows_stored_components(p.0.as_os_str());
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
    pub(super) fn new(
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
    pub(super) fn above(&self) -> Self {
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
    pub(super) fn extended(
        &self,
        more: impl IntoIterator<Item = Step>,
        tail: Vec<EntryName>,
    ) -> Self {
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

    pub(super) fn into_real(self) -> RealPath {
        self.real
    }

    /// What `identity.rs` reads, nothing outside `path/`.
    pub(in crate::path) fn steps(&self) -> &[Step] {
        &self.steps
    }

    pub(in crate::path) fn tail(&self) -> &[EntryName] {
        &self.tail
    }

    /// What the walk passed besides its steps, by position.
    pub(crate) fn passed(&self) -> &[RealPath] {
        &self.passed
    }

    pub(super) fn passed_mut(&mut self) -> &mut Vec<RealPath> {
        &mut self.passed
    }

    pub(in crate::path) fn root_id(&self) -> ObjectId {
        self.root_id
    }

    /// Each opened object's identity, from the root to the last step.
    #[cfg(target_os = "linux")]
    pub(crate) fn spine_ids(&self) -> impl DoubleEndedIterator<Item = ObjectId> + '_ {
        std::iter::once(self.root_id).chain(self.steps.iter().map(Step::id))
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
