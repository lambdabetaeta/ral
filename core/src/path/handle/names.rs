use serde::de::Error as _;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::VecDeque;
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::io;
use std::path::{Component, Path, PathBuf};

use crate::fact::{EntryNameFailure, LexicalPathFailure, PathIoLeaf};
use crate::path::lex::{is_absolute, is_unparsed_head};
use crate::path::site::Silent;
use crate::path::{PathRules, shown};
use crate::site::site;

/// A name refused as an entry name by [`EntryName::parse`].  Shows as the
/// sentence naming the rule.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct NotAnEntryName {
    name: OsString,
    rule: EntryNameFailure,
}

impl NotAnEntryName {
    pub fn name(&self) -> &OsStr {
        &self.name
    }
}

impl From<&NotAnEntryName> for EntryNameFailure {
    fn from(error: &NotAnEntryName) -> Self {
        error.rule
    }
}

impl fmt::Display for NotAnEntryName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = shown(&self.name);
        match self.rule {
            EntryNameFailure::Empty => f.write_str("an entry name cannot be empty"),
            EntryNameFailure::Dots => write!(
                f,
                "{name} is not an entry name: it is a directory's own or its parent's"
            ),
            EntryNameFailure::Separator => write!(
                f,
                "{name} is not an entry name: it holds a path separator, so it names a path"
            ),
            EntryNameFailure::Stream => write!(
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
/// [`Dir::open_dir`](super::held::Dir::open_dir) cannot be handed `../x` or
/// `a/b`.  Crosses the wire as serde's lossless `OsString` form, never through
/// `Path`'s UTF-8-only one.
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
            Some(EntryNameFailure::Empty)
        } else if bytes == b"." || bytes == b".." {
            Some(EntryNameFailure::Dots)
        } else if bytes
            .iter()
            .any(|&b| PathRules::HOST.is_separator(char::from(b)))
        {
            Some(EntryNameFailure::Separator)
        } else {
            (cfg!(windows) && bytes.contains(&b':')).then_some(EntryNameFailure::Stream)
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
    pub fn fresh(prefix: &str) -> Result<Self, NotAnEntryName> {
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
pub struct RootName(pub(super) PathBuf);

impl RootName {
    pub(super) fn parse(raw: &OsStr) -> Result<Self, LexicalPathFailure> {
        let path = lexical(raw);
        rooted_lexical(path)?;
        match split_lexical(path).map_err(|error| LexicalPathFailure::EntryName((&error).into()))? {
            Spelling {
                root: Some(root),
                names,
            } if names.is_empty() => Ok(root),
            _ => Err(LexicalPathFailure::MoreThanRoot),
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
        Self::parse(&OsString::deserialize(d)?).map_err(|cause| D::Error::custom(cause.describe()))
    }
}

/// `raw` as a path, read lexically: no disk, no fold.
pub(super) fn lexical(raw: &OsStr) -> &Path {
    site!(Silent::LexicalLift, Path::new(raw))
}

/// Where an entry name came from: the spelling the walk was asked for, or the
/// target of a link it met.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Origin {
    Spelled,
    Spliced,
}

/// One component of a spelling past its root.  An entry carries its
/// [`Origin`], which a making walk reads: a name a link's target spliced in is
/// never made.
pub(super) enum Name {
    Entry(EntryName, Origin),
    Parent,
}

impl Name {
    /// This name as a link's target put it in the spelling.
    pub(super) fn spliced(self) -> Self {
        match self {
            Self::Entry(name, _) => Self::Entry(name, Origin::Spliced),
            Self::Parent => Self::Parent,
        }
    }
}

/// A spelling split into its root, where it has one, and its names.
pub(super) struct Spelling {
    pub(super) root: Option<RootName>,
    pub(super) names: VecDeque<Name>,
}

/// `path` as its root and the names past it, `.` dropped.
pub(super) fn split(path: &Path) -> io::Result<Spelling> {
    split_lexical(path).map_err(Into::into)
}

pub(super) fn split_lexical(path: &Path) -> Result<Spelling, NotAnEntryName> {
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
pub(super) fn rooted(path: &Path) -> io::Result<()> {
    rooted_lexical(path).map_err(|cause| lexical_error(path.as_os_str(), &cause))
}

pub(super) fn rooted_lexical(path: &Path) -> Result<(), LexicalPathFailure> {
    if !is_absolute(path) {
        return Err(LexicalPathFailure::Relative);
    }
    if is_unparsed_head(path) {
        return Err(LexicalPathFailure::MalformedRoot);
    }
    Ok(())
}

pub(super) fn lexical_error(raw: &OsStr, cause: &LexicalPathFailure) -> io::Error {
    let shown = crate::text::quote_native(raw);
    let leaf = match cause {
        LexicalPathFailure::Relative => PathIoLeaf::RelativeWalk,
        LexicalPathFailure::MalformedRoot => PathIoLeaf::MalformedRoot,
        LexicalPathFailure::MoreThanRoot => PathIoLeaf::MoreThanRoot(shown),
        LexicalPathFailure::EntryName(cause) => {
            PathIoLeaf::EntryName(crate::fact::PathIoLeafEntryName {
                name: shown,
                cause: *cause,
            })
        }
        LexicalPathFailure::ParentInPosition => PathIoLeaf::ParentInPosition(shown),
        LexicalPathFailure::NonCanonicalPosition => PathIoLeaf::NonCanonicalPosition(shown),
        LexicalPathFailure::NotWorkingDirectory(_) => PathIoLeaf::NotDirectory(shown),
    };
    io::Error::new(io::ErrorKind::InvalidInput, leaf)
}
